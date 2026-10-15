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
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let v = (i + 1) as i64;
        if v > applied {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO schema_migrations(version, applied_at) VALUES(?1,?2)",
                rusqlite::params![v, crate::util::now_unix()],
            )?;
            tx.commit()?;
            tracing::info!(version = v, "applied db migration");
        }
    }
    Ok(())
}
