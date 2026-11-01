//! Ordered, transactional schema migrations. `schema_migrations` records the
//! applied version; each entry runs inside its own transaction.

use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[
    // v1: identity, sessions, setup
    r#"
    CREATE TABLE users (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        username TEXT NOT NULL UNIQUE COLLATE NOCASE,
        display_name TEXT NOT NULL,
        password_hash TEXT NOT NULL,
        role TEXT NOT NULL DEFAULT 'member'
            CHECK(role IN ('owner','member','guest')),
        created_at INTEGER NOT NULL,
        disabled_at INTEGER
    );
    CREATE TABLE sessions (
        id TEXT PRIMARY KEY,
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        csrf TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        revoked_at INTEGER
    );
    CREATE TABLE setup_state (
        id INTEGER PRIMARY KEY CHECK(id = 1),
        token_hash TEXT NOT NULL,
        used_at INTEGER
    );
    CREATE TABLE settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    -- Server-side only. Values are sealed by crate::secrets (enc:v1:);
    -- rows from older builds are re-sealed at startup.
    CREATE TABLE secrets (
        name TEXT PRIMARY KEY,
        value TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    "#,
    // v2: providers, HA connection, entity registry
    r#"
    CREATE TABLE providers (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL
            CHECK(kind IN ('openai_responses','openai_compatible')),
        label TEXT NOT NULL,
        base_url TEXT NOT NULL,
        model TEXT NOT NULL,
        secret_name TEXT,
        enabled INTEGER NOT NULL DEFAULT 1,
        is_default INTEGER NOT NULL DEFAULT 0,
        extra TEXT NOT NULL DEFAULT '{}',
        created_at INTEGER NOT NULL
    );
    CREATE TABLE ha_connection (
        id INTEGER PRIMARY KEY CHECK(id = 1),
        url TEXT NOT NULL,
        secret_name TEXT NOT NULL,
        enabled INTEGER NOT NULL DEFAULT 1,
        last_ok_at INTEGER,
        last_error TEXT
    );
    CREATE TABLE entities (
        entity_id TEXT PRIMARY KEY,
        area TEXT,
        name TEXT NOT NULL,
        domain TEXT NOT NULL,
        state TEXT NOT NULL DEFAULT '',
        attributes TEXT NOT NULL DEFAULT '{}',
        allowed INTEGER NOT NULL DEFAULT 0,
        updated_at INTEGER NOT NULL
    );
    "#,
    // v3: memory (explicit facts + FTS5), conversations
    r#"
    CREATE TABLE facts (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        owner_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        scope TEXT NOT NULL DEFAULT 'private' CHECK(scope IN ('private','household')),
        text TEXT NOT NULL,
        source TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        deleted_at INTEGER,
        version INTEGER NOT NULL DEFAULT 1
    );
    CREATE VIRTUAL TABLE facts_fts USING fts5(
        text, content='facts', content_rowid='id'
    );
    CREATE TRIGGER facts_ai AFTER INSERT ON facts BEGIN
        INSERT INTO facts_fts(rowid, text) VALUES (new.id, new.text);
    END;
    CREATE TRIGGER facts_au AFTER UPDATE ON facts BEGIN
        INSERT INTO facts_fts(facts_fts, rowid, text)
            VALUES('delete', old.id, old.text);
        INSERT INTO facts_fts(rowid, text) VALUES (new.id, new.text);
    END;
    CREATE TRIGGER facts_ad AFTER DELETE ON facts BEGIN
        INSERT INTO facts_fts(facts_fts, rowid, text)
            VALUES('delete', old.id, old.text);
    END;
    CREATE TABLE conversations (
        id TEXT PRIMARY KEY,
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        title TEXT NOT NULL DEFAULT '',
        created_at INTEGER NOT NULL
    );
    CREATE TABLE messages (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        conversation_id TEXT NOT NULL
            REFERENCES conversations(id) ON DELETE CASCADE,
        role TEXT NOT NULL,
        content TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    "#,
    // v4: audio endpoints, durable alarms + deliveries, action ledger
    r#"
    CREATE TABLE endpoints (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        kind TEXT NOT NULL CHECK(kind IN ('ha_media_player','browser')),
        config TEXT NOT NULL DEFAULT '{}',
        enabled INTEGER NOT NULL DEFAULT 1,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE alarms (
        id TEXT PRIMARY KEY,
        owner_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        label TEXT NOT NULL DEFAULT '',
        kind TEXT NOT NULL DEFAULT 'alarm' CHECK(kind IN ('alarm','timer')),
        original_request TEXT NOT NULL DEFAULT '',
        timezone TEXT NOT NULL,
        local_time TEXT,
        recurrence TEXT NOT NULL DEFAULT 'none'
            CHECK(recurrence IN ('none','daily','weekdays')),
        next_due_utc INTEGER,
        endpoint_id TEXT REFERENCES endpoints(id),
        sound TEXT NOT NULL DEFAULT 'builtin:tone',
        volume REAL NOT NULL DEFAULT 0.7,
        status TEXT NOT NULL DEFAULT 'scheduled'
            CHECK(status IN ('scheduled','ringing','done','canceled')),
        snooze_minutes INTEGER NOT NULL DEFAULT 9,
        schedule_version INTEGER NOT NULL DEFAULT 1,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        deleted_at INTEGER
    );
    CREATE INDEX alarms_due ON alarms(status, next_due_utc);
    CREATE TABLE alarm_deliveries (
        id TEXT PRIMARY KEY,
        alarm_id TEXT NOT NULL,
        occurrence_utc INTEGER NOT NULL,
        execution_id TEXT NOT NULL,
        endpoint_id TEXT,
        status TEXT NOT NULL DEFAULT 'delivering'
            CHECK(status IN ('delivering','played','failed','canceled','missed_late')),
        detail TEXT NOT NULL DEFAULT '',
        started_at INTEGER NOT NULL,
        finished_at INTEGER,
        UNIQUE(alarm_id, occurrence_utc)
    );
    CREATE TABLE action_ledger (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        user_id INTEGER,
        actor TEXT NOT NULL,
        action TEXT NOT NULL,
        target TEXT NOT NULL DEFAULT '',
        params TEXT NOT NULL DEFAULT '{}',
        outcome TEXT NOT NULL,
        detail TEXT NOT NULL DEFAULT '',
        created_at INTEGER NOT NULL
    );
    "#,
    // v5: native satellites — paired devices + one-use pairing codes
    r#"
    CREATE TABLE satellites (
        id TEXT PRIMARY KEY,
        user_id INTEGER NOT NULL REFERENCES users(id),
        name TEXT NOT NULL,
        token_hash TEXT NOT NULL UNIQUE,
        paired_at INTEGER NOT NULL,
        last_seen INTEGER,
        revoked INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX satellites_user ON satellites(user_id);

    CREATE TABLE pairing_codes (
        code TEXT PRIMARY KEY,
        user_id INTEGER NOT NULL REFERENCES users(id),
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        used INTEGER NOT NULL DEFAULT 0
    );
    "#,
    // v6: satellites as alarm endpoints — rebuild endpoints with an
    // extended kind CHECK (SQLite cannot ALTER a CHECK constraint).
    r#"
    CREATE TABLE endpoints_v6 (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        kind TEXT NOT NULL CHECK(kind IN ('ha_media_player','browser','satellite')),
        config TEXT NOT NULL DEFAULT '{}',
        enabled INTEGER NOT NULL DEFAULT 1,
        created_at INTEGER NOT NULL
    );
    INSERT INTO endpoints_v6 SELECT * FROM endpoints;
    DROP TABLE endpoints;
    ALTER TABLE endpoints_v6 RENAME TO endpoints;
    "#,
    // v7: metered provider usage, user-entered prices, monthly budgets.
    // Money is integer micro-USD. cost_micros is NULL when a cloud model has
    // no price row (it cannot be bounded by a budget until one is entered).
    r#"
    CREATE TABLE usage_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        at INTEGER NOT NULL,
        provider_id TEXT NOT NULL,
        provider_label TEXT NOT NULL,
        model TEXT NOT NULL,
        capability TEXT NOT NULL CHECK(capability IN ('chat','stt')),
        module TEXT NOT NULL DEFAULT 'web',
        user_id INTEGER,
        input_tokens INTEGER NOT NULL DEFAULT 0,
        cached_tokens INTEGER NOT NULL DEFAULT 0,
        output_tokens INTEGER NOT NULL DEFAULT 0,
        audio_seconds REAL NOT NULL DEFAULT 0,
        cost_micros INTEGER,
        estimated INTEGER NOT NULL DEFAULT 0,
        local INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX usage_events_at ON usage_events(at);
    CREATE TABLE model_prices (
        provider_id TEXT NOT NULL,
        model TEXT NOT NULL,
        input_per_mtok INTEGER NOT NULL DEFAULT 0,
        cached_per_mtok INTEGER,
        output_per_mtok INTEGER NOT NULL DEFAULT 0,
        audio_per_minute INTEGER NOT NULL DEFAULT 0,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (provider_id, model)
    );
    CREATE TABLE budgets (
        id TEXT PRIMARY KEY,
        scope TEXT NOT NULL CHECK(scope IN ('global','provider','module')),
        scope_id TEXT NOT NULL DEFAULT '',
        monthly_limit_micros INTEGER NOT NULL CHECK(monthly_limit_micros >= 0),
        soft_pct INTEGER NOT NULL DEFAULT 80 CHECK(soft_pct BETWEEN 1 AND 100),
        enabled INTEGER NOT NULL DEFAULT 1,
        created_at INTEGER NOT NULL,
        UNIQUE(scope, scope_id)
    );
    "#,
    // v8: the registry keeps lights only. Nothing else here is usable, and
    // the rest of a home's state is not every login's business. Speakers
    // don't come from this table.
    r#"
    DELETE FROM entities WHERE domain <> 'light';
    "#,
    // v9: fact history. Each edit keeps the text it replaced and who replaced
    // it, and the fact names its last editor. A tombstone keeps no text, so
    // old ones lose theirs here; the facts_au trigger clears their search
    // entries.
    r#"
    CREATE TABLE fact_versions (
        fact_id INTEGER NOT NULL REFERENCES facts(id) ON DELETE CASCADE,
        version INTEGER NOT NULL,
        text TEXT NOT NULL,
        editor_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
        edited_at INTEGER NOT NULL,
        PRIMARY KEY (fact_id, version)
    );
    CREATE INDEX fact_versions_edited_at ON fact_versions(edited_at);
    ALTER TABLE facts ADD COLUMN edited_by INTEGER REFERENCES users(id) ON DELETE SET NULL;
    UPDATE facts SET text = '' WHERE deleted_at IS NOT NULL;
    "#,
];

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL
        );",
    )?;
    let applied: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version),0) FROM schema_migrations",
        [],
        |r| r.get(0),
    )?;
    if applied >= MIGRATIONS.len() as i64 {
        return Ok(());
    }
    // Rebuilding a table that others reference (v6) fails under enforcement:
    // DROP TABLE runs an implicit DELETE. The pragma is ignored inside a
    // transaction, so it brackets the whole run and each commit checks the
    // references itself. `reopen_or_die` keeps this connection even when a
    // migration fails, so enforcement comes back on every path.
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let applied_all = apply_pending(conn, applied);
    let restored = conn.pragma_update(None, "foreign_keys", "ON");
    applied_all.and(restored)
}

fn apply_pending(conn: &Connection, applied: i64) -> rusqlite::Result<()> {
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let v = (i + 1) as i64;
        if v > applied {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO schema_migrations(version, applied_at) VALUES(?1,?2)",
                rusqlite::params![v, crate::util::now_unix()],
            )?;
            check_foreign_keys(&tx)?;
            tx.commit()?;
            tracing::info!(version = v, "applied db migration");
        }
    }
    Ok(())
}

/// Fail if any row points at a parent that does not exist. With enforcement
/// off this is all that stops a migration from committing a broken reference.
fn check_foreign_keys(conn: &Connection) -> rusqlite::Result<()> {
    let mut st = conn.prepare("PRAGMA foreign_key_check")?;
    let mut rows = st.query([])?;
    if let Some(r) = rows.next()? {
        let table: String = r.get(0)?;
        let parent: String = r.get(2)?;
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
            Some(format!(
                "migration left a dangling reference: {table} -> {parent}"
            )),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database as v5 left it, enforcing foreign keys like `configure`.
    fn at_v5() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL
            );",
        )
        .unwrap();
        for (i, sql) in MIGRATIONS[..5].iter().enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations(version, applied_at) VALUES(?1, 1)",
                [i as i64 + 1],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO users(id,username,display_name,password_hash,role,created_at)
             VALUES(1,'o','o','x','owner',1)",
            [],
        )
        .unwrap();
        conn
    }

    fn insert_alarm(conn: &Connection, endpoint_id: &str) {
        conn.execute(
            "INSERT INTO alarms(id,owner_id,timezone,endpoint_id,created_at,updated_at)
             VALUES('a',1,'UTC',?1,1,1)",
            [endpoint_id],
        )
        .unwrap();
    }

    fn version(conn: &Connection) -> i64 {
        conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    fn enforcing_foreign_keys(conn: &Connection) -> bool {
        conn.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
            .unwrap()
            == 1
    }

    #[test]
    fn rebuilding_endpoints_keeps_the_alarms_that_use_them() {
        let conn = at_v5();
        conn.execute(
            "INSERT INTO endpoints(id,name,kind,created_at) VALUES('e','Kitchen','browser',1)",
            [],
        )
        .unwrap();
        insert_alarm(&conn, "e");

        migrate(&conn).unwrap();

        assert_eq!(version(&conn), MIGRATIONS.len() as i64);
        let endpoint: String = conn
            .query_row("SELECT endpoint_id FROM alarms WHERE id='a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(endpoint, "e");
        let dangling: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(dangling, 0);
        assert!(enforcing_foreign_keys(&conn));
        // The rebuilt table is still the alarm's parent.
        let err = conn
            .execute("DELETE FROM endpoints WHERE id='e'", [])
            .unwrap_err();
        assert!(err.to_string().contains("FOREIGN KEY"), "{err}");
    }

    #[test]
    fn the_registry_drops_everything_but_lights() {
        let conn = at_v5();
        for (id, domain) in [("light.desk", "light"), ("lock.front_door", "lock")] {
            conn.execute(
                "INSERT INTO entities(entity_id,name,domain,allowed,updated_at)
                 VALUES(?1,?1,?2,1,1)",
                [id, domain],
            )
            .unwrap();
        }

        migrate(&conn).unwrap();

        let kept: Vec<String> = conn
            .prepare("SELECT entity_id FROM entities")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(kept, ["light.desk"]);
    }

    #[test]
    fn old_tombstones_lose_their_text_and_search_entry() {
        let conn = at_v5();
        conn.execute(
            "INSERT INTO facts(id,owner_id,text,source,created_at,updated_at,deleted_at)
             VALUES(1,1,'the old secret','test',1,1,2)",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let text: String = conn
            .query_row("SELECT text FROM facts WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(text, "");
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM facts_fts WHERE facts_fts MATCH 'secret'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 0);
    }

    /// With enforcement off during a migration, the check before commit is
    /// all that stops a broken reference, and a failed migration must not
    /// leave the connection unenforced (`reopen_or_die` keeps it).
    #[test]
    fn dangling_reference_aborts_the_migration_with_enforcement_back_on() {
        let conn = at_v5();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        insert_alarm(&conn, "gone");
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let err = migrate(&conn).unwrap_err();

        assert!(err.to_string().contains("alarms"), "{err}");
        assert_eq!(version(&conn), 5);
        assert!(enforcing_foreign_keys(&conn));
    }
}
