//! Persistence operations used by the Telegram side, schedulers and the LLM layer.
//! Everything is scoped by `user_id` (the internal `users.id`, not the Telegram id).

use rusqlite::{params, Connection, OptionalExtension, Result};

pub const TS_FMT: &str = "%Y-%m-%d %H:%M:%S";

pub fn fmt_ts(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format(TS_FMT).to_string()
}

pub fn ensure_user(c: &Connection, telegram_id: i64) -> Result<i64> {
    c.execute("INSERT OR IGNORE INTO users(telegram_id) VALUES (?)", [telegram_id])?;
    let id: i64 = c.query_row("SELECT id FROM users WHERE telegram_id = ?", [telegram_id], |r| r.get(0))?;
    c.execute("INSERT OR IGNORE INTO user_settings(user_id) VALUES (?)", [id])?;
    Ok(id)
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub auto_reply_enabled: bool,
    pub auto_reply_mode: String,
    pub auto_reply_text: String,
    pub auto_reply_cooldown_min: i64,
    pub llm_provider: String,
    pub use_heavy_model: bool,
    pub timezone: String,
    pub digest_enabled: bool,
    pub digest_time: String,
    pub reminders_enabled: bool,
    pub reminder_lead_hours: i64,
    pub reminder_overdue_enabled: bool,
    pub news_enabled: bool,
    pub news_window_hours: i64,
    pub news_digest_time: String,
    pub ignore_archived: bool,
}

pub fn settings(c: &Connection, user_id: i64) -> Result<Settings> {
    c.query_row(
        "SELECT auto_reply_enabled, auto_reply_mode, auto_reply_text, auto_reply_cooldown_min, llm_provider,
                use_heavy_model, timezone, digest_enabled, digest_time, reminders_enabled, reminder_lead_hours,
                reminder_overdue_enabled, news_enabled, news_window_hours, news_digest_time, ignore_archived
         FROM user_settings WHERE user_id = ?",
        [user_id],
        |r| {
            Ok(Settings {
                auto_reply_enabled: r.get(0)?,
                auto_reply_mode: r.get(1)?,
                auto_reply_text: r.get(2)?,
                auto_reply_cooldown_min: r.get(3)?,
                llm_provider: r.get(4)?,
                use_heavy_model: r.get(5)?,
                timezone: r.get(6)?,
                digest_enabled: r.get(7)?,
                digest_time: r.get(8)?,
                reminders_enabled: r.get(9)?,
                reminder_lead_hours: r.get(10)?,
                reminder_overdue_enabled: r.get(11)?,
                news_enabled: r.get(12)?,
                news_window_hours: r.get(13)?,
                news_digest_time: r.get(14)?,
                ignore_archived: r.get(15)?,
            })
        },
    )
}

/// Columns that `set_setting` may touch — a whitelist, since the name is spliced into SQL.
const SETTING_COLUMNS: &[&str] = &[
    "auto_reply_enabled", "auto_reply_mode", "auto_reply_text", "auto_reply_cooldown_min", "llm_provider",
    "use_heavy_model", "timezone", "digest_enabled", "digest_time", "reminders_enabled", "reminder_lead_hours",
    "reminder_overdue_enabled", "news_enabled", "news_window_hours", "news_digest_time", "ignore_archived",
];

pub fn set_setting(c: &Connection, user_id: i64, column: &str, value: rusqlite::types::Value) -> Result<bool> {
    if !SETTING_COLUMNS.contains(&column) {
        return Ok(false);
    }
    let n = c.execute(&format!("UPDATE user_settings SET {column} = ?1 WHERE user_id = ?2"), params![value, user_id])?;
    Ok(n > 0)
}

// ---- secrets (already encrypted by the caller) -----------------------------

pub fn save_session(c: &Connection, user_id: i64, api_id: i64, api_hash_enc: &str, session_enc: &str, phone: &str, label: Option<&str>) -> Result<()> {
    c.execute(
        "INSERT INTO telegram_sessions(user_id, api_id, api_hash_enc, session_string_enc, phone, account_label)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(user_id) DO UPDATE SET api_id = ?2, api_hash_enc = ?3, session_string_enc = ?4, phone = ?5, account_label = ?6",
        params![user_id, api_id, api_hash_enc, session_enc, phone, label],
    )?;
    Ok(())
}

pub fn update_session_blob(c: &Connection, user_id: i64, session_enc: &str) -> Result<()> {
    c.execute("UPDATE telegram_sessions SET session_string_enc = ?1 WHERE user_id = ?2", params![session_enc, user_id])?;
    Ok(())
}

pub fn load_session(c: &Connection, user_id: i64) -> Result<Option<(i64, String, String)>> {
    c.query_row(
        "SELECT api_id, api_hash_enc, session_string_enc FROM telegram_sessions WHERE user_id = ?",
        [user_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
}

pub fn delete_session(c: &Connection, user_id: i64) -> Result<()> {
    c.execute("DELETE FROM telegram_sessions WHERE user_id = ?", [user_id])?;
    Ok(())
}

pub fn set_api_key(c: &Connection, user_id: i64, provider: &str, key_enc: &str) -> Result<()> {
    c.execute(
        "INSERT INTO api_keys(user_id, provider, key_enc) VALUES (?1, ?2, ?3)
         ON CONFLICT(user_id, provider) DO UPDATE SET key_enc = ?3",
        params![user_id, provider, key_enc],
    )?;
    Ok(())
}

pub fn get_api_key(c: &Connection, user_id: i64, provider: &str) -> Result<Option<String>> {
    c.query_row("SELECT key_enc FROM api_keys WHERE user_id = ? AND provider = ?", params![user_id, provider], |r| r.get(0))
        .optional()
}

// ---- contacts & messages ---------------------------------------------------

#[derive(Debug, Clone)]
pub struct ContactRow {
    pub peer_id: i64,
    pub peer_kind: String,
    pub is_bot: bool,
    pub is_archived: bool,
    pub display_name: String,
    pub username: Option<String>,
}

pub fn upsert_contact(c: &Connection, user_id: i64, k: &ContactRow) -> Result<()> {
    c.execute(
        "INSERT INTO contacts(user_id, peer_id, peer_kind, is_bot, is_archived, display_name, username)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(user_id, peer_id) DO UPDATE SET peer_kind = ?3, is_bot = ?4, is_archived = ?5, display_name = ?6, username = ?7",
        params![user_id, k.peer_id, k.peer_kind, k.is_bot, k.is_archived, k.display_name, k.username],
    )?;
    Ok(())
}

pub fn list_contacts(c: &Connection, user_id: i64) -> Result<Vec<ContactRow>> {
    let mut st = c.prepare(
        "SELECT peer_id, peer_kind, is_bot, is_archived, display_name, username FROM contacts WHERE user_id = ? ORDER BY display_name",
    )?;
    let rows = st.query_map([user_id], |r| {
        Ok(ContactRow { peer_id: r.get(0)?, peer_kind: r.get(1)?, is_bot: r.get(2)?, is_archived: r.get(3)?, display_name: r.get(4)?, username: r.get(5)? })
    })?;
    rows.collect()
}

#[derive(Debug, Clone)]
pub struct MessageRow {
    pub peer_id: i64,
    pub message_id: i64,
    pub sender_id: Option<i64>,
    pub sender_name: Option<String>,
    pub is_outgoing: bool,
    pub date: String,
    pub kind: String,
    pub text: Option<String>,
}

/// Returns true if the row was new.
pub fn save_message(c: &Connection, user_id: i64, m: &MessageRow) -> Result<bool> {
    let n = c.execute(
        "INSERT INTO messages(user_id, peer_id, message_id, sender_id, sender_name, is_outgoing, date, kind, text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(user_id, peer_id, message_id) DO UPDATE SET text = excluded.text WHERE text IS NOT excluded.text",
        params![user_id, m.peer_id, m.message_id, m.sender_id, m.sender_name, m.is_outgoing, m.date, m.kind, m.text],
    )?;
    Ok(n > 0)
}

pub fn recent_messages(c: &Connection, user_id: i64, peer_id: i64, limit: i64) -> Result<Vec<MessageRow>> {
    let mut st = c.prepare(
        "SELECT peer_id, message_id, sender_id, sender_name, is_outgoing, date, kind, text FROM messages
         WHERE user_id = ? AND peer_id = ? ORDER BY date DESC, message_id DESC LIMIT ?",
    )?;
    let mut rows = st
        .query_map(params![user_id, peer_id, limit], |r| {
            Ok(MessageRow { peer_id: r.get(0)?, message_id: r.get(1)?, sender_id: r.get(2)?, sender_name: r.get(3)?,
                is_outgoing: r.get(4)?, date: r.get(5)?, kind: r.get(6)?, text: r.get(7)? })
        })?
        .collect::<Result<Vec<_>>>()?;
    rows.reverse(); // chronological
    Ok(rows)
}

/// FTS5 search. The user's text is turned into a safe query of quoted terms.
pub fn search_messages(c: &Connection, user_id: i64, query: &str, limit: i64) -> Result<Vec<MessageRow>> {
    let fts: String = query
        .split_whitespace()
        .map(|w| format!("\"{}\"", w.replace('"', "")))
        .filter(|w| w != "\"\"")
        .collect::<Vec<_>>()
        .join(" ");
    if fts.is_empty() {
        return Ok(vec![]);
    }
    let mut st = c.prepare(
        "SELECT m.peer_id, m.message_id, m.sender_id, m.sender_name, m.is_outgoing, m.date, m.kind, m.text
         FROM messages_fts f JOIN messages m ON m.id = f.rowid
         WHERE messages_fts MATCH ?1 AND m.user_id = ?2 ORDER BY m.date DESC LIMIT ?3",
    )?;
    let rows = st.query_map(params![fts, user_id, limit], |r| {
        Ok(MessageRow { peer_id: r.get(0)?, message_id: r.get(1)?, sender_id: r.get(2)?, sender_name: r.get(3)?,
            is_outgoing: r.get(4)?, date: r.get(5)?, kind: r.get(6)?, text: r.get(7)? })
    })?;
    rows.collect()
}

// ---- commitments -----------------------------------------------------------

pub fn add_commitment(c: &Connection, user_id: i64, peer_id: i64, peer_name: &str, direction: &str, text: &str, deadline: Option<&str>) -> Result<i64> {
    c.execute(
        "INSERT INTO commitments(user_id, peer_id, peer_name, direction, text, deadline_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![user_id, peer_id, peer_name, direction, text, deadline],
    )?;
    Ok(c.last_insert_rowid())
}

pub fn set_commitment_status(c: &Connection, user_id: i64, id: i64, status: &str) -> Result<bool> {
    Ok(c.execute("UPDATE commitments SET status = ?1 WHERE id = ?2 AND user_id = ?3", params![status, id, user_id])? > 0)
}

#[derive(Debug, Clone)]
pub struct CommitmentRow {
    pub id: i64,
    pub peer_id: i64,
    pub peer_name: String,
    pub direction: String,
    pub text: String,
    pub deadline_at: Option<String>,
    pub status: String,
    pub created_at: String,
}

pub fn open_commitments(c: &Connection, user_id: i64, direction: Option<&str>) -> Result<Vec<CommitmentRow>> {
    let mut st = c.prepare(
        "SELECT id, peer_id, COALESCE(peer_name, ''), direction, text, deadline_at, status, created_at FROM commitments
         WHERE user_id = ?1 AND status IN ('open', 'reminded', 'overdue') AND (?2 IS NULL OR direction = ?2)
         ORDER BY deadline_at IS NULL, deadline_at, id",
    )?;
    let rows = st.query_map(params![user_id, direction], |r| {
        Ok(CommitmentRow { id: r.get(0)?, peer_id: r.get(1)?, peer_name: r.get(2)?, direction: r.get(3)?, text: r.get(4)?, deadline_at: r.get(5)?, status: r.get(6)?, created_at: r.get(7)? })
    })?;
    rows.collect()
}

/// Chats whose latest incoming message (since `since`) has no outgoing reply after it: (peer_id, name, snippet).
pub fn waiting_for_reply(c: &Connection, user_id: i64, since: &str, limit: i64) -> Result<Vec<(i64, String, String)>> {
    let mut st = c.prepare(
        "SELECT m.peer_id, COALESCE(m.sender_name, k.display_name, CAST(m.peer_id AS TEXT)), substr(COALESCE(m.text, m.extracted_text, ''), 1, 200)
         FROM messages m LEFT JOIN contacts k ON k.user_id = m.user_id AND k.peer_id = m.peer_id
         WHERE m.user_id = ?1 AND m.is_outgoing = 0 AND m.date >= ?2
           AND m.date = (SELECT max(date) FROM messages x WHERE x.user_id = m.user_id AND x.peer_id = m.peer_id AND x.is_outgoing = 0)
           AND NOT EXISTS (SELECT 1 FROM messages o WHERE o.user_id = m.user_id AND o.peer_id = m.peer_id AND o.is_outgoing = 1 AND o.date > m.date)
         GROUP BY m.peer_id ORDER BY m.date DESC LIMIT ?3",
    )?;
    let rows = st.query_map(params![user_id, since, limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    rows.collect()
}

pub fn auto_replies_since(c: &Connection, user_id: i64, since: &str) -> Result<Vec<String>> {
    let mut st = c.prepare("SELECT COALESCE(peer_name, CAST(peer_id AS TEXT)) FROM auto_reply_logs WHERE user_id = ? AND created_at >= ?")?;
    let rows = st.query_map(params![user_id, since], |r| r.get(0))?;
    rows.collect()
}

/// Cancels open commitments whose text or peer name contains `query` (case-insensitive). Returns the texts.
pub fn cancel_commitments_matching(c: &Connection, user_id: i64, query: &str) -> Result<Vec<String>> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Ok(vec![]);
    }
    let hits: Vec<(i64, String)> = open_commitments(c, user_id, None)?
        .into_iter()
        .filter(|r| r.text.to_lowercase().contains(&q) || r.peer_name.to_lowercase().contains(&q))
        .map(|r| (r.id, r.text))
        .collect();
    for (id, _) in &hits {
        set_commitment_status(c, user_id, *id, "cancelled")?;
    }
    Ok(hits.into_iter().map(|h| h.1).collect())
}

// ---- pending actions (confirm-before-send) ---------------------------------

pub fn pending_add(c: &Connection, user_id: i64, kind: &str, payload: &str) -> Result<i64> {
    // Old unconfirmed drafts are worthless after a day.
    c.execute("DELETE FROM pending_actions WHERE user_id = ? AND created_at < datetime('now', '-1 day')", [user_id])?;
    c.execute("INSERT INTO pending_actions(user_id, kind, payload) VALUES (?1, ?2, ?3)", params![user_id, kind, payload])?;
    Ok(c.last_insert_rowid())
}

pub fn pending_get(c: &Connection, user_id: i64, id: i64) -> Result<Option<(String, String)>> {
    c.query_row("SELECT kind, payload FROM pending_actions WHERE id = ? AND user_id = ?", params![id, user_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()
}

pub fn pending_set_payload(c: &Connection, user_id: i64, id: i64, payload: &str) -> Result<()> {
    c.execute("UPDATE pending_actions SET payload = ?1 WHERE id = ?2 AND user_id = ?3", params![payload, id, user_id])?;
    Ok(())
}

/// Atomically claims a pending action: returns it once, then it is gone (no double send on double click).
pub fn pending_take(c: &Connection, user_id: i64, id: i64) -> Result<Option<(String, String)>> {
    let row = pending_get(c, user_id, id)?;
    if row.is_some() {
        c.execute("DELETE FROM pending_actions WHERE id = ? AND user_id = ?", params![id, user_id])?;
    }
    Ok(row)
}

// ---- news topics & sources -------------------------------------------------

pub fn add_news_topic(c: &Connection, user_id: i64, topic: &str, hours: i64) -> Result<()> {
    c.execute("INSERT INTO news_topics(user_id, topic, hours) VALUES (?1, ?2, ?3)", params![user_id, topic, hours])?;
    Ok(())
}

pub fn remove_news_topics(c: &Connection, user_id: i64, needle: &str) -> Result<usize> {
    c.execute("DELETE FROM news_topics WHERE user_id = ?1 AND lower(topic) LIKE '%' || lower(?2) || '%'", params![user_id, needle])
}

pub fn list_news_topics(c: &Connection, user_id: i64) -> Result<Vec<(String, i64)>> {
    let mut st = c.prepare("SELECT topic, hours FROM news_topics WHERE user_id = ? AND enabled = 1 ORDER BY id")?;
    let rows = st.query_map([user_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

pub fn set_news_source(c: &Connection, user_id: i64, peer_id: i64, on: bool) -> Result<()> {
    c.execute("UPDATE contacts SET is_news_source = ?1 WHERE user_id = ?2 AND peer_id = ?3", params![on, user_id, peer_id])?;
    Ok(())
}

/// Recent texts from channels flagged as news sources that mention `topic`.
pub fn news_messages(c: &Connection, user_id: i64, topic: &str, since: &str, limit: i64) -> Result<Vec<MessageRow>> {
    let mut st = c.prepare(
        "SELECT m.peer_id, m.message_id, m.sender_id, COALESCE(k.display_name, m.sender_name), m.is_outgoing, m.date, m.kind, m.text
         FROM messages m JOIN contacts k ON k.user_id = m.user_id AND k.peer_id = m.peer_id AND k.is_news_source = 1
         WHERE m.user_id = ?1 AND m.date >= ?2 AND m.text IS NOT NULL AND lower(m.text) LIKE '%' || lower(?3) || '%'
         ORDER BY m.date DESC LIMIT ?4",
    )?;
    let rows = st.query_map(params![user_id, since, topic, limit], |r| {
        Ok(MessageRow { peer_id: r.get(0)?, message_id: r.get(1)?, sender_id: r.get(2)?, sender_name: r.get(3)?, is_outgoing: r.get(4)?, date: r.get(5)?, kind: r.get(6)?, text: r.get(7)? })
    })?;
    rows.collect()
}

/// Chats that mention `query` most (global FTS), for "which chat was that?": (peer_id, name, hits).
pub fn chats_matching(c: &Connection, user_id: i64, query: &str, limit: i64) -> Result<Vec<(i64, String, i64)>> {
    let fts: String = query.split_whitespace().map(|w| format!("\"{}\"", w.replace('"', ""))).filter(|w| w != "\"\"").collect::<Vec<_>>().join(" OR ");
    if fts.is_empty() {
        return Ok(vec![]);
    }
    let mut st = c.prepare(
        "SELECT m.peer_id, COALESCE(k.display_name, CAST(m.peer_id AS TEXT)), count(*) AS n
         FROM messages_fts f JOIN messages m ON m.id = f.rowid LEFT JOIN contacts k ON k.user_id = m.user_id AND k.peer_id = m.peer_id
         WHERE messages_fts MATCH ?1 AND m.user_id = ?2 GROUP BY m.peer_id ORDER BY n DESC LIMIT ?3",
    )?;
    let rows = st.query_map(params![fts, user_id, limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    rows.collect()
}

/// Open "mine" commitments with a deadline, for the reminder loop: (id, peer_name, text, deadline, status).
pub fn commitments_with_deadline(c: &Connection, user_id: i64) -> Result<Vec<(i64, String, String, String, String)>> {
    let mut st = c.prepare(
        "SELECT id, COALESCE(peer_name, ''), text, deadline_at, status FROM commitments
         WHERE user_id = ? AND direction = 'mine' AND deadline_at IS NOT NULL AND status IN ('open', 'reminded')",
    )?;
    let rows = st.query_map([user_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
    rows.collect()
}

/// True if an event of this kind was logged at or after `since` (used to fire scheduled jobs once).
pub fn event_since(c: &Connection, kind: &str, since: &str, detail: Option<&str>) -> Result<bool> {
    let n: i64 = c.query_row(
        "SELECT count(*) FROM events WHERE kind = ?1 AND ts >= ?2 AND (?3 IS NULL OR detail = ?3)",
        params![kind, since, detail],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

// ---- auto-reply ------------------------------------------------------------

pub fn last_auto_reply_at(c: &Connection, user_id: i64, peer_id: i64) -> Result<Option<String>> {
    c.query_row("SELECT max(created_at) FROM auto_reply_logs WHERE user_id = ? AND peer_id = ?", params![user_id, peer_id], |r| r.get(0))
}

pub fn log_auto_reply(c: &Connection, user_id: i64, peer_id: i64, peer_name: &str, incoming: &str, reply: &str) -> Result<()> {
    c.execute(
        "INSERT INTO auto_reply_logs(user_id, peer_id, peer_name, incoming_text, reply_text) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![user_id, peer_id, peer_name, incoming, reply],
    )?;
    Ok(())
}

// ---- analytics feeds -------------------------------------------------------

pub fn log_event(c: &Connection, kind: &str, peer_id: Option<i64>, detail: Option<&str>) -> Result<()> {
    c.execute("INSERT INTO events(kind, peer_id, detail) VALUES (?1, ?2, ?3)", params![kind, peer_id, detail])?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn log_llm_usage(c: &Connection, provider: &str, model: &str, purpose: &str, prompt: i64, completion: i64, latency_ms: i64, ok: bool) -> Result<()> {
    c.execute(
        "INSERT INTO llm_usage(provider, model, purpose, prompt_tokens, completion_tokens, latency_ms, ok) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![provider, model, purpose, prompt, completion, latency_ms, ok],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::migrate;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c
    }

    #[test]
    fn users_settings_and_whitelist() {
        let c = db();
        let u = ensure_user(&c, 42).unwrap();
        assert_eq!(ensure_user(&c, 42).unwrap(), u);
        assert!(set_setting(&c, u, "digest_time", "07:30".to_string().into()).unwrap());
        assert!(!set_setting(&c, u, "digest_time = 'x'; DROP TABLE users; --", 1.into()).unwrap());
        let s = settings(&c, u).unwrap();
        assert_eq!(s.digest_time, "07:30");
        assert!(s.ignore_archived && !s.auto_reply_enabled);
    }

    #[test]
    fn messages_dedupe_search_and_history() {
        let c = db();
        let u = ensure_user(&c, 1).unwrap();
        let m = |id: i64, text: &str| MessageRow { peer_id: 5, message_id: id, sender_id: None, sender_name: Some("Оля".into()),
            is_outgoing: id % 2 == 0, date: format!("2026-01-01 10:0{id}:00"), kind: "text".into(), text: Some(text.into()) };
        assert!(save_message(&c, u, &m(1, "купи молоко")).unwrap());
        assert!(save_message(&c, u, &m(2, "ок, куплю")).unwrap());
        save_message(&c, u, &m(1, "купи молоко")).unwrap(); // duplicate
        assert_eq!(recent_messages(&c, u, 5, 10).unwrap().len(), 2);
        assert_eq!(recent_messages(&c, u, 5, 10).unwrap()[0].message_id, 1);
        // last incoming (id 1, 10:01) is answered by outgoing id 2 (10:02) -> nobody is waiting
        assert!(waiting_for_reply(&c, u, "2026-01-01 00:00:00", 10).unwrap().is_empty());
        save_message(&c, u, &MessageRow { message_id: 3, is_outgoing: false, date: "2026-01-01 10:05:00".into(), text: Some("ти де?".into()), ..m(3, "") }).unwrap();
        let w = waiting_for_reply(&c, u, "2026-01-01 00:00:00", 10).unwrap();
        assert_eq!((w.len(), w[0].2.as_str()), (1, "ти де?"));
        assert_eq!(search_messages(&c, u, "молоко", 10).unwrap().len(), 1);
        assert!(search_messages(&c, u, "\" OR 1", 10).is_ok()); // hostile input must not break FTS syntax
    }

    #[test]
    fn session_keys_commitments_autoreply() {
        let c = db();
        let u = ensure_user(&c, 1).unwrap();
        save_session(&c, u, 123, "h", "s", "+380", None).unwrap();
        assert_eq!(load_session(&c, u).unwrap().unwrap().0, 123);
        delete_session(&c, u).unwrap();
        assert!(load_session(&c, u).unwrap().is_none());
        set_api_key(&c, u, "openai", "e1").unwrap();
        set_api_key(&c, u, "openai", "e2").unwrap();
        assert_eq!(get_api_key(&c, u, "openai").unwrap().as_deref(), Some("e2"));
        let id = add_commitment(&c, u, 5, "Оля", "mine", "send doc", None).unwrap();
        assert!(set_commitment_status(&c, u, id, "done").unwrap());
        assert_eq!(open_commitments(&c, u, None).unwrap().len(), 0); // done is not open
        let id2 = add_commitment(&c, u, 5, "Оля", "theirs", "pay", Some("2026-02-01 10:00:00")).unwrap();
        assert_eq!(open_commitments(&c, u, Some("theirs")).unwrap()[0].id, id2);
        assert!(open_commitments(&c, u, Some("mine")).unwrap().is_empty());
        assert_eq!(cancel_commitments_matching(&c, u, "PAY").unwrap(), vec!["pay".to_string()]);
        let pid = pending_add(&c, u, "send_message", "{}").unwrap();
        assert!(pending_take(&c, u, pid).unwrap().is_some());
        assert!(pending_take(&c, u, pid).unwrap().is_none()); // single use
        add_news_topic(&c, u, "AI агенти", 24).unwrap();
        assert_eq!(list_news_topics(&c, u).unwrap().len(), 1);
        assert_eq!(remove_news_topics(&c, u, "ai").unwrap(), 1);
        assert!(last_auto_reply_at(&c, u, 5).unwrap().is_none());
        log_auto_reply(&c, u, 5, "Оля", "hi", "busy").unwrap();
        assert!(last_auto_reply_at(&c, u, 5).unwrap().is_some());
    }
}
