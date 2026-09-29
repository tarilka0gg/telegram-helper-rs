//! News digest from the channels marked as sources.
//!
//! 1. pull the latest posts of every source (backfill, deduplicated by message id);
//! 2. take what appeared in the last 24 h; if there is nothing, fall back to the latest post
//!    of each source, whatever its age;
//! 3. never repeat: posts that were already delivered are remembered in `news_sent`.

use std::sync::Arc;

use anyhow::Result;
use chrono::{Duration, Utc};
use tgh_core::{
    db::repo::{self, MessageRow},
    llm::ChatMessage,
    sanitize::sanitize_html,
};

use crate::{ctx::Ctx, manager::Manager, userbot};

const SYSTEM: &str = "Ти робиш дайджест новин із підписаних каналів. Згрупуй за змістом, прибери дублі, \
3–7 пунктів, HTML (<b>, <i>). Наприкінці — назви каналів-джерел. Нічого не вигадуй понад текст постів.";

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

pub async fn build(ctx: &Arc<Ctx>, mgr: &Manager, topic: Option<&str>) -> Result<News> {
    build_with(ctx, mgr, topic, None).await
}

/// `llm` lets tests inject a fake provider; `None` uses the configured chain.
pub async fn build_with(ctx: &Arc<Ctx>, mgr: &Manager, topic: Option<&str>, llm: Option<&tgh_core::llm::LlmClient>) -> Result<News> {
    let uid = ctx.user_id;
    let sources = ctx.db.call(move |c| repo::news_sources(c, uid)).await?;
    if sources.is_empty() {
        return Ok(News::Nothing(
            "Немає каналів-джерел. Познач їх на сторінці /chats у веб-інтерфейсі або командою /sources Назва.".into(),
        ));
    }
    if let Some(client) = mgr.client().await {
        for (peer_id, kind, name) in &sources {
            let Some(pref) = mgr.peer_ref(kind, *peer_id).await else {
                tracing::warn!("news source '{name}' is not in the session cache yet (run /sync)");
                continue;
            };
            if let Err(e) = userbot::backfill_peer(ctx, &client, pref, *peer_id, 15).await {
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
    let owned;
    let llm = match llm {
        Some(l) => l,
        None => {
            owned = ctx.llm().await?;
            let Some(l) = owned.as_ref() else { return Ok(News::Nothing("Спершу додай LLM-ключ.".into())) };
            l
        }
    };
    let heavy = ctx.settings().await?.use_heavy_model;
    let note = if fallback {
        "За добу нових постів немає — нижче останній наявний пост кожного каналу.\n\n"
    } else {
        ""
    };
    let body: String = posts.iter().map(format_post).collect::<Vec<_>>().join("\n\n");
    let head = topic.map(|t| format!("Тема: {t}\n\n")).unwrap_or_default();
    let raw = llm
        .chat(
            "news",
            &[ChatMessage::system(format!("{SYSTEM}{}", crate::features::UK)), ChatMessage::user(format!("{head}{note}{body}"))],
            heavy,
        )
        .await?;
    let html = sanitize_html(&raw);
    let html = if fallback {
        format!("<i>За добу нових постів немає — показую останні наявні.</i>\n\n{html}")
    } else {
        html
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::testkit::{ctx, fake_llm, reply, user_text};
    use tgh_core::llm::{LlmClient, Provider};

    async fn seed(ctx: &Arc<Ctx>, posts: &[(i64, i64, &str, &str)]) {
        let uid = ctx.user_id;
        let posts: Vec<(i64, i64, String, String)> = posts.iter().map(|(p, m, d, t)| (*p, *m, d.to_string(), t.to_string())).collect();
        ctx.db
            .call(move |c| {
                for (peer, name) in [(10i64, "Channel A"), (11, "Channel B")] {
                    repo::upsert_contact(
                        c,
                        uid,
                        &repo::ContactRow {
                            peer_id: peer,
                            peer_kind: "channel".into(),
                            is_bot: false,
                            is_archived: false,
                            display_name: name.into(),
                            username: None,
                        },
                    )?;
                    repo::update_contact_flags(c, uid, peer, Some(true), None, None)?;
                }
                for (peer, id, date, text) in &posts {
                    repo::save_message(
                        c,
                        uid,
                        &repo::MessageRow {
                            peer_id: *peer,
                            message_id: *id,
                            sender_id: None,
                            sender_name: None,
                            is_outgoing: false,
                            date: date.clone(),
                            kind: "text".into(),
                            text: Some(text.clone()),
                        },
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn digest_lifecycle_recent_then_latest_then_nothing() {
        let ctx = ctx().await;
        let mgr = Manager::new(ctx.clone());
        let today = repo::fmt_ts(Utc::now() - Duration::hours(2));
        seed(&ctx, &[(10, 1, &today, "fresh rust release"), (11, 1, "2020-01-01 10:00:00", "ancient news from B")]).await;
        let base = fake_llm(|req| reply(&user_text(&req))).await; // echoes the prompt: lets us inspect it
        let llm = LlmClient::with_base(Provider::Groq, "k".into(), ctx.db.clone(), &base);

        // 1) something happened today: only today's post goes in (the ancient one is not "recent")
        let News::Digest(d1) = build_with(&ctx, &mgr, None, Some(&llm)).await.unwrap() else { panic!("expected a digest") };
        assert!(d1.html.contains("fresh rust release") && !d1.html.contains("ancient"), "{}", d1.html);
        assert_eq!(d1.posts, vec![(10, 1)]);
        mark_sent(&ctx, d1.posts).await.unwrap();

        // 2) nothing new today -> fall back to the latest unsent post per source, and say so
        let News::Digest(d2) = build_with(&ctx, &mgr, None, Some(&llm)).await.unwrap() else { panic!("expected fallback digest") };
        assert!(d2.html.contains("ancient news from B") && !d2.html.contains("fresh rust"), "{}", d2.html);
        assert!(d2.html.contains("За добу нових постів немає"), "fallback must be announced: {}", d2.html);
        mark_sent(&ctx, d2.posts).await.unwrap();

        // 3) everything was delivered once: never repeat
        match build_with(&ctx, &mgr, None, Some(&llm)).await.unwrap() {
            News::Nothing(why) => assert!(why.contains("вже надсилав"), "{why}"),
            News::Digest(_) => panic!("already delivered posts were sent again"),
        }
    }

    #[tokio::test]
    async fn topic_filter_and_missing_sources() {
        let ctx = ctx().await;
        let mgr = Manager::new(ctx.clone());
        let base = fake_llm(|req| reply(&user_text(&req))).await;
        let llm = LlmClient::with_base(Provider::Groq, "k".into(), ctx.db.clone(), &base);
        // no sources at all -> a helpful explanation, no LLM call
        assert!(matches!(build_with(&ctx, &mgr, None, Some(&llm)).await.unwrap(), News::Nothing(w) if w.contains("Немає каналів-джерел")));
        let now = repo::fmt_ts(Utc::now() - Duration::hours(1));
        seed(&ctx, &[(10, 1, &now, "Rust 2.0 announced"), (10, 2, &now, "cooking tips"), (11, 1, &now, "RUST weekly")]).await;
        let News::Digest(d) = build_with(&ctx, &mgr, Some("rust"), Some(&llm)).await.unwrap() else { panic!() };
        assert_eq!(d.posts.len(), 2, "topic match is case-insensitive and excludes other posts");
        assert!(d.html.contains("Тема: rust"));
        assert!(!d.html.contains("cooking"));
        // a topic nobody wrote about
        assert!(matches!(build_with(&ctx, &mgr, Some("quantum"), Some(&llm)).await.unwrap(), News::Nothing(w) if w.contains("quantum")));
    }
}
