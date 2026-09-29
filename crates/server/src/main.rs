mod demo;
mod web;

use std::sync::Arc;

use tgh_core::{config::Config, db::Db, Status};
use tgh_tg::{bot, ctx::Ctx, manager::Manager};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,grammers_mtsender=warn,grammers_client=warn,grammers_session=warn,grammers_mtproto=warn".into()),
        )
        .init();
    if std::env::args().any(|a| a == "--demo") {
        let db = Db::open_in_memory()?;
        demo::seed(&db).await?;
        let addr = std::env::var("WEB_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
        let status = Arc::new(Status::default());
        status.userbot_connected.store(true, std::sync::atomic::Ordering::Relaxed);
        return web::serve(&addr, web::AppState { db, status, mgr: None, user_id: 1, avatars: std::path::PathBuf::from("data/avatars") })
            .await;
    }
    let cfg = Config::from_env()?;
    // `tgh-server key <openai|gemini>`: store an LLM key (read from stdin, never from argv).
    // `tgh-server import-keys <file.env>`: validate and store provider keys found in another project's .env.
    if let Some(pos) = std::env::args().position(|a| a == "import-keys") {
        let path = std::env::args().nth(pos + 1).unwrap_or_default();
        return import_keys(&cfg, &path).await;
    }
    // `tgh-server ask "question"`: one LLM call through the configured chain (smoke test).
    if let Some(pos) = std::env::args().position(|a| a == "ask") {
        let q = std::env::args().skip(pos + 1).collect::<Vec<_>>().join(" ");
        let db = Db::open(&cfg.db_path())?;
        let ctx = Ctx::new(cfg, db.clone(), Arc::new(Status::default())).await?;
        let Some(llm) = ctx.llm().await? else { anyhow::bail!("no LLM key stored") };
        let a = llm.chat("cli_ask", &[tgh_core::llm::ChatMessage::user(q)], false).await?;
        let (prov, model): (String, String) = db
            .call(|c| {
                c.query_row("SELECT provider, model FROM llm_usage WHERE ok = 1 ORDER BY id DESC LIMIT 1", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
            })
            .await?;
        println!("[{prov}/{model}] {}", a.trim());
        return Ok(());
    }
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
    let flush_mgr = mgr.clone();
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = tokio::select! {
        r = bot::run_forever(ctx, mgr) => r,
        r = web => r?,
        _ = sigterm.recv() => {
            tracing::info!("SIGTERM: shutting down");
            Ok(())
        }
    };
    // Save the MTProto session (auth key, update state) before exiting, whichever way we got here.
    flush_mgr.flush().await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    result
}

async fn store_key(cfg: &Config, provider: &str) -> anyhow::Result<()> {
    use tgh_core::{
        crypto::Crypto,
        db::repo,
        llm::{LlmClient, Provider},
    };
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

/// Reads `GEMINI_API_KEY`, `GROQ_API_KEY`, `ZAI_API_KEY`, `OPENAI_API_KEY` from a dotenv-style file.
/// Keys are validated with a real request and stored encrypted; nothing secret is printed.
async fn import_keys(cfg: &Config, path: &str) -> anyhow::Result<()> {
    use tgh_core::{
        crypto::Crypto,
        db::repo,
        llm::{LlmClient, Provider},
    };
    let text = std::fs::read_to_string(path)?;
    let vars: std::collections::HashMap<&str, &str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim(), v.trim().trim_matches('"').trim_matches('\'')))
        .collect();
    let db = Db::open(&cfg.db_path())?;
    let crypto = Crypto::new(&cfg.encryption_key)?;
    let owner = cfg.owner_telegram_id;
    for (env, p) in [
        ("GEMINI_API_KEY", Provider::Gemini),
        ("GROQ_API_KEY", Provider::Groq),
        ("ZAI_API_KEY", Provider::Zai),
        ("OPENAI_API_KEY", Provider::OpenAi),
    ] {
        let Some(key) = vars.get(env).filter(|k| !k.is_empty()) else {
            println!("{:<7} not found in file", p.name());
            continue;
        };
        if !LlmClient::new(p, key.to_string(), db.clone()).validate().await {
            println!("{:<7} REJECTED by the provider (or rate-limited right now) — not stored", p.name());
            continue;
        }
        let (enc, name) = (crypto.encrypt(key), p.name());
        db.call(move |c| {
            let uid = repo::ensure_user(c, owner)?;
            repo::set_api_key(c, uid, name, &enc)
        })
        .await?;
        println!("{:<7} ok — stored (encrypted)", p.name());
    }
    Ok(())
}
