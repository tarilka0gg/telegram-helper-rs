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
            // A panic inside an earlier closure must not brick the database for the rest of the process:
            // SQLite itself is still consistent (the transaction rolled back), so take the guard anyway.
            let guard = inner.lock().unwrap_or_else(|e| e.into_inner());
            Ok(f(&guard)?)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn survives_a_panicking_closure() {
        let db = Db::open_in_memory().unwrap();
        let r = tokio::spawn({
            let db = db.clone();
            async move { db.call::<(), _>(|_| panic!("boom")).await }
        })
        .await;
        assert!(r.is_err() || r.unwrap().is_err());
        // the mutex is poisoned now, yet the DB keeps working
        let n: i64 = db.call(|c| c.query_row("SELECT 41 + 1", [], |r| r.get(0))).await.unwrap();
        assert_eq!(n, 42);
    }
}
