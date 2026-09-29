//! Owns the userbot connection: login (phone -> code -> 2FA), restore on start, logout.

use std::sync::Arc;

use anyhow::{bail, Result};
use grammers_client::{client::PasswordToken, Client, SignInError};
use tgh_core::db::repo;
use tokio::{sync::Mutex, task::JoinHandle};

use crate::{ctx::Ctx, dbsession::DbSession, login::{self, CodeInfo, SignIn}, userbot};

struct Pending {
    conn: userbot::Connected,
    session: Arc<DbSession>,
    phone: String,
    info: Option<CodeInfo>,
    password: Option<PasswordToken>,
}

#[derive(Default)]
struct Inner {
    client: Option<Client>,
    task: Option<JoinHandle<()>>,
    persister: Option<JoinHandle<()>>,
    pending: Option<Pending>,
}

pub enum CodeResult {
    LoggedIn(String),
    PasswordRequired(Option<String>),
    InvalidCode,
}

pub struct Manager {
    ctx: Arc<Ctx>,
    inner: Mutex<Inner>,
}

impl Manager {
    pub fn new(ctx: Arc<Ctx>) -> Arc<Self> {
        Arc::new(Self { ctx, inner: Mutex::new(Inner::default()) })
    }

    pub async fn client(&self) -> Option<Client> {
        self.inner.lock().await.client.clone()
    }

    pub async fn is_logged_in(&self) -> bool {
        self.inner.lock().await.client.is_some()
    }

    /// Reconnects a previously saved session, if there is one and it is still valid.
    pub async fn restore(self: &Arc<Self>) -> Result<bool> {
        let uid = self.ctx.user_id;
        let Some((_, _, blob)) = self.ctx.db.call(move |c| repo::load_session(c, uid)).await? else { return Ok(false) };
        let session = match self.ctx.crypto.decrypt(&blob).map_err(anyhow::Error::from).and_then(|j| DbSession::from_json(&j)) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("saved userbot session is unreadable ({e:#}); /login again");
                return Ok(false);
            }
        };
        let conn = userbot::connect(session.clone(), self.ctx.cfg.api_id);
        if !conn.client.is_authorized().await.unwrap_or(false) {
            conn.handle.quit();
            tracing::warn!("saved userbot session is no longer authorized");
            return Ok(false);
        }
        self.start(conn, session).await;
        Ok(true)
    }

    async fn start(self: &Arc<Self>, conn: userbot::Connected, session: Arc<DbSession>) {
        let client = conn.client.clone();
        let ctx = self.ctx.clone();
        let this = self.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = userbot::sync_dialogs(&ctx, &conn.client).await {
                tracing::warn!("initial dialog sync failed: {e:#}");
            }
            if let Err(e) = userbot::run_updates(ctx, conn).await {
                tracing::error!("userbot stopped: {e:#}");
            }
            this.inner.lock().await.client = None;
        });
        let persister = self.spawn_persister(session);
        let mut g = self.inner.lock().await;
        g.client = Some(client);
        g.task = Some(task);
        if let Some(old) = g.persister.replace(persister) {
            old.abort();
        }
    }

    /// Writes the session to the DB (encrypted) shortly after it changes.
    fn spawn_persister(&self, session: Arc<DbSession>) -> JoinHandle<()> {
        let ctx = self.ctx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if session.take_dirty() {
                    let (uid, blob) = (ctx.user_id, ctx.crypto.encrypt(&session.to_json()));
                    if let Err(e) = ctx.db.call(move |c| repo::update_session_blob(c, uid, &blob)).await {
                        tracing::warn!("cannot persist session: {e:#}");
                    }
                }
            }
        })
    }

    /// Requests a login code; returns where Telegram says it delivered it.
    pub async fn begin_login(&self, phone: &str) -> Result<CodeInfo> {
        let mut g = self.inner.lock().await;
        if let Some(old) = g.pending.take() {
            old.conn.handle.quit();
        }
        let session = DbSession::new();
        let conn = userbot::connect(session.clone(), self.ctx.cfg.api_id);
        let info = match login::send_code(&conn.client, &conn.handle, &session, phone, self.ctx.cfg.api_id, &self.ctx.cfg.api_hash).await {
            Ok(i) => i,
            Err(e) => {
                conn.handle.quit();
                return Err(e);
            }
        };
        tracing::info!("login code requested, delivery: {}", info.via);
        g.pending = Some(Pending { conn, session, phone: phone.to_string(), info: Some(info.clone()), password: None });
        Ok(info)
    }

    pub async fn resend_code(&self) -> Result<CodeInfo> {
        let mut g = self.inner.lock().await;
        let Some(p) = g.pending.as_mut() else { bail!("no login in progress; send /login first") };
        let Some(info) = p.info.as_ref() else { bail!("nothing to resend") };
        let new = login::resend_code(&p.conn.client, info).await?;
        tracing::info!("login code resent, delivery: {}", new.via);
        p.info = Some(new.clone());
        Ok(new)
    }

    pub async fn submit_code(self: &Arc<Self>, code: &str) -> Result<CodeResult> {
        let mut g = self.inner.lock().await;
        let Some(p) = g.pending.as_mut() else { bail!("no login in progress; send /login first") };
        let Some(info) = p.info.clone() else { bail!("code was already used") };
        match login::sign_in(&p.conn.client, &p.session, &info, code).await? {
            SignIn::Done(user) => {
                let label = user.full_name();
                drop(g);
                self.finish_login(label.clone()).await?;
                Ok(CodeResult::LoggedIn(label))
            }
            SignIn::Password(pt) => {
                let hint = pt.hint().map(str::to_string);
                p.password = Some(pt);
                p.info = None;
                Ok(CodeResult::PasswordRequired(hint))
            }
            SignIn::InvalidCode => Ok(CodeResult::InvalidCode),
            SignIn::SignUpRequired => bail!("для цього номера немає акаунта Telegram — спершу зареєструйся в офіційному застосунку"),
        }
    }

    /// Returns the account label on success, `None` when the password was wrong (can retry).
    pub async fn submit_password(self: &Arc<Self>, password: &str) -> Result<Option<String>> {
        let mut g = self.inner.lock().await;
        let Some(p) = g.pending.as_mut() else { bail!("no login in progress") };
        let Some(pt) = p.password.take() else { bail!("no password requested") };
        match p.conn.client.check_password(pt, password.as_bytes()).await {
            Ok(user) => {
                let label = user.full_name();
                drop(g);
                self.finish_login(label.clone()).await?;
                Ok(Some(label))
            }
            Err(SignInError::InvalidPassword(pt)) => {
                p.password = Some(pt);
                Ok(None)
            }
            Err(e) => bail!("{e}"),
        }
    }

    async fn finish_login(self: &Arc<Self>, label: String) -> Result<()> {
        let pending = self.inner.lock().await.pending.take().ok_or_else(|| anyhow::anyhow!("login state lost"))?;
        let (uid, api_id) = (self.ctx.user_id, self.ctx.cfg.api_id as i64);
        let hash_enc = self.ctx.crypto.encrypt(&self.ctx.cfg.api_hash);
        let phone = pending.phone.clone();
        let blob = self.ctx.crypto.encrypt(&pending.session.to_json());
        self.ctx.db.call(move |c| repo::save_session(c, uid, api_id, &hash_enc, &blob, &phone, Some(&label))).await?;
        self.ctx.event("login", None, None).await;
        self.start(pending.conn, pending.session).await;
        Ok(())
    }

    pub async fn cancel_login(&self) {
        if let Some(p) = self.inner.lock().await.pending.take() {
            p.conn.handle.quit();
        }
    }

    pub async fn logout(&self) -> Result<()> {
        let (client, task, persister) = {
            let mut g = self.inner.lock().await;
            (g.client.take(), g.task.take(), g.persister.take())
        };
        if let Some(p) = persister {
            p.abort();
        }
        if let Some(c) = client {
            let _ = c.sign_out().await;
            c.disconnect();
        }
        if let Some(t) = task {
            t.abort();
        }
        let uid = self.ctx.user_id;
        self.ctx.db.call(move |c| repo::delete_session(c, uid)).await?;
        self.ctx.status.userbot_connected.store(false, std::sync::atomic::Ordering::Relaxed);
        self.ctx.event("logout", None, None).await;
        Ok(())
    }
}
