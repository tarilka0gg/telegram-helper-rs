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
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Same as [`Config::from_env`] over any lookup (lets tests avoid touching the process environment).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        use anyhow::Context;
        let req = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .with_context(|| format!("missing environment variable {k} (see .env.example)"))
        };
        let cfg = Self {
            bot_token: req("BOT_TOKEN")?,
            owner_telegram_id: req("OWNER_TELEGRAM_ID")?.parse().context("OWNER_TELEGRAM_ID must be a number (your Telegram user id)")?,
            encryption_key: req("ENCRYPTION_KEY")?,
            api_id: req("TG_API_ID")?.parse().context("TG_API_ID must be a number (from https://my.telegram.org)")?,
            api_hash: req("TG_API_HASH")?,
            data_dir: get("DATA_DIR").filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "data".into()).into(),
            web_addr: get("WEB_ADDR").filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "127.0.0.1:8787".into()),
        };
        // Fail early and clearly instead of at the first use.
        crate::crypto::Crypto::new(&cfg.encryption_key).context("ENCRYPTION_KEY is not a valid Fernet key (32 url-safe base64 bytes)")?;
        cfg.web_addr
            .parse::<std::net::SocketAddr>()
            .with_context(|| format!("WEB_ADDR {:?} is not a socket address like 127.0.0.1:8787", cfg.web_addr))?;
        if cfg.api_id <= 0 {
            anyhow::bail!("TG_API_ID must be a positive number");
        }
        if !cfg.bot_token.contains(':') {
            anyhow::bail!("BOT_TOKEN does not look like a BotFather token (expected 123456:ABC...)");
        }
        Ok(cfg)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn good() -> HashMap<&'static str, String> {
        HashMap::from([
            ("BOT_TOKEN", "123456:ABC-def".to_string()),
            ("OWNER_TELEGRAM_ID", "123456789".into()),
            ("ENCRYPTION_KEY", fernet::Fernet::generate_key()),
            ("TG_API_ID", "12345".into()),
            ("TG_API_HASH", "0123456789abcdef0123456789abcdef".into()),
        ])
    }

    fn load(m: &HashMap<&'static str, String>) -> anyhow::Result<Config> {
        Config::from_lookup(|k| m.get(k).cloned())
    }

    #[test]
    fn valid_config_and_defaults() {
        let c = load(&good()).unwrap();
        assert_eq!((c.owner_telegram_id, c.api_id, c.web_addr.as_str()), (123456789, 12345, "127.0.0.1:8787"));
        assert_eq!(c.db_path(), std::path::PathBuf::from("data/app.db"));
        // surrounding whitespace/newlines from a hand-edited .env are tolerated
        let mut m = good();
        m.insert("BOT_TOKEN", "  123456:ABC-def \n".into());
        assert_eq!(load(&m).unwrap().bot_token, "123456:ABC-def");
    }

    #[test]
    fn every_bad_setting_names_the_variable() {
        for (var, bad, needle) in [
            ("BOT_TOKEN", "", "BOT_TOKEN"),
            ("BOT_TOKEN", "   ", "BOT_TOKEN"),
            ("BOT_TOKEN", "nocolon", "BotFather"),
            ("OWNER_TELEGRAM_ID", "me", "OWNER_TELEGRAM_ID"),
            ("OWNER_TELEGRAM_ID", "12.5", "OWNER_TELEGRAM_ID"),
            ("TG_API_ID", "abc", "TG_API_ID"),
            ("TG_API_ID", "99999999999999", "TG_API_ID"),
            ("TG_API_ID", "-5", "positive"),
            ("TG_API_ID", "0", "positive"),
            ("TG_API_HASH", "", "TG_API_HASH"),
            ("ENCRYPTION_KEY", "short", "ENCRYPTION_KEY"),
            ("ENCRYPTION_KEY", "", "ENCRYPTION_KEY"),
            ("WEB_ADDR", "localhost", "WEB_ADDR"),
            ("WEB_ADDR", "127.0.0.1:99999", "WEB_ADDR"),
        ] {
            let mut m = good();
            m.insert(var, bad.to_string());
            let e = format!("{:#}", load(&m).unwrap_err());
            assert!(e.contains(needle), "{var}={bad:?} -> {e}");
        }
        // a missing variable is reported by name too
        for var in ["BOT_TOKEN", "OWNER_TELEGRAM_ID", "ENCRYPTION_KEY", "TG_API_ID", "TG_API_HASH"] {
            let mut m = good();
            m.remove(var);
            assert!(format!("{:#}", load(&m).unwrap_err()).contains(var), "{var}");
        }
    }
}
