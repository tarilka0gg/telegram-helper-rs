//! Runtime configuration from environment (`.env` is loaded by the binary).

use std::path::PathBuf;

/// Model names as of 2026-05; change when new ones ship.
pub mod llm_defaults {
    pub const OPENAI_CHAT_LIGHT: &str = "gpt-5-mini";
    pub const OPENAI_CHAT_HEAVY: &str = "gpt-5.5";
    pub const OPENAI_EMBED: &str = "text-embedding-3-small";
    pub const GEMINI_CHAT_LIGHT: &str = "gemini-3.5-flash";
    pub const GEMINI_CHAT_FALLBACK: &str = "gemini-3.5-flash-lite";
    pub const GEMINI_CHAT_HEAVY: &str = "gemini-pro-latest";
    pub const GEMINI_EMBED: &str = "text-embedding-004";
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bot_token: String,
    pub owner_telegram_id: i64,
    pub encryption_key: String,
    /// Telegram app credentials from https://my.telegram.org (used by the userbot and the control bot).
    pub api_id: i32,
    pub api_hash: String,
    pub data_dir: PathBuf,
    /// Web UI bind address. Loopback only by default.
    pub web_addr: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let req = |k: &str| std::env::var(k).map_err(|_| anyhow::anyhow!("missing env var {k}"));
        Ok(Self {
            bot_token: req("BOT_TOKEN")?,
            owner_telegram_id: req("OWNER_TELEGRAM_ID")?.parse()?,
            encryption_key: req("ENCRYPTION_KEY")?,
            api_id: req("TG_API_ID")?.parse()?,
            api_hash: req("TG_API_HASH")?,
            data_dir: std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into()).into(),
            web_addr: std::env::var("WEB_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into()),
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("app.db")
    }
}

impl Config {
    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }
}
