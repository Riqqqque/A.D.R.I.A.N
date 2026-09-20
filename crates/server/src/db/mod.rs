//! SQLite access through a single dedicated worker thread behind a bounded
//! channel. The async world never blocks on the connection, and the audio /
//! scheduler paths never touch it directly.
//!
//! Durability: WAL + synchronous=FULL for committed state, foreign keys on,
//! bounded busy timeout. The live database must sit on local storage —
//! SQLite documents correctness limits on network filesystems.

pub mod migrate;

use crate::error::ApiError;
use rusqlite::{Connection, ToSql, params_from_iter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// Typed SQL parameter (keeps the worker API simple and safe).
#[derive(Debug, Clone)]
pub enum P {
    Null,
    I(i64),
    F(f64),
    T(String),
    B(Vec<u8>),
}

impl ToSql for P {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::{ToSqlOutput, Value};
        Ok(match self {
            P::Null => ToSqlOutput::Owned(Value::Null),
            P::I(v) => ToSqlOutput::Owned(Value::Integer(*v)),
            P::F(v) => ToSqlOutput::Owned(Value::Real(*v)),
            P::T(v) => ToSqlOutput::Borrowed(rusqlite::types::ValueRef::Text(v.as_bytes())),
            P::B(v) => ToSqlOutput::Owned(Value::Blob(v.clone())),
        })
    }
}

struct Task {
    f: Box<dyn FnOnce(&mut Connection) + Send>,
}

#[derive(Clone)]
pub struct Db {
    tx: mpsc::Sender<Task>,
    path: Arc<PathBuf>,
}

impl Db {
    /// Open (creating) the database at `path`, apply pragmas + migrations on
    /// the worker thread, and return the async handle.
    pub async fn open(path: &Path, queue: usize) -> Result<Self, ApiError> {
        let path = path.to_path_buf();
        let path_arc = Arc::new(path.clone());
        let (tx, rx) = mpsc::channel::<Task>(queue.max(8));
        let (init_tx, init_rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("adrian-db".into())
            .spawn(move || worker(path, rx, init_tx))
            .map_err(ApiError::internal)?;
        init_rx
            .await
            .map_err(|_| ApiError::internal_msg("db worker failed to start"))?
            .map_err(|e| ApiError::internal_msg(format!("db init: {e}")))?;
        Ok(Self { tx, path: path_arc })
    }

    /// Run `f` on the worker thread with the live connection. Transactions
    /// are opened inside `f` via `conn.transaction()`.
    pub async fn call<F, R>(&self, f: F) -> Result<R, ApiError>
    where
        F: FnOnce(&mut Connection) -> Result<R, ApiError> + Send + 'static,
        R: Send + 'static,
    {
        let (rtx, rrx) = oneshot::channel();
        self.tx
            .send(Task {
                f: Box::new(move |conn: &mut Connection| {
                    let out = f(conn).map(|r| Box::new(r) as Box<dyn std::any::Any + Send>);
                    let _ = rtx.send(out);
                }),
            })
            .await
            .map_err(|_| ApiError::internal_msg("db worker is gone"))?;
        let out = rrx
            .await
            .map_err(|_| ApiError::internal_msg("db worker dropped the task"))?;
        out.map(|b| {
            *b.downcast::<R>()
                .unwrap_or_else(|_| unreachable!("db reply type mismatch"))
        })
    }

    /// Execute a statement returning affected row count.
    pub async fn exec(&self, sql: &str, args: Vec<P>) -> Result<usize, ApiError> {
        let sql = sql.to_owned();
        self.call(move |c| Ok(c.execute(&sql, params_from_iter(args))?))
            .await
    }

    /// Query all rows, mapping each with `f`.
    pub async fn q<T, F>(&self, sql: &str, args: Vec<P>, f: F) -> Result<Vec<T>, ApiError>
    where
        F: Fn(&rusqlite::Row<'_>) -> Result<T, rusqlite::Error> + Send + 'static,
        T: Send + 'static,
    {
        let sql = sql.to_owned();
        self.call(move |c| {
            let mut st = c.prepare(&sql)?;
            let rows = st
                .query_map(params_from_iter(args), f)?
                .collect::<Result<Vec<T>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Query an optional single row.
    pub async fn q1<T, F>(&self, sql: &str, args: Vec<P>, f: F) -> Result<Option<T>, ApiError>
    where
        F: Fn(&rusqlite::Row<'_>) -> Result<T, rusqlite::Error> + Send + 'static,
        T: Send + 'static,
    {
        Ok(self.q(sql, args, f).await?.into_iter().next())
    }

    /// Restore the live database from a backup file produced by
    /// `backup_to`. Runs on the worker thread, so it is serialized against
    /// every other query; the live connection is swapped out under the
    /// running server. The pre-restore file is kept next to the database as
    /// `<db>.pre-restore` for manual rollback.
    pub async fn restore_from(&self, backup: &Path) -> Result<(), ApiError> {
        let backup = backup.to_path_buf();
        let live = self.path.as_ref().clone();
        self.call(move |conn| restore_inner(conn, &live, &backup))
            .await
    }

    /// Consistent online backup into `dest` (SQLite Online Backup API — not
    /// a filesystem copy of a live database).
    pub async fn backup_to(&self, dest: PathBuf) -> Result<(), ApiError> {
        self.call(move |src| {
            let mut dst = Connection::open(&dest)?;
            let backup =
                rusqlite::backup::Backup::new(src, &mut dst).map_err(ApiError::internal)?;
            backup
                .run_to_completion(64, std::time::Duration::from_millis(50), None)
                .map_err(ApiError::internal)?;
            Ok(())
        })
        .await
    }
}

/// Swap the live database file for `backup`, serialized on the worker.
/// Steps: validate the backup, close the live connection, keep a
/// `pre-restore` copy, replace the file, reopen + migrate, and swap the
/// connection in. On mid-restore failure the pre-restore copy is put back.
fn restore_inner(conn: &mut Connection, live: &Path, backup: &Path) -> Result<(), ApiError> {
    // Validate: must be a readable SQLite database that passes a quick
    // integrity probe.
    {
        let src = Connection::open_with_flags(backup, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| {
            ApiError::BadRequest(format!("backup is not a SQLite database: {e}"))
        })?;
        let check: String = src
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(|e| ApiError::BadRequest(format!("backup failed integrity check: {e}")))?;
        if check != "ok" {
            return Err(ApiError::BadRequest(format!(
                "backup failed integrity check: {check}"
            )));
        }
    }

    // Park the live connection so the file can be replaced. If close fails
    // we put the connection back and leave the database untouched.
    let placeholder = Connection::open_in_memory().map_err(ApiError::internal)?;
    let old = std::mem::replace(conn, placeholder);
    if let Err((returned, e)) = old.close() {
        *conn = returned;
        return Err(ApiError::internal_msg(format!(
            "could not close live database: {e}"
        )));
    }
    // Stale WAL/SHM sidecars must not survive the file swap.
    for ext in ["wal", "shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}-{ext}", live.display())));
    }

    let rollback = PathBuf::from(format!("{}.pre-restore", live.display()));
    std::fs::copy(live, &rollback).map_err(|e| {
        reopen_or_die(conn, live);
        ApiError::internal_msg(format!("could not snapshot live db: {e}"))
    })?;

    if let Err(e) = std::fs::copy(backup, live) {
        let _ = std::fs::copy(&rollback, live);
        reopen_or_die(conn, live);
        return Err(ApiError::internal_msg(format!(
            "restore copy failed, rolled back: {e}"
        )));
    }

    let reopened = Connection::open(live)
        .and_then(|c| configure(&c).and_then(|_| migrate::migrate(&c)).map(|_| c));
    match reopened {
        Ok(c) => {
            *conn = c;
            Ok(())
        }
        Err(e) => {
            // Restored file unreadable — put the pre-restore copy back.
            let _ = std::fs::copy(&rollback, live);
            reopen_or_die(conn, live);
            Err(ApiError::internal_msg(format!(
                "restored database failed to open, rolled back: {e}"
            )))
        }
    }
}

/// Best-effort reopen of the live path into  after a failed restore.
fn reopen_or_die(conn: &mut Connection, live: &Path) {
    if let Ok(c) = Connection::open(live) {
        let _ = configure(&c);
        let _ = migrate::migrate(&c);
        *conn = c;
    }
}

fn worker(
    path: PathBuf,
    mut rx: mpsc::Receiver<Task>,
    init: oneshot::Sender<Result<String, String>>,
) {
    let opened = (|| -> Result<(Connection, String), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let conn = Connection::open(&path).map_err(|e| e.to_string())?;
        configure(&conn).map_err(|e| e.to_string())?;
        migrate::migrate(&conn).map_err(|e| e.to_string())?;
        let v: String = conn
            .query_row("SELECT sqlite_version()", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        Ok((conn, v))
    })();
    let mut conn = match opened {
        Ok((conn, v)) => {
            tracing::info!(sqlite_version = %v, path = %path.display(), "database ready");
            let _ = init.send(Ok(v));
            conn
        }
        Err(e) => {
            let _ = init.send(Err(e));
            return;
        }
    };
    while let Some(t) = rx.blocking_recv() {
        (t.f)(&mut conn);
    }
}

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000i64)?;
    Ok(())
}
