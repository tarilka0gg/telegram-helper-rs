mod web;

use std::sync::Arc;

use tgh_core::{config::Config, db::Db, Status};
use tgh_tg::{bot, ctx::Ctx, manager::Manager};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,grammers_mtsender=warn,grammers_client=warn".into()))
        .init();
    let cfg = Config::from_env()?;
    let db = Db::open(&cfg.db_path())?;
    let status = Arc::new(Status::default());
    let web_addr = cfg.web_addr.clone();
    let ctx = Ctx::new(cfg, db.clone(), status.clone()).await?;
    let mgr = Manager::new(ctx.clone());

    match mgr.restore().await {
        Ok(true) => tracing::info!("userbot session restored"),
        Ok(false) => tracing::info!("no userbot session yet — send /login to the control bot"),
        Err(e) => tracing::warn!("cannot restore userbot: {e:#}"),
    }

    // Web UI in the background; the control bot keeps the process alive until Ctrl+C.
    let web = tokio::spawn(async move { web::serve(&web_addr, web::AppState { db, status }).await });
    tokio::select! {
        r = bot::run(ctx, mgr) => r?,
        r = web => r??,
    }
    Ok(())
}
