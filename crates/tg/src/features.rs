//! LLM-backed features: summaries, drafts, catch-up, commitments, digest, contact lookup.
//! Prompts are ported from the Python original (Russian on purpose: it is the owner's language).

use std::sync::Arc;

use anyhow::Result;
use chrono::{Duration, Utc};
use serde::Deserialize;
use tgh_core::{
    db::repo::{self, ContactRow, MessageRow},
    llm::{ChatMessage, LlmClient},
    sanitize::sanitize_html,
};

use crate::ctx::Ctx;

const SUMMARY_SYSTEM: &str = "Ты делаешь сжатое саммари переписки. Структура ответа:\n\
📝 <b>Главное</b> — 2–4 буллета.\n\
🎯 <b>Открытые вопросы / задачи</b> — что от меня ждут.\n\
📅 <b>Договорённости</b> — даты, встречи, обещания (с датой если есть).\n\
🌡 <b>Тон</b> — одной фразой.\n\
Используй HTML-разметку (<b>, <i>, <code>). Без markdown.";

const DRAFT_SYSTEM: &str = "Ты пишешь черновик ответа от моего имени. Только текст ответа, без префиксов и пояснений.\n\
Учитывай контекст последних сообщений и не повторяй уже сказанное.\n\
Если важная информация неоднозначна — задай короткий уточняющий вопрос вместо домысла.";

const CATCHUP_SYSTEM: &str = "Я давно не отвечал в этом чате. Сделай:\n\
1) <b>Где мы остановились</b> — 2–3 буллета о текущем состоянии.\n\
2) <b>Чего от меня ждут</b> — что нужно ответить или сделать.\n\
3) <b>Черновик ответа</b> — 1–4 предложения, в моём стиле.\n\
Используй HTML-разметку.";

const COMMITMENTS_SYSTEM: &str = "Ты выделяешь явные обязательства из переписки. Обязательство — конкретное обещание \
что-то сделать, прислать, ответить, прийти. Игнорируй риторические фразы.\n\n\
Возвращай JSON-массив (только массив, без обёрток):\n\
[\n  {\"direction\": \"mine\" | \"theirs\",\n   \"message_id\": <int или null>,\n   \"text\": \"обещание одной фразой\",\n   \"deadline\": \"ISO-8601 datetime UTC или null\"}\n]\n\
Если обязательств нет — пустой массив [].\n\
Не выдумывай дедлайны, если их нет в тексте.";

const DIGEST_SYSTEM: &str = "Ты делаешь короткий утренний дайджест по моей Telegram-активности.\n\
Структура (HTML):\n☀ <b>Доброе утро!</b>\n\n\
📨 <b>Ждут ответа</b> (если есть): кто и про что (1 строка на собеседника).\n\
🔥 <b>Мои горящие обещания</b>: те, что просрочены или ближайшие 24ч.\n\
💼 <b>Обещания мне</b>: что просрочено или скоро.\n\
🤖 <b>Авто-ответы</b>: сколько и кому, без подробностей.\n\
Если в каком-то блоке пусто — пропускай блок целиком.";

pub fn message_to_text(m: &MessageRow) -> String {
    let body = m.text.clone().unwrap_or_else(|| format!("[{}]", m.kind));
    let who = if m.is_outgoing { "Я" } else { m.sender_name.as_deref().unwrap_or("Они") };
    format!("[{}] {who}: {body}", m.date.get(..16).unwrap_or(&m.date))
}

pub fn transcript(msgs: &[MessageRow]) -> String {
    msgs.iter().map(message_to_text).collect::<Vec<_>>().join("\n")
}

pub async fn history(ctx: &Ctx, peer_id: i64, limit: i64) -> Result<Vec<MessageRow>> {
    let uid = ctx.user_id;
    ctx.db.call(move |c| repo::recent_messages(c, uid, peer_id, limit)).await
}

async fn ask(llm: &LlmClient, purpose: &str, system: &str, user: String, heavy: bool) -> Result<String> {
    Ok(sanitize_html(&llm.chat(purpose, &[ChatMessage::system(system), ChatMessage::user(user)], heavy).await?))
}

pub async fn summarize(llm: &LlmClient, heavy: bool, name: &str, msgs: &[MessageRow]) -> Result<String> {
    ask(llm, "summary", SUMMARY_SYSTEM, format!("Собеседник: {name}\n\nПереписка (последние {} сообщений):\n{}", msgs.len(), transcript(msgs)), heavy).await
}

pub async fn draft_reply(llm: &LlmClient, heavy: bool, name: &str, msgs: &[MessageRow], instruction: Option<&str>) -> Result<String> {
    let tail = instruction.map(|i| format!("Инструкция: {i}")).unwrap_or_else(|| "Напиши уместный ответ на последнее сообщение.".into());
    ask(llm, "draft", DRAFT_SYSTEM, format!("Собеседник: {name}\n\nКонтекст переписки:\n{}\n\n{tail}", transcript(msgs)), heavy).await
}

pub async fn catchup(llm: &LlmClient, heavy: bool, name: &str, msgs: &[MessageRow]) -> Result<String> {
    ask(llm, "catchup", CATCHUP_SYSTEM, format!("Собеседник: {name}\n\nПоследние сообщения:\n{}", transcript(msgs)), heavy).await
}

#[derive(Debug, Deserialize)]
struct RawCommitment {
    direction: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    deadline: Option<String>,
}

/// Parses the LLM's JSON array (optionally fenced); keeps only valid direction + non-empty text.
pub fn parse_commitments(raw: &str) -> Vec<(String, String, Option<String>)> {
    let mut s = raw.trim();
    if let Some(rest) = s.strip_prefix("```") {
        s = rest.trim_start().strip_prefix("json").unwrap_or(rest).trim();
        s = s.strip_suffix("```").unwrap_or(s).trim();
    }
    let items: Vec<RawCommitment> = serde_json::from_str(s).unwrap_or_default();
    items
        .into_iter()
        .filter(|c| matches!(c.direction.as_str(), "mine" | "theirs") && !c.text.trim().is_empty())
        .map(|c| (c.direction, c.text.trim().to_string(), c.deadline.as_deref().and_then(parse_deadline)))
        .collect()
}

/// ISO-8601 (with or without offset) -> "YYYY-MM-DD HH:MM:SS" in UTC.
pub fn parse_deadline(s: &str) -> Option<String> {
    let s = s.trim();
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").map(|n| n.and_utc()))
        .ok()?;
    Some(repo::fmt_ts(dt))
}

/// Extracts commitments from a chat and stores them. Returns what was saved.
pub async fn extract_commitments(ctx: &Arc<Ctx>, llm: &LlmClient, peer_id: i64, name: &str, msgs: &[MessageRow]) -> Result<Vec<(String, String, Option<String>)>> {
    if msgs.is_empty() {
        return Ok(vec![]);
    }
    let prompt = format!("Собеседник: {name}.\nПереписка:\n\n{}\n\nВыдели обязательства.", transcript(msgs));
    let raw = llm.chat("commitments", &[ChatMessage::system(COMMITMENTS_SYSTEM), ChatMessage::user(prompt)], false).await?;
    let items = parse_commitments(&raw);
    let (uid, name, saved) = (ctx.user_id, name.to_string(), items.clone());
    ctx.db
        .call(move |c| {
            for (dir, text, dl) in &saved {
                repo::add_commitment(c, uid, peer_id, &name, dir, text, dl.as_deref())?;
            }
            Ok(())
        })
        .await?;
    Ok(items)
}

/// Contacts ranked by name similarity to `query` (Zig fuzzy scorer); only plausible matches.
pub fn rank_contacts(contacts: &[ContactRow], query: &str) -> Vec<(ContactRow, u32)> {
    let q = query.trim().trim_start_matches('@');
    let mut out: Vec<(ContactRow, u32)> = contacts
        .iter()
        .filter(|c| !c.is_bot)
        .map(|c| {
            let by_name = tgh_native::fuzzy_score(q, &c.display_name);
            let by_user = c.username.as_deref().map_or(0, |u| tgh_native::fuzzy_score(q, u));
            (c.clone(), by_name.max(by_user))
        })
        .filter(|(_, s)| *s >= 60)
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.display_name.cmp(&b.0.display_name)));
    out.truncate(5);
    out
}

pub async fn find_contacts(ctx: &Ctx, query: &str) -> Result<Vec<(ContactRow, u32)>> {
    let uid = ctx.user_id;
    let all = ctx.db.call(move |c| repo::list_contacts(c, uid)).await?;
    Ok(rank_contacts(&all, query))
}

pub async fn build_digest(ctx: &Ctx) -> Result<String> {
    let Some(llm) = ctx.llm().await? else {
        return Ok("Не задан LLM-ключ — не могу собрать дайджест. Открой /settings.".into());
    };
    build_digest_with(ctx, &llm).await
}

pub async fn build_digest_with(ctx: &Ctx, llm: &LlmClient) -> Result<String> {
    let s = ctx.settings().await?;
    let uid = ctx.user_id;
    let since = repo::fmt_ts(Utc::now() - Duration::hours(14));
    let (waiting, mine, theirs, autos) = ctx
        .db
        .call(move |c| {
            Ok((
                repo::waiting_for_reply(c, uid, &since, 20)?,
                repo::open_commitments(c, uid, Some("mine"))?,
                repo::open_commitments(c, uid, Some("theirs"))?,
                repo::auto_replies_since(c, uid, &since)?,
            ))
        })
        .await?;
    let now = Utc::now();
    let hot = |v: Vec<repo::CommitmentRow>| -> Vec<String> {
        v.into_iter()
            .filter(|c| match &c.deadline_at {
                Some(d) => repo::parse_ts(d).is_none_or(|d| d.and_utc() <= now + Duration::hours(24)),
                None => repo::parse_ts(&c.created_at).is_some_and(|t| now - t.and_utc() > Duration::days(2)),
            })
            .take(20)
            .map(|c| format!("- {}: {} (до {})", c.peer_name, c.text, c.deadline_at.as_deref().unwrap_or("без срока")))
            .collect()
    };
    let mut parts = Vec::new();
    if !waiting.is_empty() {
        parts.push(format!("Ждут ответа:\n{}", waiting.iter().map(|(_, n, t)| format!("- {n}: {t}")).collect::<Vec<_>>().join("\n")));
    }
    let (mh, th) = (hot(mine), hot(theirs));
    if !mh.is_empty() {
        parts.push(format!("Мои горящие обещания:\n{}", mh.join("\n")));
    }
    if !th.is_empty() {
        parts.push(format!("Обещания мне (горящие):\n{}", th.join("\n")));
    }
    if !autos.is_empty() {
        let mut who = autos.clone();
        who.sort();
        who.dedup();
        parts.push(format!("Авто-ответов: {} (кому: {})", autos.len(), who.join(", ")));
    }
    if parts.is_empty() {
        return Ok("☀ Доброе утро! За ночь — тишина.".into());
    }
    ask(llm, "digest", DIGEST_SYSTEM, parts.join("\n\n"), s.use_heavy_model).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(id: i64, name: &str, user: Option<&str>) -> ContactRow {
        ContactRow { peer_id: id, peer_kind: "user".into(), is_bot: false, is_archived: false, display_name: name.into(), username: user.map(Into::into) }
    }

    #[test]
    fn commitments_parse_filters_and_normalizes() {
        let raw = "```json\n[{\"direction\":\"mine\",\"text\":\"скинути файл\",\"deadline\":\"2026-05-10T15:00:00Z\"},\
                   {\"direction\":\"nobody\",\"text\":\"x\"},{\"direction\":\"theirs\",\"text\":\"  \"},\
                   {\"direction\":\"theirs\",\"text\":\"оплатити\",\"deadline\":\"вчора\"}]\n```";
        let v = parse_commitments(raw);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].2.as_deref(), Some("2026-05-10 15:00:00"));
        assert_eq!(v[1].2, None);
        assert!(parse_commitments("not json").is_empty());
    }

    #[test]
    fn contact_ranking_uses_fuzzy_and_username() {
        let cs = vec![contact(1, "Олександр Петренко", Some("oleks")), contact(2, "Оля Іванова", None), contact(3, "Максим", Some("max_t"))];
        assert_eq!(rank_contacts(&cs, "Оля")[0].0.peer_id, 2);
        assert_eq!(rank_contacts(&cs, "@oleks")[0].0.peer_id, 1);
        assert!(rank_contacts(&cs, "Зіновій").is_empty());
    }

    #[test]
    fn transcript_format() {
        let m = MessageRow { peer_id: 1, message_id: 1, sender_id: None, sender_name: Some("Оля".into()), is_outgoing: false, date: "2026-01-01 10:00:00".into(), kind: "photo".into(), text: None };
        assert_eq!(message_to_text(&m), "[2026-01-01 10:00] Оля: [photo]");
    }

    #[test]
    fn deadlines_in_many_shapes() {
        assert_eq!(parse_deadline("2026-05-10T15:00:00Z").as_deref(), Some("2026-05-10 15:00:00"));
        assert_eq!(parse_deadline("2026-05-10T18:00:00+03:00").as_deref(), Some("2026-05-10 15:00:00")); // offset -> UTC
        assert_eq!(parse_deadline("2026-05-10T15:00:00").as_deref(), Some("2026-05-10 15:00:00")); // naive = UTC
        assert_eq!(parse_deadline("  2026-05-10T15:00:00Z \n").as_deref(), Some("2026-05-10 15:00:00"));
        assert_eq!(parse_deadline("2026-05-10T15:00:00.123Z").as_deref(), Some("2026-05-10 15:00:00"));
        for bad in ["", "завтра", "2026-13-40T99:99:99Z", "0000-00-00", "2026-05-10", "null", "1e999", &"9".repeat(1000)] {
            assert_eq!(parse_deadline(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn commitments_parser_survives_garbage() {
        for junk in ["", "[", "{}", "null", "[null]", "[1,2]", "[{}]", "[{\"direction\":1}]", "[{\"direction\":\"mine\",\"text\":null}]", "```json\n[\n```", &"[".repeat(10_000)] {
            let _ = parse_commitments(junk);
        }
        let v = parse_commitments("[{\"direction\":\"mine\",\"text\":\"  a\\tb  \",\"deadline\":null,\"message_id\":\"x\"}]");
        assert_eq!(v, vec![("mine".to_string(), "a\tb".to_string(), None)]);
    }

    #[test]
    fn contact_ranking_is_total_and_stable_for_odd_names() {
        let names = ["", " ", "🙂", "Оля", "Оля", "Ольга Іванова", "ОЛЯ", "o", "A".repeat(500).leak() as &str, "\u{0}", "İİİ"];
        let cs: Vec<ContactRow> = names.iter().enumerate().map(|(i, n)| ContactRow { peer_id: i as i64, peer_kind: "user".into(), is_bot: i == 9, is_archived: false, display_name: (*n).into(), username: if i % 2 == 0 { Some((*n).into()) } else { None } }).collect();
        for q in ["", "оля", "@", "@оля", "🙂", "İ", &"я".repeat(1000), "\u{0}"] {
            let r = rank_contacts(&cs, q);
            assert!(r.len() <= 5);
            assert!(r.windows(2).all(|w| w[0].1 >= w[1].1), "not sorted for {q:?}");
            assert!(r.iter().all(|(c, s)| *s >= 60 && !c.is_bot));
        }
        assert_eq!(rank_contacts(&cs, "Оля")[0].1, 100);
        // duplicates keep a deterministic order (same score -> by name)
        let r = rank_contacts(&cs, "Оля");
        assert!(r.iter().filter(|(c, _)| c.display_name == "Оля").count() == 2);
    }

    #[test]
    fn transcript_handles_multibyte_dates_and_missing_bodies() {
        let mut m = MessageRow { peer_id: 1, message_id: 1, sender_id: None, sender_name: None, is_outgoing: true, date: "2026".into(), kind: "photo".into(), text: None };
        assert_eq!(message_to_text(&m), "[2026] Я: [photo]"); // short date must not panic on slicing
        m.date = "🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂".into();
        let _ = message_to_text(&m); // 16-byte cut inside an emoji must not panic
    }

    #[tokio::test]
    async fn digest_prompt_contains_waiting_promises_and_autoreplies() {
        use crate::ctx::testkit::{ctx, fake_llm, reply, user_text};
        let ctx = ctx().await;
        let uid = ctx.user_id;
        let now = chrono::Utc::now();
        let recent = repo::fmt_ts(now - Duration::hours(1));
        ctx.db.call(move |c| {
            repo::upsert_contact(c, uid, &ContactRow { peer_id: 10, peer_kind: "user".into(), is_bot: false, is_archived: false, display_name: "Оля".into(), username: None })?;
            repo::upsert_contact(c, uid, &ContactRow { peer_id: 20, peer_kind: "channel".into(), is_bot: false, is_archived: false, display_name: "Канал".into(), username: None })?;
            let m = |peer: i64, id: i64, out: bool, text: &str| MessageRow { peer_id: peer, message_id: id, sender_id: None, sender_name: Some("Оля".into()), is_outgoing: out, date: recent.clone(), kind: "text".into(), text: Some(text.into()) };
            repo::save_message(c, uid, &m(10, 1, false, "ти завтра будеш?"))?;
            repo::save_message(c, uid, &m(20, 1, false, "новий пост каналу"))?; // must NOT appear as "waiting"
            repo::add_commitment(c, uid, 10, "Оля", "mine", "надіслати договір", Some("2000-01-01 00:00:00"))?; // overdue
            repo::add_commitment(c, uid, 10, "Оля", "mine", "далека справа", Some("2999-01-01 00:00:00"))?; // not hot
            repo::log_auto_reply(c, uid, 10, "Оля", "hi", "busy")?;
            Ok(())
        }).await.unwrap();
        // the fake model echoes what it was asked, so the test can inspect the prompt it received
        let base = fake_llm(|req| reply(&user_text(&req))).await;
        let llm = LlmClient::with_base(tgh_core::llm::Provider::Groq, "k".into(), ctx.db.clone(), &base);
        let out = build_digest_with(&ctx, &llm).await.unwrap();
        assert!(out.contains("Ждут ответа") && out.contains("ти завтра будеш?"), "{out}");
        assert!(!out.contains("новий пост каналу"), "channel post leaked into 'waiting': {out}");
        assert!(out.contains("надіслати договір") && !out.contains("далека справа"), "{out}");
        assert!(out.contains("Авто-ответов: 1"), "{out}");
    }

    #[tokio::test]
    async fn quiet_night_needs_no_llm_call() {
        use crate::ctx::testkit::{ctx, fake_llm};
        let ctx = ctx().await;
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = calls.clone();
        let base = fake_llm(move |_| { c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst); (500, serde_json::json!({})) }).await;
        let llm = LlmClient::with_base(tgh_core::llm::Provider::Groq, "k".into(), ctx.db.clone(), &base);
        assert!(build_digest_with(&ctx, &llm).await.unwrap().contains("тишина"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
