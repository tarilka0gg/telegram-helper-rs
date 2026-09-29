//! Background jobs: morning digest, news digests, deadline reminders, hourly dialog sync.
//! Each job checks "already fired?" against the `events` table, so restarts never double-send.

use std::{sync::Arc, time::Duration};

use chrono::{Timelike, Utc};
use grammers_client::session::types::PeerId;
use tgh_core::db::repo;

use crate::{bot::{esc, Bot}, features, userbot};

const TICK: Duration = Duration::from_secs(60);

pub fn spawn_all(bot: Arc<Bot>) {
    tokio::spawn(digest_loop(bot.clone()));
    tokio::spawn(news_loop(bot.clone()));
    tokio::spawn(reminders_loop(bot.clone()));
    tokio::spawn(classify_loop(bot.clone()));
    tokio::spawn(prune_loop(bot.clone()));
    tokio::spawn(sync_loop(bot));
}

async fn owner(bot: &Bot) -> Option<grammers_client::session::types::PeerRef> {
    // The reference learned from the owner's own messages carries the access hash; the ambient one is a last resort.
    if let Some(r) = *bot.owner_ref.lock().await {
        return Some(r);
    }
    PeerId::user(bot.ctx.cfg.owner_telegram_id).map(PeerId::to_ambient_ref)
}

/// `HH:MM` now in the owner's time zone plus the start of the local day, expressed in UTC.
/// Minutes since midnight for an `HH:MM` string (None if malformed).
fn minutes(hhmm: &str) -> Option<i64> {
    let (h, m) = hhmm.split_once(':')?;
    Some(h.parse::<i64>().ok()? * 60 + m.parse::<i64>().ok()?)
}

/// A daily job fires once, at its time or shortly after (a restart at 09:03 still delivers the 09:00
/// digest) — but not hours later, which would spam the moment a job is switched on in the afternoon.
const GRACE_MIN: i64 = 90;

fn due(now_hhmm: &str, at_hhmm: &str) -> bool {
    match (minutes(now_hhmm), minutes(at_hhmm)) {
        (Some(now), Some(at)) => now >= at && now - at <= GRACE_MIN,
        _ => false,
    }
}

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
        if !s.digest_enabled || !due(&hhmm, &s.digest_time) || fired_today(&bot, "digest_sent", day_start, None).await {
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
        if !s.news_enabled || !due(&hhmm, &s.news_digest_time) || fired_today(&bot, "news_sent", day_start, None).await {
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

/// What to tell the owner about a deadline `left` from now: `(label, new status)`, or nothing yet.
/// `open` -> "soon" once inside the lead window; past the deadline -> "overdue" once (status then leaves the reminder set).
fn reminder_due(left: chrono::Duration, status: &str, lead_hours: i64, overdue_enabled: bool) -> Option<(&'static str, &'static str)> {
    if left < chrono::Duration::zero() {
        return overdue_enabled.then_some(("⚠ Прострочено", "overdue"));
    }
    (status == "open" && left <= chrono::Duration::hours(lead_hours)).then_some(("⏰ Скоро", "reminded"))
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
            let Some(dl) = repo::parse_ts(&deadline) else { continue };
            let Some((label, new_status)) = reminder_due(dl - now, &status, s.reminder_lead_hours, s.reminder_overdue_enabled) else { continue };
            let who = if who.is_empty() { String::new() } else { format!(" ({})", esc(&who)) };
            let sent = bot.say(peer, &format!("{label}: {}{who}\nдо {deadline} UTC", esc(&text))).await;
            if sent.is_ok() {
                let st = new_status.to_string();
                let _ = bot.ctx.db.call(move |c| repo::set_commitment_status(c, uid, id, &st)).await;
            }
        }
    }
}

/// Once a day: trim old analytics rows.
async fn prune_loop(bot: Arc<Bot>) {
    loop {
        match bot.ctx.db.call(repo::prune_old).await {
            Ok(n) if n > 0 => tracing::info!("pruned {n} old analytics rows"),
            Ok(_) => {}
            Err(e) => tracing::warn!("prune failed: {e:#}"),
        }
        tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_jobs_fire_in_a_window_not_forever() {
        assert!(due("09:00", "09:00"));
        assert!(due("09:03", "09:00")); // restart shortly after the scheduled time
        assert!(due("10:30", "09:00")); // exactly at the grace limit
        assert!(!due("10:31", "09:00")); // enabled hours later: do not spam
        assert!(!due("08:59", "09:00")); // not yet
        assert!(!due("23:59", "00:05"));
        assert!(!due("09:00", "garbage") && !due("nope", "09:00"));
    }

    #[test]
    fn reminder_rules() {
        use chrono::Duration as D;
        // outside the lead window: nothing; inside: "soon" exactly once (status must be open)
        assert_eq!(reminder_due(D::hours(5), "open", 2, true), None);
        assert_eq!(reminder_due(D::hours(2), "open", 2, true), Some(("⏰ Скоро", "reminded")));
        assert_eq!(reminder_due(D::minutes(1), "open", 2, true), Some(("⏰ Скоро", "reminded")));
        assert_eq!(reminder_due(D::minutes(1), "reminded", 2, true), None); // already warned
        // overdue fires regardless of the earlier "reminded", and only if enabled
        assert_eq!(reminder_due(D::minutes(-1), "open", 2, true), Some(("⚠ Прострочено", "overdue")));
        assert_eq!(reminder_due(D::days(-9), "reminded", 2, true), Some(("⚠ Прострочено", "overdue")));
        assert_eq!(reminder_due(D::minutes(-1), "open", 2, false), None);
        // exactly at the deadline counts as still upcoming (not yet overdue)
        assert_eq!(reminder_due(D::zero(), "open", 2, true), Some(("⏰ Скоро", "reminded")));
    }

    #[test]
    fn local_clock_handles_bad_zone_and_dst() {
        let (hhmm, day_start) = local_clock("Not/AZone"); // falls back to UTC instead of panicking
        assert_eq!(hhmm.len(), 5);
        assert_eq!(day_start.len(), 19);
        let (_, kyiv_start) = local_clock("Europe/Kyiv");
        assert!(repo::parse_ts(&kyiv_start).is_some());
    }
}
