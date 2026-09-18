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
    })
}

const FACT_COLS: &str = "f.id, f.owner_id, u.username, f.scope, f.text, f.source, f.created_at, f.updated_at, f.version";

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

/// Edit a fact (new version, updated FTS). Only the owner of a private fact
/// or any authenticated user for household facts may edit — matching the
/// visibility rule.
pub async fn update(db: &Db, id: i64, requester_id: i64, new_text: &str) -> Result<Fact, ApiError> {
    let existing = get(db, id, requester_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("fact".into()))?;
    let text = new_text.trim().to_string();
    if text.is_empty() || text.chars().count() > 4000 {
        return Err(ApiError::BadRequest(
            "fact text must be 1-4000 chars".into(),
        ));
    }
    let now = util::now_unix();
    db.exec(
        "UPDATE facts SET text = ?1, updated_at = ?2, version = version + 1 WHERE id = ?3",
        vec![P::T(text), P::I(now), P::I(existing.id)],
    )
    .await?;
    get(db, id, requester_id)
        .await?
        .ok_or_else(|| ApiError::internal_msg("fact vanished"))
}

/// Soft-delete with a tombstone (`deleted_at`) so restoring an older backup
/// cannot silently resurrect what the user removed; retrieval excludes
/// tombstones entirely.
pub async fn delete(db: &Db, id: i64, requester_id: i64) -> Result<(), ApiError> {
    let existing = get(db, id, requester_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("fact".into()))?;
    db.exec(
        "UPDATE facts SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2",
        vec![P::I(util::now_unix()), P::I(existing.id)],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db"), 8).await.unwrap();
        (db, dir)
    }

    async fn user(db: &Db, name: &str) -> i64 {
        db.call({
            let name = name.to_string();
            move |c| {
                c.execute(
                    "INSERT INTO users(username, display_name, password_hash, role, created_at)
                     VALUES(?1, ?1, 'x', 'member', 1)",
                    rusqlite::params![name],
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
        let uid = user(&db, "ann").await;
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
        let f2 = update(&db, f.id, uid, "the spare key moved to the garage")
            .await
            .unwrap();
        assert_eq!(f2.version, 2);
        assert_eq!(search(&db, uid, "garage", 10).await.unwrap().len(), 1);
        assert_eq!(search(&db, uid, "mat", 10).await.unwrap().len(), 0);
        delete(&db, f.id, uid).await.unwrap();
        assert_eq!(list(&db, uid).await.unwrap().len(), 0);
        assert_eq!(search(&db, uid, "garage", 10).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn private_facts_are_isolated_per_owner() {
        let (db, _dir) = db().await;
        let ann = user(&db, "ann").await;
        let bob = user(&db, "bob").await;
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
        assert!(update(&db, ann_private.id, bob, "hacked").await.is_err());
        assert!(delete(&db, ann_private.id, bob).await.is_err());
        // FTS search does not leak it either.
        assert_eq!(search(&db, bob, "diary", 10).await.unwrap().len(), 0);
    }
}
