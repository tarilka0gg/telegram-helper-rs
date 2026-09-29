//! Persistence operations used by the Telegram side, schedulers and the LLM layer.
//! Everything is scoped by `user_id` (the internal `users.id`, not the Telegram id).

use rusqlite::{params, Connection, OptionalExtension, Result, Row};

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

// ---- chats page, categories, news dedupe -----------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct ContactFull {
    pub peer_id: i64,
    pub name: String,
    pub username: Option<String>,
    pub kind: String,
    pub category: Option<String>,
    pub is_news_source: bool,
    pub mirror: bool,
    pub is_archived: bool,
    pub is_bot: bool,
    pub messages: i64,
}

pub fn list_contacts_full(c: &Connection, user_id: i64) -> Result<Vec<ContactFull>> {
    let mut st = c.prepare(
        "SELECT k.peer_id, k.display_name, k.username, k.peer_kind, k.category, k.is_news_source, k.mirror, k.is_archived, k.is_bot,
                (SELECT count(*) FROM messages m WHERE m.user_id = k.user_id AND m.peer_id = k.peer_id)
         FROM contacts k WHERE k.user_id = ? ORDER BY k.is_news_source DESC, k.display_name COLLATE NOCASE",
    )?;
    let rows = st.query_map([user_id], |r| {
        Ok(ContactFull { peer_id: r.get(0)?, name: r.get(1)?, username: r.get(2)?, kind: r.get(3)?, category: r.get(4)?, is_news_source: r.get(5)?, mirror: r.get(6)?, is_archived: r.get(7)?, is_bot: r.get(8)?, messages: r.get(9)? })
    })?;
    rows.collect()
}

/// Partial update from the web UI; only the given fields change. `category` is length-limited.
pub fn update_contact_flags(c: &Connection, user_id: i64, peer_id: i64, news_source: Option<bool>, mirror: Option<bool>, category: Option<&str>) -> Result<bool> {
    let n = c.execute(
        "UPDATE contacts SET is_news_source = COALESCE(?1, is_news_source), mirror = COALESCE(?2, mirror), category = COALESCE(?3, category)
         WHERE user_id = ?4 AND peer_id = ?5",
        params![news_source, mirror, category.map(|s| s.chars().take(32).collect::<String>()), user_id, peer_id],
    )?;
    Ok(n > 0)
}

/// Unknown chats are mirrored (the default), so a new conversation is never silently dropped.
pub fn mirror_enabled(c: &Connection, user_id: i64, peer_id: i64) -> Result<bool> {
    Ok(c.query_row("SELECT mirror FROM contacts WHERE user_id = ? AND peer_id = ?", params![user_id, peer_id], |r| r.get(0)).optional()?.unwrap_or(true))
}

/// News sources: (peer_id, kind, name).
pub fn news_sources(c: &Connection, user_id: i64) -> Result<Vec<(i64, String, String)>> {
    let mut st = c.prepare("SELECT peer_id, peer_kind, display_name FROM contacts WHERE user_id = ? AND is_news_source = 1 AND mirror = 1 ORDER BY display_name")?;
    let rows = st.query_map([user_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    rows.collect()
}

const NEWS_FROM: &str = "FROM messages m
    JOIN contacts k ON k.user_id = m.user_id AND k.peer_id = m.peer_id AND k.is_news_source = 1 AND k.mirror = 1
    WHERE m.user_id = ?1 AND m.text IS NOT NULL AND trim(m.text) <> ''
      AND (?3 IS NULL OR lower(m.text) LIKE '%' || lower(?3) || '%')
      AND NOT EXISTS (SELECT 1 FROM news_sent s WHERE s.user_id = m.user_id AND s.peer_id = m.peer_id AND s.message_id = m.message_id)";

fn news_row(r: &Row) -> Result<MessageRow> {
    Ok(MessageRow { peer_id: r.get(0)?, message_id: r.get(1)?, sender_id: r.get(2)?, sender_name: r.get(3)?, is_outgoing: r.get(4)?, date: r.get(5)?, kind: r.get(6)?, text: r.get(7)? })
}

/// Posts from news sources since `since` that were never included in a digest.
pub fn news_unsent(c: &Connection, user_id: i64, since: &str, topic: Option<&str>, limit: i64) -> Result<Vec<MessageRow>> {
    let sql = format!("SELECT m.peer_id, m.message_id, m.sender_id, k.display_name, m.is_outgoing, m.date, m.kind, m.text {NEWS_FROM} AND m.date >= ?2 ORDER BY m.date DESC LIMIT ?4");
    let mut st = c.prepare(&sql)?;
    let rows = st.query_map(params![user_id, since, topic, limit], news_row)?;
    rows.collect()
}

/// Fallback when nothing is new today: the latest unsent post(s) of each source, whatever their age.
pub fn news_latest_unsent(c: &Connection, user_id: i64, topic: Option<&str>, per_source: i64) -> Result<Vec<MessageRow>> {
    let sql = format!(
        "SELECT peer_id, message_id, sender_id, name, is_outgoing, date, kind, text FROM (
            SELECT m.peer_id AS peer_id, m.message_id AS message_id, m.sender_id AS sender_id, k.display_name AS name,
                   m.is_outgoing AS is_outgoing, m.date AS date, m.kind AS kind, m.text AS text,
                   ROW_NUMBER() OVER (PARTITION BY m.peer_id ORDER BY m.date DESC) AS rn {NEWS_FROM}
         ) WHERE rn <= ?2 ORDER BY date DESC"
    );
    let mut st = c.prepare(&sql)?;
    let rows = st.query_map(params![user_id, per_source, topic], news_row)?;
    rows.collect()
}

pub fn mark_news_sent(c: &Connection, user_id: i64, posts: &[(i64, i64)]) -> Result<()> {
    for (peer, msg) in posts {
        c.execute("INSERT OR IGNORE INTO news_sent(user_id, peer_id, message_id) VALUES (?1, ?2, ?3)", params![user_id, peer, msg])?;
    }
    Ok(())
}

/// Rows for the LLM classifier: (peer_id, name, username, kind, up to 3 recent text snippets).
pub fn contacts_for_classification(c: &Connection, user_id: i64, limit: i64) -> Result<Vec<(i64, String, Option<String>, String, Vec<String>)>> {
    let mut st = c.prepare("SELECT peer_id, display_name, username, peer_kind FROM contacts WHERE user_id = ? AND is_bot = 0 AND category IS NULL ORDER BY display_name LIMIT ?")?;
    let base = st.query_map(params![user_id, limit], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?)))?.collect::<Result<Vec<_>>>()?;
    let mut snip = c.prepare("SELECT substr(text, 1, 120) FROM messages WHERE user_id = ?1 AND peer_id = ?2 AND text IS NOT NULL AND trim(text) <> '' ORDER BY date DESC LIMIT 3")?;
    let mut out = Vec::with_capacity(base.len());
    for (id, name, user, kind) in base {
        let s = snip.query_map(params![user_id, id], |r| r.get(0))?.collect::<Result<Vec<String>>>()?;
        out.push((id, name, user, kind, s));
    }
    Ok(out)
}

/// Stores categories from the classifier; channels judged "news" become news sources.
/// Existing manual choices are never overridden: only rows without a category are touched.
pub fn apply_classification(c: &Connection, user_id: i64, items: &[(i64, String, bool)]) -> Result<usize> {
    let mut n = 0;
    for (peer_id, category, news) in items {
        n += c.execute(
            "UPDATE contacts SET category = ?1, is_news_source = CASE WHEN ?2 AND peer_kind = 'channel' THEN 1 ELSE is_news_source END
             WHERE user_id = ?3 AND peer_id = ?4 AND category IS NULL",
            params![category, news, user_id, peer_id],
        )?;
    }
    Ok(n)
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

    #[test]
    fn news_dedupe_fallback_and_classification() {
        let c = db();
        let u = ensure_user(&c, 1).unwrap();
        let ch = |id: i64, name: &str, kind: &str| ContactRow { peer_id: id, peer_kind: kind.into(), is_bot: false, is_archived: false, display_name: name.into(), username: None };
        upsert_contact(&c, u, &ch(10, "Tech News", "channel")).unwrap();
        upsert_contact(&c, u, &ch(11, "Family", "chat")).unwrap();
        let post = |peer: i64, id: i64, date: &str, text: &str| MessageRow { peer_id: peer, message_id: id, sender_id: None, sender_name: None, is_outgoing: false, date: date.into(), kind: "text".into(), text: Some(text.into()) };
        save_message(&c, u, &post(10, 1, "2020-01-01 10:00:00", "old rust news")).unwrap();
        save_message(&c, u, &post(10, 2, "2020-01-02 10:00:00", "older ai news")).unwrap();
        save_message(&c, u, &post(11, 1, "2020-01-03 10:00:00", "dinner?")).unwrap();

        // nothing is a news source yet
        assert!(news_latest_unsent(&c, u, None, 1).unwrap().is_empty());
        // classifier: channel -> news source, group -> only a category; manual choices survive
        let rows = contacts_for_classification(&c, u, 50).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(apply_classification(&c, u, &[(10, "news".into(), true), (11, "family".into(), true)]).unwrap(), 2);
        assert_eq!(apply_classification(&c, u, &[(10, "other".into(), false)]).unwrap(), 0); // already classified
        assert_eq!(news_sources(&c, u).unwrap().len(), 1); // the group was not turned into a source

        // nothing in the last day -> fallback returns the single latest post per source
        assert!(news_unsent(&c, u, "2030-01-01 00:00:00", None, 10).unwrap().is_empty());
        let latest = news_latest_unsent(&c, u, None, 1).unwrap();
        assert_eq!((latest.len(), latest[0].message_id), (1, 2));
        assert_eq!(news_latest_unsent(&c, u, Some("RUST"), 5).unwrap().len(), 1);

        // once delivered, never again
        mark_news_sent(&c, u, &[(10, 2)]).unwrap();
        let next = news_latest_unsent(&c, u, None, 1).unwrap();
        assert_eq!(next[0].message_id, 1);
        mark_news_sent(&c, u, &[(10, 1)]).unwrap();
        assert!(news_latest_unsent(&c, u, None, 3).unwrap().is_empty());

        // web toggles
        assert!(update_contact_flags(&c, u, 10, Some(false), Some(false), None).unwrap());
        assert!(!mirror_enabled(&c, u, 10).unwrap());
        assert!(mirror_enabled(&c, u, 999).unwrap()); // unknown chat: mirrored by default
        let full = list_contacts_full(&c, u).unwrap();
        assert_eq!(full.iter().find(|k| k.peer_id == 10).map(|k| (k.is_news_source, k.messages)), Some((false, 2)));
    }
}
