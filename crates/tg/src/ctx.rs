use std::sync::Arc;

use anyhow::Result;
use tgh_core::{
    config::Config,
    crypto::Crypto,
    db::{repo, Db},
    llm::{LlmClient, Provider},
    Status,
};

/// Everything the Telegram side needs; one per running process (single owner).
pub struct Ctx {
    pub db: Db,
    pub crypto: Crypto,
    pub cfg: Config,
    pub status: Arc<Status>,
    /// `users.id` of the owner.
    pub user_id: i64,
}

impl Ctx {
    pub async fn new(cfg: Config, db: Db, status: Arc<Status>) -> Result<Arc<Self>> {
        let crypto = Crypto::new(&cfg.encryption_key)?;
        let owner = cfg.owner_telegram_id;
        let user_id = db.call(move |c| repo::ensure_user(c, owner)).await?;
        Ok(Arc::new(Self { db, crypto, cfg, status, user_id }))
    }

    pub async fn settings(&self) -> Result<repo::Settings> {
        let uid = self.user_id;
        self.db.call(move |c| repo::settings(c, uid)).await
    }

    /// LLM client for the owner's chosen provider; `None` when no key is stored yet.
    pub async fn llm(&self) -> Result<Option<LlmClient>> {
        let uid = self.user_id;
        let (provider, key) = self
            .db
            .call(move |c| {
                let s = repo::settings(c, uid)?;
                let key = repo::get_api_key(c, uid, &s.llm_provider)?;
                Ok((s.llm_provider, key))
            })
            .await?;
        let (Some(p), Some(enc)) = (Provider::parse(&provider), key) else { return Ok(None) };
        Ok(Some(LlmClient::new(p, self.crypto.decrypt(&enc)?, self.db.clone())))
    }

    pub async fn event(&self, kind: &str, peer_id: Option<i64>, detail: Option<String>) {
        let kind = kind.to_string();
        if let Err(e) = self.db.call(move |c| repo::log_event(c, &kind, peer_id, detail.as_deref())).await {
            tracing::warn!("cannot log event: {e:#}");
        }
    }
}
