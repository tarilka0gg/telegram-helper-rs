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

    /// LLM chain: the owner's chosen provider first, then every other provider that has a key.
    /// `None` when no key is stored at all.
    pub async fn llm(&self) -> Result<Option<LlmClient>> {
        let uid = self.user_id;
        let (primary, keys) = self
            .db
            .call(move |c| {
                let s = repo::settings(c, uid)?;
                let mut keys = Vec::new();
                for p in Provider::ALL {
                    if let Some(enc) = repo::get_api_key(c, uid, p.name())? {
                        keys.push((p, enc));
                    }
                }
                Ok((s.llm_provider, keys))
            })
            .await?;
        let mut ordered: Vec<(Provider, String)> = Vec::new();
        for (p, enc) in keys {
            let plain = match self.crypto.decrypt(&enc) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!("cannot decrypt {} key: {e}", p.name());
                    continue;
                }
            };
            // the chosen provider goes to the front, the others keep their default order
            if Some(p) == Provider::parse(&primary) { ordered.insert(0, (p, plain)) } else { ordered.push((p, plain)) }
        }
        let mut it = ordered.into_iter();
        let Some((p, k)) = it.next() else { return Ok(None) };
        let mut client = LlmClient::new(p, k, self.db.clone());
        for (p, k) in it {
            client = client.with_fallback(p, k);
        }
        Ok(Some(client))
    }

    pub async fn event(&self, kind: &str, peer_id: Option<i64>, detail: Option<String>) {
        let kind = kind.to_string();
        if let Err(e) = self.db.call(move |c| repo::log_event(c, &kind, peer_id, detail.as_deref())).await {
            tracing::warn!("cannot log event: {e:#}");
        }
    }
}
