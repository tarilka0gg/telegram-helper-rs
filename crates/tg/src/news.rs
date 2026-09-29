//! News digest from the channels marked as sources.
//!
//! 1. pull the latest posts of every source (backfill, deduplicated by message id);
//! 2. take what appeared in the last 24 h; if there is nothing, fall back to the latest post
//!    of each source, whatever its age;
//! 3. never repeat: posts that were already delivered are remembered in `news_sent`.

use std::sync::Arc;

use anyhow::Result;
use chrono::{Duration, Utc};
use grammers_client::Client;
use tgh_core::{
    db::repo::{self, MessageRow},
    llm::ChatMessage,
    sanitize::sanitize_html,
};

use crate::{ctx::Ctx, userbot};

const SYSTEM: &str = "Ты делаешь дайджест новостей из подписанных каналов. Сгруппируй по смыслу, убери дубли, \
3–7 пунктов, HTML (<b>, <i>). В конце — названия каналов-источников. Ничего не выдумывай сверх текста постов.";

pub struct NewsPack {
    pub html: String,
    /// (peer_id, message_id) of every post included — mark them sent after delivery.
    pub posts: Vec<(i64, i64)>,
}

pub enum News {
    Digest(NewsPack),
    /// Human explanation of why there is nothing to send.
    Nothing(String),
}

pub async fn build(ctx: &Arc<Ctx>, client: Option<&Client>, topic: Option<&str>) -> Result<News> {
    let uid = ctx.user_id;
    let sources = ctx.db.call(move |c| repo::news_sources(c, uid)).await?;
    if sources.is_empty() {
        return Ok(News::Nothing("Немає каналів-джерел. Познач їх на сторінці /chats у веб-інтерфейсі або командою /sources Назва.".into()));
    }
    if let Some(client) = client {
        for (peer_id, kind, name) in &sources {
            if let Err(e) = userbot::backfill_peer(ctx, client, *peer_id, kind, 15).await {
                tracing::warn!("news backfill '{name}' failed: {e:#}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    }

    let (since, topic_owned) = (repo::fmt_ts(Utc::now() - Duration::hours(24)), topic.map(str::to_string));
    let t = topic_owned.clone();
    let mut posts = ctx.db.call(move |c| repo::news_unsent(c, uid, &since, t.as_deref(), 40)).await?;
    let mut fallback = false;
    if posts.is_empty() {
        let t = topic_owned.clone();
        posts = ctx.db.call(move |c| repo::news_latest_unsent(c, uid, t.as_deref(), 1)).await?;
        fallback = true;
    }
    if posts.is_empty() {
        return Ok(News::Nothing(match topic {
            Some(t) => format!("Нових постів по темі «{t}» немає (усе знайдене вже надсилалось)."),
            None => "Нових постів немає — усе, що є в джерелах, я вже надсилав.".into(),
        }));
    }
    let Some(llm) = ctx.llm().await? else { return Ok(News::Nothing("Спершу додай LLM-ключ.".into())) };
    let heavy = ctx.settings().await?.use_heavy_model;
    let note = if fallback { "За добу нових постів немає — нижче останній наявний пост кожного каналу.\n\n" } else { "" };
    let body: String = posts.iter().map(format_post).collect::<Vec<_>>().join("\n\n");
    let head = topic.map(|t| format!("Тема: {t}\n\n")).unwrap_or_default();
    let raw = llm.chat("news", &[ChatMessage::system(SYSTEM), ChatMessage::user(format!("{head}{note}{body}"))], heavy).await?;
    let html = sanitize_html(&raw);
    let html = if fallback { format!("<i>За добу нових постів немає — показую останні наявні.</i>\n\n{html}") } else { html };
    Ok(News::Digest(NewsPack { html, posts: posts.iter().map(|p| (p.peer_id, p.message_id)).collect() }))
}

fn format_post(m: &MessageRow) -> String {
    let text: String = m.text.as_deref().unwrap_or("").chars().take(600).collect();
    format!("[{}] {}: {}", m.date.get(..16).unwrap_or(&m.date), m.sender_name.as_deref().unwrap_or("канал"), text)
}

pub async fn mark_sent(ctx: &Ctx, posts: Vec<(i64, i64)>) -> Result<()> {
    let uid = ctx.user_id;
    ctx.db.call(move |c| repo::mark_news_sent(c, uid, &posts)).await
}
