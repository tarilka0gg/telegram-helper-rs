pub mod analytics;
pub mod repo;
pub mod schema;

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use rusqlite::Connection;

/// Cloneable async handle over one SQLite connection (WAL). Work runs on the blocking pool.
#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Self::from_conn(Connection::open(path)?)
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> anyhow::Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        schema::migrate(&conn)?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    pub async fn call<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let inner = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let guard = inner.lock().map_err(|_| anyhow::anyhow!("db mutex poisoned"))?;
            Ok(f(&guard)?)
        })
        .await?
    }
}
