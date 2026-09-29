//! The owner's own account over MTProto: mirrors every message into SQLite, answers
//! privately while offline, and can send on the owner's behalf.

use std::sync::{atomic::Ordering, Arc};

use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use grammers_client::{
    client::UpdatesConfiguration,
    message::{InputMessage, Message},
    peer::Peer,
    session::updates::UpdatesLike,
    sender::SenderPoolFatHandle,
    tl, Client, SenderPool,
};
use grammers_session::Session;
use tgh_core::{
    db::repo::{self, ContactRow, MessageRow},
    llm::ChatMessage,
};
use tokio::{sync::mpsc::UnboundedReceiver, task::JoinHandle};

use crate::ctx::Ctx;

pub struct Connected {
    pub client: Client,
    pub handle: SenderPoolFatHandle,
    pub updates: UnboundedReceiver<UpdatesLike>,
    pub pool_task: JoinHandle<()>,
}

/// Starts the network runner on top of `session` (in-memory or DB-backed).
pub fn connect<S>(session: Arc<S>, api_id: i32) -> Connected
where
    S: Session,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let SenderPool { runner, updates, handle } = SenderPool::new(session, api_id);
    let client = Client::new(handle.clone());
    let pool_task = tokio::spawn(async move { runner.run().await });
    Connected { client, handle, updates, pool_task }
}

pub fn peer_kind(p: &Peer) -> &'static str {
    match p {
        Peer::User(_) => "user",
        // Megagroups live in the channel id space; keep them apart from small group chats.
        Peer::Group(g) if g.is_megagroup() => "supergroup",
        Peer::Group(_) => "chat",
        Peer::Channel(_) => "channel",
    }
}

fn peer_row(p: &Peer, archived: bool) -> Option<ContactRow> {
    let peer_id = p.id().bare_id()?;
    let name = match p {
        Peer::User(u) => Some(u.full_name()).filter(|n| !n.is_empty()),
        _ => p.name().map(str::to_string),
    };
    Some(ContactRow {
        peer_id,
        peer_kind: peer_kind(p).into(),
        is_bot: matches!(p, Peer::User(u) if u.is_bot()),
        is_archived: archived,
        display_name: name.or_else(|| p.username().map(str::to_string)).unwrap_or_else(|| peer_id.to_string()),
        username: p.username().map(str::to_string),
    })
}

fn message_row(m: &Message, peer_id: i64) -> MessageRow {
    let kind = match m.media() {
        None if m.text().is_empty() => "other",
        None => "text",
        Some(grammers_client::media::Media::Photo(_)) => "photo",
        Some(_) => "document",
    };
    MessageRow {
        peer_id,
        message_id: m.id() as i64,
        sender_id: m.sender_id().and_then(|s| s.bare_id()),
        sender_name: m.sender().and_then(|s| s.name().map(str::to_string)),
        is_outgoing: m.outgoing(),
        date: repo::fmt_ts(m.date()),
        kind: kind.into(),
        text: Some(m.text().to_string()).filter(|t| !t.is_empty()),
    }
}

/// Main loop: runs until the update stream fails or the process is stopped.
pub async fn run_updates(ctx: Arc<Ctx>, conn: Connected) -> Result<()> {
    let Connected { client, handle, updates, pool_task } = conn;
    let mut stream = client
        .stream_updates(updates, UpdatesConfiguration { catch_up: true, ..Default::default() })
        .await
        .map_err(|e| anyhow::anyhow!("stream_updates: {e}"))?;
    ctx.status.userbot_connected.store(true, Ordering::Relaxed);
    ctx.event("userbot_online", None, None).await;
    tracing::info!("userbot: listening for updates");

    let result = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break Ok(()),
            upd = stream.next() => match upd {
                Ok(grammers_client::update::Update::NewMessage(m)) => {
                    let (ctx, client) = (ctx.clone(), client.clone());
                    tokio::spawn(async move {
                        if let Err(e) = on_message(&ctx, &client, m.into_inner(), false).await {
                            tracing::warn!("message handler failed: {e:#}");
                        }
                    });
                }
                Ok(grammers_client::update::Update::MessageEdited(m)) => {
                    let (ctx, client) = (ctx.clone(), client.clone());
                    tokio::spawn(async move {
                        if let Err(e) = on_message(&ctx, &client, m.into_inner(), true).await {
                            tracing::warn!("message handler failed: {e:#}");
                        }
                    });
                }
                Ok(_) => {}
                Err(e) => break Err(anyhow::anyhow!("update stream: {e}")),
            }
        }
    };
    ctx.status.userbot_connected.store(false, Ordering::Relaxed);
    ctx.event("userbot_offline", None, None).await;
    let _ = stream.sync_update_state().await;
    handle.quit();
    let _ = pool_task.await;
    result
}

async fn on_message(ctx: &Arc<Ctx>, client: &Client, m: Message, edited: bool) -> Result<()> {
    let Some(peer) = m.peer().cloned() else { return Ok(()) };
    let Some(peer_id) = peer.id().bare_id() else { return Ok(()) };
    let uid = ctx.user_id;

    let (row, contact) = (message_row(&m, peer_id), peer_row(&peer, false));
    // Chats switched off in the web UI are neither stored nor counted.
    let mirror = ctx.db.call(move |c| repo::mirror_enabled(c, uid, peer_id)).await?;
    let is_new = mirror
        && ctx
            .db
            .call(move |c| {
                if let Some(k) = contact {
                    // Keep flags we already know: archive state only changes through dialog sync.
                    let archived = c.query_row("SELECT is_archived FROM contacts WHERE user_id = ? AND peer_id = ?", [uid, k.peer_id], |r| r.get(0)).unwrap_or(false);
                    repo::upsert_contact(c, uid, &ContactRow { is_archived: archived, ..k })?;
                }
                repo::save_message(c, uid, &row)
            })
            .await?;
    if is_new {
        ctx.event(if m.outgoing() { "msg_out" } else { "msg_in" }, Some(peer_id), None).await;
    }

    // Auto-reply only to fresh incoming private messages (an edit is not a new message).
    if !edited && !m.outgoing() && (is_new || !mirror) {
        if let Peer::User(u) = &peer {
            if !u.is_bot() && !u.is_self() {
                maybe_auto_reply(ctx, client, &m, &peer, peer_id).await?;
            }
        }
    }
    Ok(())
}

const AUTO_REPLY_SYSTEM: &str = "Ты отвечаешь от моего имени, потому что я сейчас оффлайн. Будь краток, дружелюбен.\n\
Если просят что-то конкретное — не обещай за меня, скажи: «передам, сейчас занят».\n\
Если уточняющий вопрос явно нужен — задай его.\n\
Никогда не выдавай, что ты ИИ. Не упоминай ассистента.\n\
Не пиши длиннее 1–3 коротких предложений.";

async fn maybe_auto_reply(ctx: &Arc<Ctx>, client: &Client, m: &Message, peer: &Peer, peer_id: i64) -> Result<()> {
    let incoming = m.text().trim().to_string();
    if incoming.is_empty() {
        return Ok(()); // media without text: never auto-answer
    }
    let s = ctx.settings().await?;
    if !s.auto_reply_enabled {
        return Ok(());
    }
    let uid = ctx.user_id;
    let (archived, last): (bool, Option<String>) = ctx
        .db
        .call(move |c| {
            let a = c.query_row("SELECT is_archived FROM contacts WHERE user_id = ? AND peer_id = ?", [uid, peer_id], |r| r.get(0)).unwrap_or(false);
            Ok((a, repo::last_auto_reply_at(c, uid, peer_id)?))
        })
        .await?;
    if s.ignore_archived && archived {
        return Ok(());
    }
    if let Some(last) = last {
        let since = chrono::NaiveDateTime::parse_from_str(&last, repo::TS_FMT).ok().map(|t| Utc::now().naive_utc() - t);
        if since.is_some_and(|d| d < Duration::minutes(s.auto_reply_cooldown_min)) {
            return Ok(());
        }
    }
    // Only answer when the owner is really offline.
    let me = client.get_me().await?;
    if matches!(me.status(), tl::enums::UserStatus::Online(_)) {
        return Ok(());
    }

    let name = peer.name().unwrap_or("").to_string();
    let reply = if s.auto_reply_mode == "smart" {
        match smart_reply(ctx, &s, peer_id, &name, &incoming).await {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()),
            Err(e) => {
                tracing::warn!("smart auto-reply failed: {e:#}");
                return Ok(());
            }
        }
    } else {
        s.auto_reply_text.clone()
    };
    if reply.trim().is_empty() {
        return Ok(());
    }

    client.send_message(peer_ref(peer).await?, InputMessage::new().text(reply.clone())).await?;
    let (name2, incoming2) = (name.clone(), incoming.clone());
    ctx.db.call(move |c| repo::log_auto_reply(c, uid, peer_id, &name2, &incoming2, &reply)).await?;
    ctx.event("auto_reply", Some(peer_id), Some(s.auto_reply_mode)).await;
    Ok(())
}

async fn smart_reply(ctx: &Arc<Ctx>, s: &repo::Settings, peer_id: i64, name: &str, incoming: &str) -> Result<Option<String>> {
    let Some(llm) = ctx.llm().await? else {
        tracing::warn!("auto-reply: no LLM key configured");
        return Ok(None);
    };
    let uid = ctx.user_id;
    let history = ctx.db.call(move |c| repo::recent_messages(c, uid, peer_id, 20)).await?;
    let history: String = history
        .iter()
        .map(|h| format!("{}: {}", if h.is_outgoing { "Я" } else { h.sender_name.as_deref().unwrap_or("Собеседник") }, h.text.as_deref().unwrap_or("[медиа]")))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!("Собеседник: {name}.\nКонтекст последних сообщений:\n{history}\n\nПоследнее входящее: {incoming}\n\nСформируй ответ от моего имени.");
    let out = llm.chat("auto_reply", &[ChatMessage::system(AUTO_REPLY_SYSTEM), ChatMessage::user(prompt)], s.use_heavy_model).await?;
    Ok(Some(out.trim().to_string()))
}

/// Refreshes contacts from the dialog list (with archive flags). Returns how many were stored.
pub async fn sync_dialogs(ctx: &Arc<Ctx>, client: &Client) -> Result<usize> {
    let mut rows = Vec::new();
    let mut peers = Vec::new();
    let mut dialogs = client.iter_dialogs();
    while let Some(d) = dialogs.next().await? {
        peers.push(d.peer().clone());
        // Telegram keeps archived chats in folder 1.
        let archived = matches!(&d.raw, tl::enums::Dialog::Dialog(x) if x.folder_id == Some(1));
        if let Some(r) = peer_row(d.peer(), archived) {
            rows.push(r);
        }
    }
    let n = rows.len();
    let uid = ctx.user_id;
    ctx.db
        .call(move |c| {
            let tx = c.unchecked_transaction()?;
            for r in &rows {
                repo::upsert_contact(&tx, uid, r)?;
            }
            tx.commit()
        })
        .await?;
    ctx.event("sync", None, Some(format!("{n} dialogs"))).await;
    spawn_avatar_download(ctx.clone(), client.clone(), peers);
    Ok(n)
}

/// `Peer::to_ref` with a readable error when the peer is not in the session cache.
pub async fn peer_ref(peer: &Peer) -> Result<grammers_client::session::types::PeerRef> {
    peer.to_ref().await.map_err(|e| anyhow::anyhow!("peer ref: {e}"))?.context("peer not in session cache")
}

/// Latest messages of a chat straight from Telegram, oldest first. Nothing is written to the DB.
pub async fn fetch_recent(client: &Client, pref: grammers_client::session::types::PeerRef, peer_id: i64, limit: usize) -> Result<Vec<MessageRow>> {
    let mut iter = client.iter_messages(pref).limit(limit);
    let mut rows = Vec::new();
    while let Some(m) = iter.next().await.map_err(|e| anyhow::anyhow!("iter_messages: {e}"))? {
        rows.push(message_row(&m, peer_id));
    }
    rows.reverse();
    Ok(rows)
}

/// Pulls the latest posts of one chat/channel into the DB (deduplicated by message id).
pub async fn backfill_peer(ctx: &Arc<Ctx>, client: &Client, pref: grammers_client::session::types::PeerRef, peer_id: i64, limit: usize) -> Result<usize> {
    let mut iter = client.iter_messages(pref).limit(limit);
    let mut rows = Vec::new();
    while let Some(m) = iter.next().await.map_err(|e| anyhow::anyhow!("iter_messages: {e}"))? {
        rows.push(message_row(&m, peer_id));
    }
    let uid = ctx.user_id;
    ctx.db
        .call(move |c| {
            let tx = c.unchecked_transaction()?;
            let mut fresh = 0;
            for r in &rows {
                fresh += usize::from(repo::save_message(&tx, uid, r)?);
            }
            tx.commit()?;
            Ok(fresh)
        })
        .await
}

pub fn avatar_path(dir: &std::path::Path, peer_id: i64) -> std::path::PathBuf {
    dir.join(format!("{peer_id}.jpg"))
}

/// Downloads small profile pictures for the chats page (best effort, gentle on rate limits).
pub fn spawn_avatar_download(ctx: Arc<Ctx>, client: Client, peers: Vec<Peer>) {
    tokio::spawn(async move {
        let dir = ctx.cfg.data_dir.join("avatars");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let mut got = 0;
        for p in peers {
            let Some(id) = p.id().bare_id() else { continue };
            let path = avatar_path(&dir, id);
            if path.exists() {
                continue;
            }
            let Ok(Some(photo)) = p.photo(false).await else { continue };
            let mut bytes = Vec::new();
            let mut it = client.iter_download(&photo);
            loop {
                match it.next().await {
                    Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
                    Ok(None) => break,
                    Err(_) => {
                        bytes.clear();
                        break;
                    }
                }
                if bytes.len() > 512 * 1024 {
                    bytes.clear();
                    break;
                }
            }
            if !bytes.is_empty() && std::fs::write(&path, &bytes).is_ok() {
                got += 1;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        tracing::info!("avatars: downloaded {got} new");
    });
}
