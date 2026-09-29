//! Control bot (bot account over MTProto). Only the owner is served; everyone else is ignored.

use std::{collections::VecDeque, sync::Arc};

use anyhow::Result;
use grammers_client::{
    client::UpdatesConfiguration,
    message::{Button, InputMessage, ReplyMarkup},
    session::types::PeerRef,
    update::{CallbackQuery, Update},
    Client,
};
use tgh_core::{
    db::repo,
    intent,
    llm::ChatMessage,
    sanitize::sanitize_html,
    AGENT_PROMPT,
};
use tokio::sync::Mutex;

use crate::{ctx::Ctx, features, manager::{CodeResult, Manager}, userbot};

/// Inline keyboard as rows of (label, callback data); built into a `ReplyMarkup` per send attempt.
pub type Kb = Vec<Vec<(String, String)>>;

/// A pending "which one?" question: (action, candidates as (peer_id, name)).
type Pick = (String, Vec<(i64, String)>);

pub(crate) fn btn(label: impl Into<String>, data: impl Into<String>) -> (String, String) {
    (label.into(), data.into())
}

fn to_markup(kb: &Kb) -> ReplyMarkup {
    let rows: Vec<Vec<Button>> = kb.iter().map(|r| r.iter().map(|(l, d)| Button::data(l.clone(), d.clone().into_bytes())).collect()).collect();
    ReplyMarkup::from_buttons(&rows)
}

#[derive(Default, PartialEq)]
enum Conv {
    #[default]
    Idle,
    Phone,
    Code,
    Password,
}

pub struct Bot {
    pub ctx: Arc<Ctx>,
    pub mgr: Arc<Manager>,
    pub client: Client,
    conv: Mutex<Conv>,
    /// Short-term memory of the dialogue ("write him hi" needs the previous contact).
    memory: Mutex<VecDeque<(String, String)>>,
    /// The last "which one?" question: (action, candidates). Lets "both" / "all of them" answer it.
    last_pick: Mutex<Option<Pick>>,
    /// How the bot addresses the owner (with access hash). Learned from the owner's first message, then persisted.
    pub owner_ref: Mutex<Option<PeerRef>>,
}

/// Seconds Telegram asks us to wait, parsed from an error text like "FLOOD_WAIT ... (value: 1361)".
pub(crate) fn flood_wait_secs(msg: &str) -> Option<u64> {
    if !msg.contains("FLOOD_WAIT") {
        return None;
    }
    let tail = msg.split("value:").nth(1)?;
    tail.trim_start().chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// Runs the control bot forever: a failed login (flood wait, network, revoked token) is logged and retried
/// instead of taking the whole server (web UI, userbot) down with it.
pub async fn run_forever(ctx: Arc<Ctx>, mgr: Arc<Manager>) -> Result<()> {
    let mut backoff = std::time::Duration::from_secs(5);
    loop {
        match run(ctx.clone(), mgr.clone()).await {
            Ok(()) => return Ok(()), // orderly shutdown
            Err(e) => {
                let msg = format!("{e:#}");
                let wait = match flood_wait_secs(&msg) {
                    Some(s) => std::time::Duration::from_secs(s + 5),
                    None => {
                        let w = backoff;
                        backoff = (backoff * 2).min(std::time::Duration::from_secs(300));
                        w
                    }
                };
                tracing::error!("control bot stopped: {}; retrying in {}s", msg.chars().take(200).collect::<String>(), wait.as_secs());
                ctx.event("bot_error", None, Some(msg.chars().take(200).collect())).await;
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = tokio::signal::ctrl_c() => return Ok(()),
                }
            }
        }
    }
}

/// The bot's MTProto session is kept (encrypted) in the DB like the userbot's: signing a bot in again on
/// every start is rate-limited by Telegram (FLOOD_WAIT on `auth.importBotAuthorization`).
const BOT_SESSION_KEY: &str = "botsession";

pub async fn run(ctx: Arc<Ctx>, mgr: Arc<Manager>) -> Result<()> {
    let uid = ctx.user_id;
    let stored = ctx.db.call(move |c| repo::get_api_key(c, uid, BOT_SESSION_KEY)).await?;
    let mut session = stored
        .and_then(|enc| ctx.crypto.decrypt(&enc).ok())
        .and_then(|json| crate::dbsession::DbSession::from_json(&json).ok())
        .unwrap_or_else(crate::dbsession::DbSession::new);
    let mut conn = userbot::connect(session.clone(), ctx.cfg.api_id);
    let authorized = matches!(tokio::time::timeout(std::time::Duration::from_secs(30), conn.client.is_authorized()).await, Ok(Ok(true)));
    if !authorized {
        // A stale or foreign session (token replaced, revoked) is dropped: start from a clean one.
        conn.handle.quit();
        session = crate::dbsession::DbSession::new();
        conn = userbot::connect(session.clone(), ctx.cfg.api_id);
        conn.client.bot_sign_in(&ctx.cfg.bot_token, &ctx.cfg.api_hash).await.map_err(|e| anyhow::anyhow!("bot sign-in failed: {e}"))?;
        ctx.event("bot_signed_in", None, None).await;
    }
    save_bot_session(&ctx, &session).await;
    let persister = {
        let (ctx, session) = (ctx.clone(), session.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if session.take_dirty() {
                    save_bot_session(&ctx, &session).await;
                }
            }
        })
    };
    if let Ok(me) = conn.client.get_me().await {
        if let Some(u) = me.username() {
            let _ = ctx.bot_username.set(u.to_string());
        }
    }
    let bot = Arc::new(Bot { ctx: ctx.clone(), mgr, client: conn.client.clone(), conv: Mutex::default(), memory: Mutex::default(), last_pick: Mutex::default(), owner_ref: Mutex::default() });
    let mut stream = conn
        .client
        .stream_updates(conn.updates, UpdatesConfiguration { catch_up: false, update_queue_limit: Some(10_000) })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    bot.load_owner_ref().await;
    crate::scheduler::spawn_all(bot.clone());
    tracing::info!("control bot: listening");
    let result = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break Ok(()),
            upd = stream.next() => {
                let upd = match upd {
                    Ok(u) => u,
                    Err(e) => break Err(anyhow::anyhow!("bot update stream: {e}")),
                };
                let bot = bot.clone();
                tokio::spawn(async move {
                    let r = match upd {
                        Update::NewMessage(m) if !m.outgoing() => bot.on_message(m.into_inner()).await,
                        Update::CallbackQuery(q) => bot.on_callback(q).await,
                        _ => Ok(()),
                    };
                    if let Err(e) = r {
                        tracing::warn!("bot handler error: {e:#}");
                    }
                });
            }
        }
    };
    persister.abort();
    let _ = stream.sync_update_state().await;
    save_bot_session(&ctx, &session).await;
    conn.handle.quit();
    result
}

async fn save_bot_session(ctx: &Arc<Ctx>, session: &crate::dbsession::DbSession) {
    let (uid, blob) = (ctx.user_id, ctx.crypto.encrypt(&session.to_json()));
    if let Err(e) = ctx.db.call(move |c| repo::set_api_key(c, uid, BOT_SESSION_KEY, &blob)).await {
        tracing::warn!("cannot persist bot session: {e:#}");
    }
}

pub(crate) fn split_message(text: &str, max: usize) -> Vec<String> {
    // Telegram rejects blank messages (MESSAGE_EMPTY), so whitespace-only pieces are never emitted.
    fn push(out: &mut Vec<String>, piece: String) {
        if !piece.trim().is_empty() {
            out.push(piece);
        }
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        if cur.chars().count() + line.chars().count() > max && !cur.is_empty() {
            push(&mut out, std::mem::take(&mut cur));
        }
        // A single line longer than `max`: hard-split by characters.
        let mut line = line;
        while line.chars().count() > max {
            let cut = line.char_indices().nth(max).map_or(line.len(), |(i, _)| i);
            push(&mut out, line[..cut].to_string());
            line = &line[cut..];
        }
        cur.push_str(line);
    }
    push(&mut out, cur);
    out
}

impl Bot {
    /// Reports the QR-login outcome back to the owner (and asks for the 2FA password if needed).
    fn watch_qr(self: &Arc<Self>, peer: PeerRef) {
        let bot = self.clone();
        tokio::spawn(async move {
            use crate::manager::QrStatus::*;
            for _ in 0..110 {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                match bot.mgr.qr_status() {
                    Waiting => continue,
                    Done(name) => {
                        let _ = bot.say(peer, &format!("✅ Увійшов як <b>{}</b>. Синхронізую чати…", esc(&name))).await;
                        return;
                    }
                    PasswordNeeded(hint) => {
                        *bot.conv.lock().await = Conv::Password;
                        let hint = hint.map(|h| format!(" (підказка: {})", esc(&h))).unwrap_or_default();
                        let _ = bot.say(peer, &format!("QR прийнято. Потрібен пароль 2FA{hint} — надішли його сюди, повідомлення я видалю.")).await;
                        return;
                    }
                    Failed(e) => {
                        let _ = bot.say(peer, &format!("QR-вхід не вдався: {}", esc(&e))).await;
                        return;
                    }
                    Idle => return,
                }
            }
        });
    }

    pub(crate) async fn say(&self, peer: PeerRef, html: &str) -> Result<()> {
        self.say_with(peer, html, None).await
    }

    pub(crate) async fn say_with(&self, peer: PeerRef, html: &str, markup: Option<Kb>) -> Result<()> {
        let mut chunks = split_message(html, 3800);
        if chunks.is_empty() {
            chunks.push("…(порожня відповідь)".to_string()); // never stay silent
        }
        let last = chunks.len().saturating_sub(1);
        for (i, chunk) in chunks.iter().enumerate() {
            let mut m = InputMessage::new().html(chunk.as_str());
            if i == last {
                if let Some(kb) = &markup {
                    m = m.reply_markup(to_markup(kb));
                }
            }
            if self.client.send_message(peer, m).await.is_err() {
                // Malformed markup from the model must never lose the answer: resend as plain text.
                let mut plain = InputMessage::new().text(strip_html(chunk));
                if i == last {
                    if let Some(kb) = &markup {
                        plain = plain.reply_markup(to_markup(kb));
                    }
                }
                self.client.send_message(peer, plain).await.map_err(|e| anyhow::anyhow!("send: {e}"))?;
            }
        }
        Ok(())
    }

    async fn load_owner_ref(&self) {
        let stored = self.ctx.db.call(|c| c.query_row("SELECT detail FROM events WHERE kind = 'owner_ref' ORDER BY id DESC LIMIT 1", [], |r| r.get::<_, String>(0))).await;
        if let Some(r) = stored.ok().and_then(|j| serde_json::from_str::<PeerRef>(&j).ok()) {
            *self.owner_ref.lock().await = Some(r);
        }
    }

    async fn remember_owner(&self, peer: PeerRef) {
        let mut g = self.owner_ref.lock().await;
        if g.as_ref().map(|p| p.id) == Some(peer.id) && g.is_some() {
            return;
        }
        *g = Some(peer);
        if let Ok(json) = serde_json::to_string(&peer) {
            self.ctx.event("owner_ref", None, Some(json)).await;
        }
    }

    fn is_owner(&self, sender: Option<i64>) -> bool {
        sender == Some(self.ctx.cfg.owner_telegram_id)
    }

    async fn on_message(self: &Arc<Self>, m: grammers_client::message::Message) -> Result<()> {
        let sender = m.sender_id().and_then(|s| s.bare_id());
        let Some(peer) = m.peer_ref().await.map_err(|e| anyhow::anyhow!("{e}"))? else {
            tracing::debug!("bot: message {} has no resolvable peer, ignored", m.id());
            return Ok(());
        };
        if !self.is_owner(sender) || !matches!(m.peer(), Some(grammers_client::peer::Peer::User(_))) {
            // strangers get no reply at all; the log line never contains message text
            tracing::debug!("bot: ignored message from sender {sender:?} (owner is {})", self.ctx.cfg.owner_telegram_id);
            return Ok(());
        }
        tracing::debug!("bot: owner message accepted ({} chars)", m.text().chars().count());
        self.remember_owner(peer).await;
        let text = m.text().trim().to_string();
        if text.is_empty() {
            return Ok(());
        }

        // Login dialogue steps take priority over everything except explicit commands.
        let conv_now = std::mem::take(&mut *self.conv.lock().await);
        if !text.starts_with('/') && conv_now != Conv::Idle {
            return self.login_step(conv_now, &text, peer, &m).await;
        }
        if conv_now != Conv::Idle && !text.starts_with("/resend") {
            self.mgr.cancel_login().await; // a command aborts a half-done login
        }

        if let Some(rest) = text.strip_prefix('/') {
            let (cmd, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let cmd = cmd.split('@').next().unwrap_or(cmd).to_lowercase();
            return self.command(&cmd, arg.trim(), peer, &m).await;
        }
        self.free_text(&text, peer).await
    }

    async fn login_step(&self, step: Conv, text: &str, peer: PeerRef, m: &grammers_client::message::Message) -> Result<()> {
        match step {
            Conv::Phone => {
                let phone = text.trim();
                let digits = phone.chars().filter(char::is_ascii_digit).count();
                if !phone.starts_with('+') || !(7..=15).contains(&digits) {
                    *self.conv.lock().await = Conv::Phone;
                    return self.say(peer, "Формат: <code>+380501234567</code>. Спробуй ще раз або /cancel.").await;
                }
                match self.mgr.begin_login(phone).await {
                    Ok(info) => {
                        *self.conv.lock().await = Conv::Code;
                        let next = info.next.as_ref().map(|n| format!("\nНе прийшов? /resend — надіслати повторно ({}).", esc(n))).unwrap_or_default();
                        self.say(peer, &format!("Код надіслано: <b>{}</b>.\nВведи його <b>з пробілами між цифрами</b> (<code>1 2 3 4 5</code>) — інакше Telegram анулює код, побачивши його відкрито.{next}", esc(&info.via))).await
                    }
                    Err(e) => self.say(peer, &format!("Не вдалося запросити код: {}", esc(&e.to_string()))).await,
                }
            }
            Conv::Code => {
                let _ = m.delete().await; // do not leave the code in the chat
                let code: String = text.chars().filter(char::is_ascii_digit).collect();
                match self.mgr.submit_code(&code).await {
                    Ok(CodeResult::LoggedIn(name)) => self.say(peer, &format!("✅ Увійшов як <b>{}</b>. Синхронізую чати…", esc(&name))).await,
                    Ok(CodeResult::PasswordRequired(hint)) => {
                        *self.conv.lock().await = Conv::Password;
                        let hint = hint.map(|h| format!(" (підказка: {})", esc(&h))).unwrap_or_default();
                        self.say(peer, &format!("Потрібен пароль 2FA{hint}. Повідомлення з паролем я одразу видалю.")).await
                    }
                    Ok(CodeResult::InvalidCode) => {
                        *self.conv.lock().await = Conv::Code;
                        self.say(peer, "Невірний код. Введи ще раз (з пробілами) або /cancel.").await
                    }
                    Err(e) => self.say(peer, &format!("Помилка входу: {}", esc(&e.to_string()))).await,
                }
            }
            Conv::Password => {
                let res = self.mgr.submit_password(text).await;
                let _ = m.delete().await;
                match res {
                    Ok(Some(name)) => self.say(peer, &format!("✅ Увійшов як <b>{}</b>.", esc(&name))).await,
                    Ok(None) => {
                        *self.conv.lock().await = Conv::Password;
                        self.say(peer, "Невірний пароль. Ще раз або /cancel.").await
                    }
                    Err(e) => self.say(peer, &format!("Помилка входу: {}", esc(&e.to_string()))).await,
                }
            }
            Conv::Idle => Ok(()),
        }
    }

    async fn command(self: &Arc<Self>, cmd: &str, arg: &str, peer: PeerRef, m: &grammers_client::message::Message) -> Result<()> {
        match cmd {
            "start" | "help" => self.say(peer, HELP).await,
            "cancel" => {
                self.mgr.cancel_login().await;
                self.say(peer, "Скасовано.").await
            }
            "login" => {
                if self.mgr.is_logged_in().await {
                    return self.say(peer, "Уже підключено. /logout щоб вийти.").await;
                }
                *self.conv.lock().await = Conv::Phone;
                self.say(peer, "Номер телефону акаунта у форматі <code>+380501234567</code>:").await
            }
            "qr" => {
                if self.mgr.is_logged_in().await {
                    return self.say(peer, "Уже підключено. /logout щоб вийти.").await;
                }
                self.mgr.begin_qr().await?;
                self.say(peer, &format!("Відкрий на цьому комп'ютері <b>http://{}/login</b> — там QR-код.\nУ Telegram на телефоні: <i>Налаштування → Пристрої → Підключити пристрій</i> і скануй його. Код не потрібен. Чекаю до 5 хвилин.", self.ctx.cfg.web_addr)).await?;
                self.watch_qr(peer);
                Ok(())
            }
            "resend" => match self.mgr.resend_code().await {
                Ok(info) => {
                    *self.conv.lock().await = Conv::Code;
                    self.say(peer, &format!("Надіслано повторно: <b>{}</b>. Введи код з пробілами.", esc(&info.via))).await
                }
                Err(e) => self.say(peer, &format!("Не вийшло: {}", esc(&e.to_string()))).await,
            },
            "logout" => {
                self.mgr.logout().await?;
                self.say(peer, "Сесію видалено.").await
            }
            "status" => self.status(peer).await,
            "sync" => {
                let Some(c) = self.mgr.client().await else { return self.say(peer, NOT_LOGGED).await };
                let n = userbot::sync_dialogs(&self.ctx, &c).await?;
                self.say(peer, &format!("Оновлено контактів: {n}")).await
            }
            "key" => self.set_key(arg, peer, m).await,
            "set" => self.set_setting(arg, peer).await,
            "settings" => self.show_settings(peer).await,
            "search" => self.search(arg, peer).await,
            "todos" => self.todos(peer).await,
            "chat" => self.chat_pick(arg, "menu", peer).await,
            "catchup" => self.chat_pick(arg, "catchup", peer).await,
            "send" | "digest" | "news" | "topics" | "classify" | "sources" | "index" => self.extra_command(cmd, arg, peer).await,
            _ => self.say(peer, "Невідома команда. /help").await,
        }
    }

    async fn status(&self, peer: PeerRef) -> Result<()> {
        let s = self.ctx.settings().await?;
        let up = self.mgr.is_logged_in().await;
        let uid = self.ctx.user_id;
        let (msgs, contacts): (i64, i64) = self.ctx.db.call(move |c| Ok((
            c.query_row("SELECT count(*) FROM messages WHERE user_id = ?", [uid], |r| r.get(0))?,
            c.query_row("SELECT count(*) FROM contacts WHERE user_id = ?", [uid], |r| r.get(0))?,
        ))).await?;
        let has_key = self.ctx.llm().await?.is_some();
        self.say(peer, &format!(
            "Userbot: {}\nLLM: {} ({})\nАвто-відповідь: {}\nПовідомлень у БД: {msgs}, контактів: {contacts}\nВеб-аналітика: http://{}",
            if up { "🟢 підключено" } else { "🔴 не підключено (/login)" },
            s.llm_provider, if has_key { "ключ є" } else { "ключа немає — /key" },
            if s.auto_reply_enabled { "увімкнена" } else { "вимкнена" },
            self.ctx.cfg.web_addr,
        )).await
    }

    async fn set_key(&self, arg: &str, peer: PeerRef, m: &grammers_client::message::Message) -> Result<()> {
        let _ = m.delete().await; // the key must not stay in the chat history
        let Some((prov, key)) = arg.split_once(char::is_whitespace) else {
            return self.say(peer, "Формат: <code>/key openai sk-…</code> або <code>/key gemini …</code>").await;
        };
        let Some(provider) = tgh_core::llm::Provider::parse(&prov.to_lowercase()) else {
            return self.say(peer, "Провайдер: openai або gemini.").await;
        };
        let key = key.trim();
        let probe = tgh_core::llm::LlmClient::new(provider, key.to_string(), self.ctx.db.clone());
        if !probe.validate().await {
            return self.say(peer, "Ключ не пройшов перевірку — не зберіг.").await;
        }
        let (uid, enc, name) = (self.ctx.user_id, self.ctx.crypto.encrypt(key), provider.name());
        self.ctx.db.call(move |c| {
            repo::set_api_key(c, uid, name, &enc)?;
            repo::set_setting(c, uid, "llm_provider", name.to_string().into())?;
            Ok(())
        }).await?;
        self.say(peer, &format!("✅ Ключ {name} збережено (зашифровано), провайдер активний.")).await
    }

    async fn show_settings(&self, peer: PeerRef) -> Result<()> {
        let s = self.ctx.settings().await?;
        let onoff = |b: bool| if b { "✅" } else { "⬜" };
        self.say(peer, &format!(
            "<b>Налаштування</b> (змінити: <code>/set ключ значення</code> або словами)\n\n\
             {} auto_reply_enabled · режим <code>{}</code> · кулдаун {} хв\n<i>{}</i>\n\
             {} digest_enabled · о <code>{}</code>\n{} news_enabled · о <code>{}</code> · вікно {} год\n\
             {} reminders_enabled · за {} год · прострочені {}\n{} ignore_archived\n\
             {} use_heavy_model · провайдер <code>{}</code> · TZ <code>{}</code>",
            onoff(s.auto_reply_enabled), s.auto_reply_mode, s.auto_reply_cooldown_min, esc(&s.auto_reply_text),
            onoff(s.digest_enabled), s.digest_time, onoff(s.news_enabled), s.news_digest_time, s.news_window_hours,
            onoff(s.reminders_enabled), s.reminder_lead_hours, onoff(s.reminder_overdue_enabled), onoff(s.ignore_archived),
            onoff(s.use_heavy_model), s.llm_provider, s.timezone,
        )).await
    }

    async fn set_setting(&self, arg: &str, peer: PeerRef) -> Result<()> {
        let Some((key, val)) = arg.split_once(char::is_whitespace) else {
            return self.say(peer, "Формат: <code>/set ключ значення</code>, ключі — в /settings").await;
        };
        let reply = self.apply_setting(key, &parse_scalar(val.trim())).await?;
        self.say(peer, &reply).await
    }

    /// Shared by `/set` and the agent. Validates key (whitelist) and value before touching the DB.
    pub(crate) async fn apply_setting(&self, key: &str, value: &serde_json::Value) -> Result<String> {
        let Some(col) = intent::setting_column(key) else { return Ok(format!("Невідоме налаштування <code>{}</code>.", esc(key))) };
        let value = &coerce_bool(col, value);
        if let Err(why) = validate_setting(col, value) {
            return Ok(format!("Недопустиме значення для <code>{col}</code>: {why}"));
        }
        let Some(sql) = intent::json_to_sql(value) else { return Ok("Недопустиме значення.".into()) };
        let uid = self.ctx.user_id;
        self.ctx.db.call(move |c| repo::set_setting(c, uid, col, sql)).await?;
        Ok(format!("✅ <code>{col}</code> = <code>{}</code>", esc(value.to_string().trim_matches('"'))))
    }

    async fn search(&self, q: &str, peer: PeerRef) -> Result<()> {
        if q.is_empty() {
            return self.say(peer, "Формат: <code>/search текст</code>").await;
        }
        let (uid, qq) = (self.ctx.user_id, q.to_string());
        let hits = self.ctx.db.call(move |c| repo::search_messages(c, uid, &qq, 10)).await?;
        if hits.is_empty() {
            return self.say(peer, "Нічого не знайшов.").await;
        }
        let names: std::collections::HashMap<i64, String> = {
            let uid = self.ctx.user_id;
            self.ctx.db.call(move |c| repo::list_contacts(c, uid)).await?.into_iter().map(|k| (k.peer_id, k.display_name)).collect()
        };
        let lines: Vec<String> = hits.iter().map(|h| format!(
            "• <b>{}</b> · {}\n{}", esc(names.get(&h.peer_id).map_or("?", |s| s)), &h.date[..h.date.len().min(16)],
            esc(&h.text.clone().unwrap_or_default().chars().take(200).collect::<String>()))).collect();
        self.say(peer, &lines.join("\n\n")).await
    }

    async fn todos(&self, peer: PeerRef) -> Result<()> {
        let uid = self.ctx.user_id;
        let items = self.ctx.db.call(move |c| repo::open_commitments(c, uid, None)).await?;
        if items.is_empty() {
            return self.say(peer, "Відкритих обіцянок немає.").await;
        }
        for it in items.iter().take(15) {
            let who = if it.direction == "mine" { "я → " } else { "" };
            let text = format!("{}<b>{}</b>: {}\n<i>{}</i>", who, esc(&it.peer_name), esc(&it.text), it.deadline_at.as_deref().map_or("без строку".into(), |d| format!("до {d} UTC")));
            let kb = vec![vec![btn("✅ Готово", format!("done:{}", it.id)), btn("✖ Скасувати", format!("cancel:{}", it.id))]];
            self.say_with(peer, &text, Some(kb)).await?;
        }
        Ok(())
    }

    /// Resolve a name to a contact and run `action` (or show buttons when ambiguous).
    pub(crate) async fn chat_pick(&self, name: &str, action: &str, peer: PeerRef) -> Result<()> {
        if name.is_empty() {
            return self.say(peer, "Вкажи ім'я: <code>/chat Оля</code>").await;
        }
        let found = features::find_contacts(&self.ctx, name).await?;
        match found.as_slice() {
            [] => self.say(peer, "Не знайшов такого контакту. Спробуй /sync.").await,
            [(k, _)] => self.run_chat_action(action, k.peer_id, &k.display_name, peer).await,
            many if many[0].1 >= 90 && many[1].1 + 10 <= many[0].1 => self.run_chat_action(action, many[0].0.peer_id, &many[0].0.display_name, peer).await,
            many => {
                let mut rows: Kb = many.iter().map(|(k, _)| vec![btn(k.display_name.clone(), format!("c:{action}:{}", k.peer_id))]).collect();
                if action != "menu" {
                    let cands: Vec<(i64, String)> = many.iter().map(|(k, _)| (k.peer_id, k.display_name.clone())).collect();
                    let payload = serde_json::json!({"action": action, "peers": cands}).to_string();
                    let uid = self.ctx.user_id;
                    let pid = self.ctx.db.call(move |c| repo::pending_add(c, uid, "pick_all", &payload)).await?;
                    rows.push(vec![btn(format!("🔀 Усі ({})", cands.len()), format!("pa:{pid}"))]);
                    *self.last_pick.lock().await = Some((action.to_string(), cands));
                }
                self.say_with(peer, "Кого саме? Можна вибрати кількох — «🔀 Усі» або напиши «обидва».", Some(rows)).await
            }
        }
    }

    async fn on_callback(&self, q: CallbackQuery) -> Result<()> {
        if !self.is_owner(q.sender_id().bare_id()) {
            return Ok(());
        }
        let data = String::from_utf8_lossy(q.data()).to_string();
        let Some(peer) = q.peer_ref().await.map_err(|e| anyhow::anyhow!("{e}"))? else { return Ok(()) };
        let _ = q.answer().send().await;
        let uid = self.ctx.user_id;
        let parts: Vec<&str> = data.split(':').collect();
        match parts.as_slice() {
            ["done", id] | ["cancel", id] => {
                let (id, status) = (id.parse::<i64>().unwrap_or(0), if parts[0] == "done" { "done" } else { "cancelled" });
                self.ctx.db.call(move |c| repo::set_commitment_status(c, uid, id, status)).await?;
                self.say(peer, if status == "done" { "✅ Готово" } else { "✖ Скасовано" }).await
            }
            ["c", action, pid] => {
                let pid = pid.parse::<i64>().unwrap_or(0);
                let name = self.contact_name(pid).await?;
                self.run_chat_action(action, pid, &name, peer).await
            }
            ["ok", id] => self.confirm_send(id.parse().unwrap_or(0), peer).await,
            ["no", id] => {
                let id = id.parse::<i64>().unwrap_or(0);
                self.ctx.db.call(move |c| repo::pending_take(c, uid, id)).await?;
                self.say(peer, "Відхилено, нічого не відправлено.").await
            }
            ["pa", id] => {
                let id = id.parse::<i64>().unwrap_or(0);
                let Some((_, raw)) = self.ctx.db.call(move |c| repo::pending_take(c, uid, id)).await? else { return self.say(peer, "Ця дія вже виконана.").await };
                let v: serde_json::Value = serde_json::from_str(&raw)?;
                let action = v["action"].as_str().unwrap_or("summary").to_string();
                let peers: Vec<(i64, String)> = serde_json::from_value(v["peers"].clone()).unwrap_or_default();
                self.run_for_all(&action, peers, peer).await
            }
            ["sel", id, pid] => self.select_recipient(id.parse().unwrap_or(0), pid.parse().unwrap_or(0), peer).await,
            _ => Ok(()),
        }
    }

    /// Runs one chat action for several chats, each under its own heading.
    pub(crate) async fn run_for_all(&self, action: &str, peers: Vec<(i64, String)>, peer: PeerRef) -> Result<()> {
        *self.last_pick.lock().await = None;
        for (id, name) in peers {
            self.say(peer, &format!("<b>{}</b>", esc(&name))).await?;
            if let Err(e) = self.run_chat_action(action, id, &name, peer).await {
                tracing::warn!("chat action for {name} failed: {e:#}");
                self.say(peer, "Не вдалося обробити цей чат.").await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn contact_name(&self, peer_id: i64) -> Result<String> {
        let uid = self.ctx.user_id;
        Ok(self.ctx.db.call(move |c| repo::list_contacts(c, uid)).await?.into_iter().find(|k| k.peer_id == peer_id).map_or_else(|| peer_id.to_string(), |k| k.display_name))
    }

    pub(crate) async fn free_text_public(&self, text: &str, peer: PeerRef) -> Result<()> {
        self.free_text(text, peer).await
    }

    pub(crate) async fn command_by_name(&self, cmd: &str, arg: &str, peer: PeerRef) -> Result<()> {
        match cmd {
            "todos" => self.todos(peer).await,
            "search" => self.search(arg, peer).await,
            _ => Ok(()),
        }
    }

    async fn free_text(&self, text: &str, peer: PeerRef) -> Result<()> {
        // "both" / "all of them" answers the last "which one?" without another trip to the model.
        if is_all_answer(text) {
            if let Some((action, cands)) = self.last_pick.lock().await.take() {
                return self.run_for_all(&action, cands, peer).await;
            }
        }
        let Some(llm) = self.ctx.llm().await? else {
            return self.say(peer, "Спершу додай LLM-ключ: <code>/key openai sk-…</code>").await;
        };
        let s = self.ctx.settings().await?;
        let tz: chrono_tz::Tz = s.timezone.parse().unwrap_or(chrono_tz::UTC);
        let now_local = chrono::Utc::now().with_timezone(&tz).format("%Y-%m-%d %H:%M %A");
        let mut system = format!(
            "Текущее локальное время владельца: {now_local} ({tz}).\nКогда нужно превратить относительную дату («завтра», «через час», «в пятницу 18:00») в ISO-8601, считай в этом TZ и потом конвертируй в UTC.\n\n{AGENT_PROMPT}"
        );
        {
            let mem = self.memory.lock().await;
            if !mem.is_empty() {
                let block: Vec<String> = mem.iter().map(|(u, a)| format!("Владелец: {u}\nБот: {a}")).collect();
                system.push_str(&format!("\n\nКраткая память недавнего диалога (для отсылок «ему», «в том же чате»):\n{}", block.join("\n---\n")));
            }
        }
        let raw = match llm.chat("agent", &[ChatMessage::system(system), ChatMessage::user(text)], s.use_heavy_model).await {
            Ok(r) => r,
            Err(e) => return self.say(peer, &format!("LLM недоступний: {}", esc(&e.to_string()))).await,
        };
        let mut summary = Vec::new();
        for it in intent::flatten(intent::parse_intent(&raw)) {
            summary.push(format!("{it:?}").chars().take(160).collect::<String>());
            if let Err(e) = self.run_intent(it, peer).await {
                tracing::warn!("intent failed: {e:#}");
                self.say(peer, "Щось пішло не так під час виконання. Деталі в логах.").await?;
            }
        }
        let mut mem = self.memory.lock().await;
        mem.push_back((text.chars().take(300).collect(), summary.join("; ")));
        while mem.len() > 6 {
            mem.pop_front();
        }
        Ok(())
    }
}

/// Short replies meaning "all candidates" (Ukrainian / Russian / English).
pub(crate) fn is_all_answer(text: &str) -> bool {
    let t: String = text.to_lowercase().chars().filter(|c| c.is_alphanumeric() || c.is_whitespace()).collect();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    matches!(t.as_str(), "усі" | "всі" | "все" | "обидва" | "обидві" | "обоє" | "і те і те" | "і той і той" | "и то и то" | "оба" | "обе" | "всё" | "both" | "all" | "all of them" | "кожен" | "кожний")
}

/// Plain-text version of (possibly broken) HTML, for the fallback when Telegram rejects the markup.
pub(crate) fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&")
}

/// `/set flag 1` and models answering `1`/`0` for a boolean setting are accepted as true/false.
fn coerce_bool(col: &str, v: &serde_json::Value) -> serde_json::Value {
    const BOOLS: &[&str] = &["auto_reply_enabled", "digest_enabled", "news_enabled", "reminders_enabled", "reminder_overdue_enabled", "ignore_archived", "use_heavy_model"];
    match (BOOLS.contains(&col), v.as_i64()) {
        (true, Some(0)) => serde_json::Value::Bool(false),
        (true, Some(1)) => serde_json::Value::Bool(true),
        _ => v.clone(),
    }
}

pub(crate) fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub(crate) fn clean(html: &str) -> String {
    sanitize_html(html)
}

fn parse_scalar(s: &str) -> serde_json::Value {
    match s.to_lowercase().as_str() {
        "true" | "on" | "yes" | "так" | "вкл" => serde_json::Value::Bool(true),
        "false" | "off" | "no" | "ні" | "викл" => serde_json::Value::Bool(false),
        _ => s.parse::<i64>().map(Into::into).unwrap_or_else(|_| serde_json::Value::String(s.to_string())),
    }
}

/// Type/range check per setting so a confused LLM cannot store garbage (e.g. digest_time = "morning").
pub(crate) fn validate_setting(col: &str, v: &serde_json::Value) -> std::result::Result<(), &'static str> {
    use serde_json::Value::*;
    let hhmm = |s: &str| s.len() == 5 && s.as_bytes()[2] == b':' && s[..2].parse::<u8>().is_ok_and(|h| h < 24) && s[3..].parse::<u8>().is_ok_and(|m| m < 60);
    match (col, v) {
        ("auto_reply_enabled" | "digest_enabled" | "news_enabled" | "reminders_enabled" | "reminder_overdue_enabled" | "ignore_archived" | "use_heavy_model", Bool(_)) => Ok(()),
        ("auto_reply_enabled" | "digest_enabled" | "news_enabled" | "reminders_enabled" | "reminder_overdue_enabled" | "ignore_archived" | "use_heavy_model", _) => Err("потрібно true/false"),
        ("auto_reply_mode", String(s)) if s == "static" || s == "smart" => Ok(()),
        ("auto_reply_mode", _) => Err("static або smart"),
        ("llm_provider", String(s)) if tgh_core::llm::Provider::parse(s).is_some() => Ok(()),
        ("llm_provider", _) => Err("gemini, groq, zai або openai"),
        ("digest_time" | "news_digest_time", String(s)) if hhmm(s) => Ok(()),
        ("digest_time" | "news_digest_time", _) => Err("формат HH:MM"),
        ("timezone", String(s)) if s.parse::<chrono_tz::Tz>().is_ok() => Ok(()),
        ("timezone", _) => Err("потрібна IANA-зона, напр. Europe/Kyiv"),
        ("auto_reply_cooldown_min" | "news_window_hours" | "reminder_lead_hours", Number(n)) if n.as_i64().is_some_and(|x| (1..=10_000).contains(&x)) => Ok(()),
        ("auto_reply_cooldown_min" | "news_window_hours" | "reminder_lead_hours", _) => Err("ціле число ≥ 1"),
        ("auto_reply_text", String(s)) if !s.trim().is_empty() && s.chars().count() <= 500 => Ok(()),
        ("auto_reply_text", _) => Err("непорожній текст до 500 символів"),
        _ => Err("невідоме налаштування"),
    }
}

const NOT_LOGGED: &str = "Userbot не підключено — спершу /login.";

const HELP: &str = "<b>TelegramHelper</b> — асистент для твого акаунта.\n\n\
<b>Акаунт</b>: /login · /qr (вхід QR-кодом) · /logout · /sync · /status\n\
<b>Ключі</b>: <code>/key openai sk-…</code> · <code>/key gemini …</code>\n\
<b>Налаштування</b>: /settings · <code>/set ключ значення</code>\n\
<b>Чати</b>: <code>/chat Ім'я</code> · <code>/catchup Ім'я</code> · <code>/send інструкція</code> · <code>/search текст</code>\n\
<b>Пам'ять</b>: /todos · /digest · /news [тема] · /topics · /classify · <code>/sources Ім'я</code>\n<b>Чати й канали (галочки, іконки)</b>: веб-інтерфейс, сторінка /chats\n\n\
Або просто пиши словами: «напиши Олі, що дзвінок о 8», «нагадай завтра о 18:00 подзвонити мамі».\n\
Усе, що бачать інші (відправка), — лише після підтвердження кнопкою.";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn setting_validation_rejects_garbage() {
        assert!(validate_setting("digest_time", &json!("07:30")).is_ok());
        assert!(validate_setting("digest_time", &json!("morning")).is_err());
        assert!(validate_setting("digest_time", &json!("25:00")).is_err());
        assert!(validate_setting("auto_reply_enabled", &json!("yes")).is_err());
        assert!(validate_setting("auto_reply_enabled", &json!(true)).is_ok());
        assert!(validate_setting("timezone", &json!("Europe/Kyiv")).is_ok());
        assert!(validate_setting("timezone", &json!("Mars/Base")).is_err());
        assert!(validate_setting("auto_reply_cooldown_min", &json!(0)).is_err());
        assert!(validate_setting("llm_provider", &json!("gemini")).is_ok());
    }

    #[test]
    fn splitting_respects_limit_and_lines() {
        let text = format!("{}\n{}\n{}", "a".repeat(30), "b".repeat(30), "c".repeat(90));
        let parts = split_message(&text, 50);
        assert!(parts.iter().all(|p| p.chars().count() <= 50));
        assert_eq!(parts.concat().replace('\n', ""), text.replace('\n', ""));
        assert_eq!(split_message("short", 50), vec!["short".to_string()]);
    }

    #[test]
    fn all_answers_are_recognized() {
        for t in ["і те і те", "Обидва!", "усі", "Both", "  всі  "] {
            assert!(is_all_answer(t), "{t}");
        }
        assert!(!is_all_answer("напиши Олі привіт"));
        assert!(!is_all_answer("покажи новини"));
    }

    #[test]
    fn html_fallback_strips_tags_and_unescapes() {
        assert_eq!(strip_html("<b>Привіт</b> &amp; <i>світе</i> &lt;3"), "Привіт & світе <3");
        assert_eq!(strip_html("broken <b oops"), "broken ");
        assert_eq!(strip_html(""), "");
    }

    #[test]
    fn booleans_accept_zero_one_and_providers_accept_new_names() {
        assert_eq!(coerce_bool("digest_enabled", &json!(1)), json!(true));
        assert_eq!(coerce_bool("digest_enabled", &json!(0)), json!(false));
        assert_eq!(coerce_bool("auto_reply_cooldown_min", &json!(1)), json!(1)); // numeric settings untouched
        assert!(validate_setting("llm_provider", &json!("groq")).is_ok());
        assert!(validate_setting("llm_provider", &json!("zai")).is_ok());
        assert!(validate_setting("llm_provider", &json!("skynet")).is_err());
    }

    #[test]
    fn scalar_parsing() {
        assert_eq!(parse_scalar("on"), json!(true));
        assert_eq!(parse_scalar("15"), json!(15));
        assert_eq!(parse_scalar("07:00"), json!("07:00"));
    }

    #[test]
    fn splitting_is_lossless_and_bounded_for_any_unicode() {
        let atoms = ["a", "я", "🙂", "\n", "\n\n", " ", "字", "<b>", "</b>", "&amp;", "é", "İ", "\u{200d}", "x".repeat(70).leak() as &str];
        let mut x: u64 = 0xABCDEF0123456789;
        let mut next = |m: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % m as u64) as usize
        };
        for _ in 0..3000 {
            let n = next(60);
            let text: String = (0..n).map(|_| atoms[next(atoms.len())]).collect();
            let max = 1 + next(80);
            let parts = split_message(&text, max);
            assert!(parts.iter().all(|p| p.chars().count() <= max), "chunk longer than {max}");
            assert!(parts.iter().all(|p| !p.trim().is_empty()) || text.trim().is_empty());
            // nothing lost except purely-blank trailing chunks
            let joined: String = parts.concat();
            let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
            assert_eq!(strip(&joined), strip(&text), "content changed for max={max}");
            assert!(std::str::from_utf8(joined.as_bytes()).is_ok());
        }
        assert!(split_message("", 10).is_empty());
        assert!(split_message("   \n  ", 10).is_empty());
    }

    #[test]
    fn strip_html_never_panics() {
        for t in ["<", ">", "<<>>", "<b", "b>", "&", "&amp", "🙂<🙂>🙂", &"<".repeat(10_000), &"&lt;".repeat(10_000)] {
            let _ = strip_html(t);
            let _ = esc(t);
        }
    }

    #[test]
    fn flood_wait_is_parsed_from_error_text() {
        assert_eq!(flood_wait_secs("bot sign-in failed: request error: rpc error 420: FLOOD_WAIT caused by auth.importBotAuthorization (value: 1361)"), Some(1361));
        assert_eq!(flood_wait_secs("FLOOD_WAIT (value: 7) trailing"), Some(7));
        assert_eq!(flood_wait_secs("FLOOD_WAIT without number"), None);
        assert_eq!(flood_wait_secs("value: 5 but no flood"), None);
        assert_eq!(flood_wait_secs(""), None);
    }
}
