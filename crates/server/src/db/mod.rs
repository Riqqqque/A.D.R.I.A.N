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
}

impl Db {
    /// Open (creating) the database at `path`, apply pragmas + migrations on
    /// the worker thread, and return the async handle.
    pub async fn open(path: &Path, queue: usize) -> Result<Self, ApiError> {
        let path = path.to_path_buf();
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
        Ok(Self { tx })
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
