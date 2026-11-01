//! Durable explicit memory: owner-scoped facts with provenance, editable and
//! deletable, indexed by FTS5. The database is the source of truth — a model
//! context window is a temporary view, not memory.

use crate::db::{Db, P};
use crate::error::ApiError;
use crate::util;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Fact {
    pub id: i64,
    pub owner_id: i64,
    pub owner: String,
    pub scope: String,
    pub text: String,
    pub source: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub version: i64,
    /// Who last changed it, when anyone has: `owner` is only its author.
    pub edited_by: Option<String>,
}

fn map_fact(r: &rusqlite::Row<'_>) -> rusqlite::Result<Fact> {
    Ok(Fact {
        id: r.get(0)?,
        owner_id: r.get(1)?,
        owner: r.get(2)?,
        scope: r.get(3)?,
        text: r.get(4)?,
        source: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
        version: r.get(8)?,
        edited_by: r.get(9)?,
    })
}

const FACT_COLS: &str = "f.id, f.owner_id, u.username, f.scope, f.text, f.source, f.created_at, f.updated_at, f.version,
    (SELECT username FROM users WHERE id = f.edited_by)";

/// One person's live facts, so a script can't grow the database without limit.
const MAX_FACTS_PER_OWNER: i64 = 2000;
/// Earlier texts kept per fact, for the same reason.
const MAX_VERSIONS_PER_FACT: i64 = 20;

/// Visibility rule: a caller sees household facts plus their own private
/// facts. Filtering happens in SQL — unauthorized rows never leave the DB.
fn visibility_clause(requester_id: i64) -> (String, Vec<P>) {
    (
        "(f.scope = 'household' OR f.owner_id = ?)".to_string(),
        vec![P::I(requester_id)],
    )
}

pub async fn add(
    db: &Db,
    owner_id: i64,
    text: &str,
    scope: &str,
    source: &str,
) -> Result<Fact, ApiError> {
    add_capped(db, owner_id, text, scope, source, MAX_FACTS_PER_OWNER).await
}

async fn add_capped(
    db: &Db,
    owner_id: i64,
    text: &str,
    scope: &str,
    source: &str,
    cap: i64,
) -> Result<Fact, ApiError> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > 4000 {
        return Err(ApiError::BadRequest(
            "fact text must be 1-4000 chars".into(),
        ));
    }
    if !matches!(scope, "private" | "household") {
        return Err(ApiError::BadRequest(
            "scope must be private|household".into(),
        ));
    }
    let now = util::now_unix();
    let text = text.to_string();
    let scope = scope.to_string();
    let source = source.to_string();
    let id = db
        .call(move |c| {
            let tx = c.transaction()?;
            let live: i64 = tx.query_row(
                "SELECT COUNT(*) FROM facts WHERE owner_id = ?1 AND deleted_at IS NULL",
                [owner_id],
                |r| r.get(0),
            )?;
            if live >= cap {
                return Err(ApiError::Conflict(format!(
                    "the limit of {cap} saved facts is reached; delete some first"
                )));
            }
            tx.execute(
                "INSERT INTO facts(owner_id,scope,text,source,created_at,updated_at)
                 VALUES(?1,?2,?3,?4,?5,?5)",
                rusqlite::params![owner_id, scope, text, source, now],
            )?;
            let id = tx.last_insert_rowid();
            tx.commit()?;
            Ok(id)
        })
        .await?;
    get(db, id, owner_id)
        .await?
        .ok_or_else(|| ApiError::internal_msg("fact vanished"))
}

pub async fn get(db: &Db, id: i64, requester_id: i64) -> Result<Option<Fact>, ApiError> {
    let (vis, args) = visibility_clause(requester_id);
    let mut all = vec![P::I(id)];
    all.extend(args);
    db.q1(
        &format!(
            "SELECT {FACT_COLS} FROM facts f JOIN users u ON u.id = f.owner_id
             WHERE f.id = ? AND f.deleted_at IS NULL AND {vis}"
        ),
        all,
        map_fact,
    )
    .await
}

pub async fn list(db: &Db, requester_id: i64) -> Result<Vec<Fact>, ApiError> {
    let (vis, args) = visibility_clause(requester_id);
    db.q(
        &format!(
            "SELECT {FACT_COLS} FROM facts f JOIN users u ON u.id = f.owner_id
             WHERE f.deleted_at IS NULL AND {vis} ORDER BY f.updated_at DESC LIMIT 500"
        ),
        args,
        map_fact,
    )
    .await
}

/// FTS5 text search within the caller's visible scope. Terms are quoted so
/// arbitrary user text cannot become an FTS expression.
pub async fn search(
    db: &Db,
    requester_id: i64,
    query: &str,
    limit: i64,
) -> Result<Vec<Fact>, ApiError> {
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.replace('"', " ")))
        .collect();
    if terms.is_empty() {
        return Ok(vec![]);
    }
    let fts_query = terms.join(" OR ");
    let (vis, mut args) = visibility_clause(requester_id);
    let mut all = vec![P::T(fts_query)];
    all.append(&mut args);
    all.push(P::I(limit.clamp(1, 25)));
    db.q(
        &format!(
            "SELECT {FACT_COLS} FROM facts f
             JOIN users u ON u.id = f.owner_id
             JOIN facts_fts ON facts_fts.rowid = f.id
             WHERE facts_fts MATCH ? AND f.deleted_at IS NULL AND {vis}
             ORDER BY rank LIMIT ?"
        ),
        all,
        map_fact,
    )
    .await
}

/// Seeing a fact is not enough to change it: guests read household facts but
/// only change their own, and nobody reaches someone else's private fact
/// because `get` never returns it.
fn ensure_can_change(existing: &Fact, requester_id: i64, role: &str) -> Result<(), ApiError> {
    if existing.owner_id == requester_id
        || role == "owner"
        || (role == "member" && existing.scope == "household")
    {
        Ok(())
    } else {
        Err(ApiError::Forbidden(
            "this fact can only be changed by its author or a household member".into(),
        ))
    }
}

/// Keep a fact's current text as history, attributed to whoever replaces it.
/// Only the latest `MAX_VERSIONS_PER_FACT` stay.
fn keep_version(
    tx: &rusqlite::Transaction<'_>,
    id: i64,
    editor_id: i64,
    now: i64,
) -> Result<(), ApiError> {
    tx.execute(
        "INSERT INTO fact_versions(fact_id,version,text,editor_id,edited_at)
         SELECT id, version, text, ?2, ?3 FROM facts WHERE id = ?1",
        rusqlite::params![id, editor_id, now],
    )?;
    tx.execute(
        "DELETE FROM fact_versions WHERE fact_id = ?1
           AND version <= (SELECT version FROM facts WHERE id = ?1) - ?2",
        rusqlite::params![id, MAX_VERSIONS_PER_FACT],
    )?;
    Ok(())
}

/// Edit a fact (new version, updated FTS), keeping the text it replaces. See
/// `ensure_can_change` for who may.
pub async fn update(
    db: &Db,
    id: i64,
    requester_id: i64,
    requester_role: &str,
    new_text: &str,
) -> Result<Fact, ApiError> {
    let existing = get(db, id, requester_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("fact".into()))?;
    ensure_can_change(&existing, requester_id, requester_role)?;
    let text = new_text.trim().to_string();
    if text.is_empty() || text.chars().count() > 4000 {
        return Err(ApiError::BadRequest(
            "fact text must be 1-4000 chars".into(),
        ));
    }
    let now = util::now_unix();
    let fact_id = existing.id;
    db.call(move |c| {
        let tx = c.transaction()?;
        keep_version(&tx, fact_id, requester_id, now)?;
        tx.execute(
            "UPDATE facts SET text = ?1, updated_at = ?2, version = version + 1, edited_by = ?3
             WHERE id = ?4",
            rusqlite::params![text, now, requester_id, fact_id],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await?;
    get(db, id, requester_id)
        .await?
        .ok_or_else(|| ApiError::internal_msg("fact vanished"))
}

/// How long replaced and deleted text stays recoverable.
const VERSION_KEEP_SECS: i64 = 30 * 24 * 3600;

/// Drop fact history older than `VERSION_KEEP_SECS`.
pub async fn purge_versions(db: &Db) {
    let cutoff = util::now_unix() - VERSION_KEEP_SECS;
    let purged = db
        .exec(
            "DELETE FROM fact_versions WHERE edited_at < ?1",
            vec![P::I(cutoff)],
        )
        .await;
    if let Err(e) = purged {
        tracing::warn!(error = %e, "fact history purge failed");
    }
}

/// Soft-delete with a tombstone (`deleted_at`) so restoring an older backup
/// cannot silently resurrect what the user removed; retrieval excludes
/// tombstones entirely. Blanking the text is what clears its search entry. A
/// household fact is someone else's too, so its text stays recoverable for a
/// while; a private one is its author's to erase, history included.
pub async fn delete(
    db: &Db,
    id: i64,
    requester_id: i64,
    requester_role: &str,
) -> Result<(), ApiError> {
    let existing = get(db, id, requester_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("fact".into()))?;
    ensure_can_change(&existing, requester_id, requester_role)?;
    let now = util::now_unix();
    db.call(move |c| {
        let tx = c.transaction()?;
        if existing.scope == "household" {
            keep_version(&tx, existing.id, requester_id, now)?;
        } else {
            tx.execute(
                "DELETE FROM fact_versions WHERE fact_id = ?1",
                [existing.id],
            )?;
        }
        tx.execute(
            "UPDATE facts SET text = '', deleted_at = ?1, updated_at = ?1,
                version = version + 1, edited_by = ?2
             WHERE id = ?3",
            rusqlite::params![now, requester_id, existing.id],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db"), 8).await.unwrap();
        (db, dir)
    }

    async fn user(db: &Db, name: &str, role: &str) -> i64 {
        db.call({
            let (name, role) = (name.to_string(), role.to_string());
            move |c| {
                c.execute(
                    "INSERT INTO users(username, display_name, password_hash, role, created_at)
                     VALUES(?1, ?1, 'x', ?2, 1)",
                    rusqlite::params![name, role],
                )?;
                Ok(c.last_insert_rowid())
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn add_search_update_delete() {
        let (db, _dir) = db().await;
        let uid = user(&db, "ann", "member").await;
        let f = add(
            &db,
            uid,
            "the spare key is under the mat",
            "private",
            "test",
        )
        .await
        .unwrap();
        assert_eq!(f.version, 1);
        assert_eq!(search(&db, uid, "spare key", 10).await.unwrap().len(), 1);
        let f2 = update(
            &db,
            f.id,
            uid,
            "member",
            "the spare key moved to the garage",
        )
        .await
        .unwrap();
        assert_eq!(f2.version, 2);
        assert_eq!(search(&db, uid, "garage", 10).await.unwrap().len(), 1);
        assert_eq!(search(&db, uid, "mat", 10).await.unwrap().len(), 0);
        delete(&db, f.id, uid, "member").await.unwrap();
        assert_eq!(list(&db, uid).await.unwrap().len(), 0);
        assert_eq!(search(&db, uid, "garage", 10).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn private_facts_are_isolated_per_owner() {
        let (db, _dir) = db().await;
        let ann = user(&db, "ann", "member").await;
        let bob = user(&db, "bob", "member").await;
        add(&db, ann, "ann's diary entry", "private", "test")
            .await
            .unwrap();
        add(&db, ann, "shared wifi password", "household", "test")
            .await
            .unwrap();
        add(&db, bob, "bob's pin code", "private", "test")
            .await
            .unwrap();

        let ann_facts = list(&db, ann).await.unwrap();
        let bob_facts = list(&db, bob).await.unwrap();
        assert_eq!(ann_facts.len(), 2); // own private + household
        assert_eq!(bob_facts.len(), 2); // own private + household
        assert!(!bob_facts.iter().any(|f| f.text.contains("diary")));
        assert!(bob_facts.iter().any(|f| f.text.contains("wifi")));
        // Bob cannot fetch, edit, or delete Ann's private fact by id.
        let ann_private = ann_facts.iter().find(|f| f.text.contains("diary")).unwrap();
        assert!(get(&db, ann_private.id, bob).await.unwrap().is_none());
        assert!(
            update(&db, ann_private.id, bob, "member", "hacked")
                .await
                .is_err()
        );
        assert!(delete(&db, ann_private.id, bob, "member").await.is_err());
        // FTS search does not leak it either.
        assert_eq!(search(&db, bob, "diary", 10).await.unwrap().len(), 0);
    }

    async fn fact_text(db: &Db, id: i64) -> String {
        db.q1(
            "SELECT text FROM facts WHERE id = ?1",
            vec![P::I(id)],
            |r| r.get(0),
        )
        .await
        .unwrap()
        .unwrap()
    }

    /// (version, text, editor_id) rows kept for a fact.
    async fn versions(db: &Db, id: i64) -> Vec<(i64, String, Option<i64>)> {
        db.q(
            "SELECT version, text, editor_id FROM fact_versions WHERE fact_id = ?1 ORDER BY version",
            vec![P::I(id)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_guest_cannot_touch_household_facts() {
        let (db, _dir) = db().await;
        let ann = user(&db, "ann", "owner").await;
        let guest = user(&db, "visitor", "guest").await;
        let f = add(&db, ann, "Mia is allergic to peanuts", "household", "test")
            .await
            .unwrap();

        // They may read it, which is why get() alone can't gate the change.
        assert!(get(&db, f.id, guest).await.unwrap().is_some());
        let edit = update(&db, f.id, guest, "guest", "Mia has no allergies").await;
        assert!(matches!(edit, Err(ApiError::Forbidden(_))), "{edit:?}");
        let gone = delete(&db, f.id, guest, "guest").await;
        assert!(matches!(gone, Err(ApiError::Forbidden(_))), "{gone:?}");

        assert_eq!(fact_text(&db, f.id).await, "Mia is allergic to peanuts");
        assert_eq!(list(&db, ann).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_roles_hold_only_what_they_wrote() {
        let (db, _dir) = db().await;
        let ann = user(&db, "ann", "owner").await;
        let bob = user(&db, "bob", "member").await;
        let f = add(&db, ann, "shared", "household", "test").await.unwrap();
        let own = add(&db, bob, "mine", "household", "test").await.unwrap();
        assert!(update(&db, f.id, bob, "butler", "x").await.is_err());
        assert!(update(&db, own.id, bob, "butler", "y").await.is_ok());
    }

    #[tokio::test]
    async fn a_guest_keeps_control_of_their_own_facts() {
        let (db, _dir) = db().await;
        let guest = user(&db, "visitor", "guest").await;
        let f = add(&db, guest, "my note", "private", "test").await.unwrap();
        assert!(
            update(&db, f.id, guest, "guest", "my new note")
                .await
                .is_ok()
        );
        assert!(delete(&db, f.id, guest, "guest").await.is_ok());
    }

    #[tokio::test]
    async fn an_edit_keeps_the_old_text_and_names_its_editor() {
        let (db, _dir) = db().await;
        let ann = user(&db, "ann", "owner").await;
        let bob = user(&db, "bob", "member").await;
        let f = add(&db, ann, "Mia is allergic to peanuts", "household", "test")
            .await
            .unwrap();

        let edited = update(&db, f.id, bob, "member", "Mia has no allergies")
            .await
            .unwrap();

        assert_eq!(edited.owner, "ann", "the author stays the author");
        assert_eq!(edited.edited_by.as_deref(), Some("bob"));
        assert_eq!(
            versions(&db, f.id).await,
            [(1, "Mia is allergic to peanuts".to_string(), Some(bob))]
        );
    }

    /// The live-fact limit means nothing if rewriting one fact in a loop grows
    /// the history instead.
    #[tokio::test]
    async fn a_fact_keeps_a_bounded_history() {
        let (db, _dir) = db().await;
        let guest = user(&db, "visitor", "guest").await;
        let f = add(&db, guest, "edit 0", "private", "test").await.unwrap();

        for n in 1..=(MAX_VERSIONS_PER_FACT + 5) {
            update(&db, f.id, guest, "guest", &format!("edit {n}"))
                .await
                .unwrap();
        }

        let kept = versions(&db, f.id).await;
        assert_eq!(kept.len() as i64, MAX_VERSIONS_PER_FACT);
        let newest = format!("edit {}", MAX_VERSIONS_PER_FACT + 4);
        assert_eq!(kept.last().unwrap().1, newest);
    }

    #[tokio::test]
    async fn a_deleted_fact_leaves_no_text_in_the_row_or_the_index() {
        let (db, _dir) = db().await;
        let ann = user(&db, "ann", "member").await;
        let f = add(&db, ann, "the code is 1234", "private", "test")
            .await
            .unwrap();
        update(&db, f.id, ann, "member", "the code is 5678")
            .await
            .unwrap();

        delete(&db, f.id, ann, "member").await.unwrap();

        assert_eq!(fact_text(&db, f.id).await, "");
        for word in ["code", "1234", "5678"] {
            let hits: i64 = db
                .q1(
                    "SELECT COUNT(*) FROM facts_fts WHERE facts_fts MATCH ?1",
                    vec![P::T(word.into())],
                    |r| r.get(0),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(hits, 0, "{word}");
        }
        assert_eq!(versions(&db, f.id).await, [], "a private fact is just gone");
    }

    #[tokio::test]
    async fn a_deleted_household_fact_can_be_recovered_for_a_while() {
        let (db, _dir) = db().await;
        let ann = user(&db, "ann", "owner").await;
        let bob = user(&db, "bob", "member").await;
        let f = add(&db, ann, "the bins go out on monday", "household", "test")
            .await
            .unwrap();

        delete(&db, f.id, bob, "member").await.unwrap();

        assert_eq!(
            versions(&db, f.id).await,
            [(1, "the bins go out on monday".to_string(), Some(bob))]
        );
        assert_eq!(fact_text(&db, f.id).await, "");

        // Kept for 30 days, not forever.
        db.exec("UPDATE fact_versions SET edited_at = 1", vec![])
            .await
            .unwrap();
        purge_versions(&db).await;
        assert_eq!(versions(&db, f.id).await, []);
    }

    #[tokio::test]
    async fn each_person_has_a_fact_limit_and_deleting_makes_room() {
        let (db, _dir) = db().await;
        let ann = user(&db, "ann", "member").await;
        let bob = user(&db, "bob", "member").await;
        let first = add_capped(&db, ann, "one", "private", "test", 2)
            .await
            .unwrap();
        add_capped(&db, ann, "two", "private", "test", 2)
            .await
            .unwrap();

        let over = add_capped(&db, ann, "three", "private", "test", 2).await;
        assert!(matches!(over, Err(ApiError::Conflict(_))), "{over:?}");
        add_capped(&db, bob, "one of bob's", "private", "test", 2)
            .await
            .unwrap();

        delete(&db, first.id, ann, "member").await.unwrap();
        add_capped(&db, ann, "three", "private", "test", 2)
            .await
            .unwrap();
    }
}
