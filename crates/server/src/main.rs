mod web;

use std::sync::Arc;

use tgh_core::{config::Config, db::Db, Status};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let cfg = Config::from_env()?;
    let db = Db::open(&cfg.db_path())?;
    let status = Arc::new(Status::default());
    web::serve(&cfg.web_addr, web::AppState { db, status }).await
}
