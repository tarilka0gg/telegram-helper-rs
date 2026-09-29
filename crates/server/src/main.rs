mod demo;
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
    if std::env::args().any(|a| a == "--demo") {
        let db = Db::open_in_memory()?;
        demo::seed(&db).await?;
        let addr = std::env::var("WEB_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
        let status = Arc::new(Status::default());
        status.userbot_connected.store(true, std::sync::atomic::Ordering::Relaxed);
        return web::serve(&addr, web::AppState { db, status, mgr: None, user_id: 1, avatars: std::path::PathBuf::from("data/avatars") }).await;
    }
    let cfg = Config::from_env()?;
    // `tgh-server key <openai|gemini>`: store an LLM key (read from stdin, never from argv).
    if let Some(pos) = std::env::args().position(|a| a == "key") {
        let provider = std::env::args().nth(pos + 1).unwrap_or_default();
        return store_key(&cfg, &provider).await;
    }
    let db = Db::open(&cfg.db_path())?;
    let status = Arc::new(Status::default());
    let web_addr = cfg.web_addr.clone();
    let ctx = Ctx::new(cfg, db.clone(), status.clone()).await?;
    let mgr = Manager::new(ctx.clone());
    let web_mgr = mgr.clone();
    let (user_id, avatars) = (ctx.user_id, ctx.cfg.data_dir.join("avatars"));

    match mgr.restore().await {
        Ok(true) => tracing::info!("userbot session restored"),
        Ok(false) => tracing::info!("no userbot session yet — send /login to the control bot"),
        Err(e) => tracing::warn!("cannot restore userbot: {e:#}"),
    }

    // Web UI in the background; the control bot keeps the process alive until Ctrl+C.
    let web = tokio::spawn(async move { web::serve(&web_addr, web::AppState { db, status, mgr: Some(web_mgr), user_id, avatars }).await });
    tokio::select! {
        r = bot::run(ctx, mgr) => r?,
        r = web => r??,
    }
    Ok(())
}

async fn store_key(cfg: &Config, provider: &str) -> anyhow::Result<()> {
    use tgh_core::{crypto::Crypto, db::repo, llm::{LlmClient, Provider}};
    let Some(p) = Provider::parse(provider) else { anyhow::bail!("usage: echo KEY | tgh-server key <openai|gemini>") };
    let mut key = String::new();
    std::io::stdin().read_line(&mut key)?;
    let key = key.trim().to_string();
    let db = Db::open(&cfg.db_path())?;
    if !LlmClient::new(p, key.clone(), db.clone()).validate().await {
        anyhow::bail!("the provider rejected this key — not stored");
    }
    let enc = Crypto::new(&cfg.encryption_key)?.encrypt(&key);
    let (owner, name) = (cfg.owner_telegram_id, p.name());
    db.call(move |c| {
        let uid = repo::ensure_user(c, owner)?;
        repo::set_api_key(c, uid, name, &enc)?;
        repo::set_setting(c, uid, "llm_provider", name.to_string().into())?;
        Ok(())
    })
    .await?;
    println!("{name} key stored (encrypted); provider set to {name}");
    Ok(())
}
