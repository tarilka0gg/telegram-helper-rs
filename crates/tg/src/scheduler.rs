//! Background jobs: morning digest, news digests, deadline reminders, hourly dialog sync.
//! Each job checks "already fired?" against the `events` table, so restarts never double-send.

use std::{sync::Arc, time::Duration};

use chrono::{NaiveDateTime, Timelike, Utc};
use grammers_client::session::types::PeerId;
use tgh_core::db::repo;

use crate::{bot::{esc, Bot}, features, userbot};

const TICK: Duration = Duration::from_secs(60);

pub fn spawn_all(bot: Arc<Bot>) {
    tokio::spawn(digest_loop(bot.clone()));
    tokio::spawn(news_loop(bot.clone()));
    tokio::spawn(reminders_loop(bot.clone()));
    tokio::spawn(classify_loop(bot.clone()));
    tokio::spawn(sync_loop(bot));
}

async fn owner(bot: &Bot) -> Option<grammers_client::session::types::PeerRef> {
    // The reference learned from the owner's own messages carries the access hash; the ambient one is a last resort.
    if let Some(r) = bot.owner_ref.lock().await.clone() {
        return Some(r);
    }
    PeerId::user(bot.ctx.cfg.owner_telegram_id).map(PeerId::to_ambient_ref)
}

/// `HH:MM` now in the owner's time zone plus the start of the local day, expressed in UTC.
fn local_clock(tz: &str) -> (String, String) {
    let tz: chrono_tz::Tz = tz.parse().unwrap_or(chrono_tz::UTC);
    let now = Utc::now().with_timezone(&tz);
    let midnight = now.date_naive().and_hms_opt(0, 0, 0).and_then(|m| m.and_local_timezone(tz).earliest()).map(|d| d.with_timezone(&Utc)).unwrap_or_else(Utc::now);
    (format!("{:02}:{:02}", now.hour(), now.minute()), repo::fmt_ts(midnight))
}

async fn fired_today(bot: &Bot, kind: &'static str, since: String, detail: Option<String>) -> bool {
    bot.ctx.db.call(move |c| repo::event_since(c, kind, &since, detail.as_deref())).await.unwrap_or(true)
}

async fn digest_loop(bot: Arc<Bot>) {
    loop {
        tokio::time::sleep(TICK).await;
        let Ok(s) = bot.ctx.settings().await else { continue };
        let (hhmm, day_start) = local_clock(&s.timezone);
        // `>=` rather than `==`: a restart at 09:03 still delivers the 09:00 digest, once.
        if !s.digest_enabled || hhmm < s.digest_time || fired_today(&bot, "digest_sent", day_start, None).await {
            continue;
        }
        let Some(peer) = owner(&bot).await else { continue };
        bot.ctx.event("digest_sent", None, None).await;
        match features::build_digest(&bot.ctx).await {
            Ok(text) => {
                if let Err(e) = bot.say(peer, &text).await {
                    tracing::warn!("digest delivery failed: {e:#}");
                }
            }
            Err(e) => tracing::warn!("digest failed: {e:#}"),
        }
    }
}

async fn news_loop(bot: Arc<Bot>) {
    loop {
        tokio::time::sleep(TICK).await;
        let Ok(s) = bot.ctx.settings().await else { continue };
        // news_digest_time is stored in UTC (as in the original)
        let now = Utc::now();
        let hhmm = format!("{:02}:{:02}", now.hour(), now.minute());
        let day_start = repo::fmt_ts(now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc());
        if !s.news_enabled || hhmm < s.news_digest_time || fired_today(&bot, "news_sent", day_start, None).await {
            continue;
        }
        let uid = bot.ctx.user_id;
        let Ok(topics) = bot.ctx.db.call(move |c| repo::list_news_topics(c, uid)).await else { continue };
        let Some(peer) = owner(&bot).await else { continue };
        bot.ctx.event("news_sent", None, None).await;
        // No topics configured: one general digest of all sources.
        let topics = if topics.is_empty() { vec![(String::new(), 24)] } else { topics };
        for (topic, hours) in topics {
            if let Err(e) = bot.news_digest(&topic, hours, peer).await {
                tracing::warn!("news digest '{topic}' failed: {e:#}");
            }
        }
    }
}

async fn reminders_loop(bot: Arc<Bot>) {
    loop {
        tokio::time::sleep(TICK).await;
        let Ok(s) = bot.ctx.settings().await else { continue };
        if !s.reminders_enabled {
            continue;
        }
        let uid = bot.ctx.user_id;
        let Ok(items) = bot.ctx.db.call(move |c| repo::commitments_with_deadline(c, uid)).await else { continue };
        let Some(peer) = owner(&bot).await else { continue };
        let now = Utc::now().naive_utc();
        for (id, who, text, deadline, status) in items {
            let Ok(dl) = NaiveDateTime::parse_from_str(&deadline, repo::TS_FMT) else { continue };
            let left = dl - now;
            let (label, new_status) = if left < chrono::Duration::zero() {
                if !s.reminder_overdue_enabled { continue }
                ("⚠ Прострочено", "overdue")
            } else if status == "open" && left <= chrono::Duration::hours(s.reminder_lead_hours) {
                ("⏰ Скоро", "reminded")
            } else {
                continue;
            };
            let who = if who.is_empty() { String::new() } else { format!(" ({})", esc(&who)) };
            let sent = bot.say(peer, &format!("{label}: {}{who}\nдо {deadline} UTC", esc(&text))).await;
            if sent.is_ok() {
                let st = new_status.to_string();
                let _ = bot.ctx.db.call(move |c| repo::set_commitment_status(c, uid, id, &st)).await;
            }
        }
    }
}

async fn sync_loop(bot: Arc<Bot>) {
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        if let Some(client) = bot.mgr.client().await {
            if let Err(e) = userbot::sync_dialogs(&bot.ctx, &client).await {
                tracing::warn!("hourly sync failed: {e:#}");
            }
        }
    }
}

/// First run: once the userbot is connected and an LLM key exists, sort all chats/channels by context
/// and tell the owner where to review the result. Happens once; `/classify` handles later additions.
/// A flaky LLM gets three attempts (5 min apart) before we give up quietly.
async fn classify_loop(bot: Arc<Bot>) {
    let mut attempts = 0;
    loop {
        tokio::time::sleep(if attempts == 0 { TICK } else { Duration::from_secs(300) }).await;
        if bot.mgr.client().await.is_none() || fired_today(&bot, "auto_classified", "1970-01-01 00:00:00".into(), None).await {
            continue;
        }
        if !matches!(bot.ctx.llm().await, Ok(Some(_))) {
            continue;
        }
        // Contacts must exist first (the initial dialog sync runs right after login).
        let uid = bot.ctx.user_id;
        if bot.ctx.db.call(move |c| repo::list_contacts(c, uid)).await.map_or(true, |v| v.is_empty()) {
            continue;
        }
        attempts += 1;
        match crate::classify::run(&bot.ctx, 400).await {
            Ok(Some((n, news))) if n > 0 => {
                bot.ctx.event("auto_classified", None, Some(format!("{n} chats, {news} news"))).await;
                if let Some(peer) = owner(&bot).await {
                    let _ = bot.say(peer, &format!(
                        "🗂 Розклав чати й канали за контекстом: <b>{n}</b>, джерел новин: <b>{news}</b>.\nПеревір, поправ галочки й іконки: <b>http://{}/chats</b>", bot.ctx.cfg.web_addr)).await;
                }
            }
            other => {
                tracing::warn!("first-run classification attempt {attempts} produced nothing ({:?})", other.as_ref().map(|o| o.is_some()));
                if attempts >= 3 {
                    bot.ctx.event("auto_classified", None, Some("gave up".into())).await;
                }
            }
        }
    }
}
