//! SQLite schema. Table/column names match the Python original so an existing
//! `app.db` keeps working; `llm_usage` and `events` are new (analytics).

use rusqlite::Connection;

const TS: &str = "TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%S', 'now'))";

pub fn migration_v1() -> String {
    format!(
        r#"
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    telegram_id BIGINT NOT NULL UNIQUE,
    created_at {TS}
);
CREATE TABLE IF NOT EXISTS user_settings (
    user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    auto_reply_enabled BOOLEAN NOT NULL DEFAULT 0,
    llm_provider TEXT NOT NULL DEFAULT 'openai',
    use_heavy_model BOOLEAN NOT NULL DEFAULT 0,
    timezone TEXT NOT NULL DEFAULT 'UTC',
    digest_time TEXT NOT NULL DEFAULT '09:00',
    digest_enabled BOOLEAN NOT NULL DEFAULT 0,
    transcription_mode TEXT NOT NULL DEFAULT 'local',
    auto_reply_cooldown_min INTEGER NOT NULL DEFAULT 30,
    auto_reply_mode TEXT NOT NULL DEFAULT 'static',
    auto_reply_text TEXT NOT NULL DEFAULT 'Сейчас не у телефона, отвечу как только смогу.',
    ignore_archived BOOLEAN NOT NULL DEFAULT 1,
    reminders_enabled BOOLEAN NOT NULL DEFAULT 0,
    reminder_lead_hours INTEGER NOT NULL DEFAULT 2,
    reminder_overdue_enabled BOOLEAN NOT NULL DEFAULT 1,
    news_enabled BOOLEAN NOT NULL DEFAULT 0,
    news_window_hours INTEGER NOT NULL DEFAULT 24,
    news_digest_time TEXT NOT NULL DEFAULT '08:00'
);
CREATE TABLE IF NOT EXISTS telegram_sessions (
    user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    api_id BIGINT NOT NULL,
    api_hash_enc TEXT NOT NULL,
    session_string_enc TEXT NOT NULL,
    phone TEXT NOT NULL,
    account_label TEXT,
    created_at {TS}
);
CREATE TABLE IF NOT EXISTS api_keys (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    key_enc TEXT NOT NULL,
    UNIQUE (user_id, provider)
);
CREATE TABLE IF NOT EXISTS contacts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    peer_id BIGINT NOT NULL,
    peer_kind TEXT NOT NULL,
    is_bot BOOLEAN NOT NULL DEFAULT 0,
    is_archived BOOLEAN NOT NULL DEFAULT 0,
    is_news_source BOOLEAN NOT NULL DEFAULT 0,
    display_name TEXT NOT NULL,
    username TEXT,
    phone TEXT,
    style_profile TEXT,
    style_updated_at TEXT,
    last_seen_message_id BIGINT,
    UNIQUE (user_id, peer_id)
);
CREATE INDEX IF NOT EXISTS ix_contacts_user_id ON contacts(user_id);
CREATE INDEX IF NOT EXISTS ix_contacts_peer_id ON contacts(peer_id);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    peer_id BIGINT NOT NULL,
    message_id BIGINT NOT NULL,
    sender_id BIGINT,
    sender_name TEXT,
    is_outgoing BOOLEAN NOT NULL DEFAULT 0,
    date TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'text',
    text TEXT,
    transcript TEXT,
    media_path TEXT,
    extracted_text TEXT,
    indexed_in_vector BOOLEAN NOT NULL DEFAULT 0,
    UNIQUE (user_id, peer_id, message_id)
);
CREATE INDEX IF NOT EXISTS ix_messages_user_peer_date ON messages(user_id, peer_id, date);
CREATE INDEX IF NOT EXISTS ix_messages_date ON messages(date);
CREATE TABLE IF NOT EXISTS commitments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    peer_id BIGINT NOT NULL,
    peer_name TEXT,
    message_id BIGINT,
    direction TEXT NOT NULL,
    text TEXT NOT NULL,
    deadline_at TEXT,
    status TEXT NOT NULL DEFAULT 'open',
    created_at {TS}
);
CREATE TABLE IF NOT EXISTS auto_reply_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    peer_id BIGINT NOT NULL,
    peer_name TEXT,
    incoming_text TEXT,
    reply_text TEXT NOT NULL,
    created_at {TS}
);
CREATE INDEX IF NOT EXISTS ix_auto_reply_logs_created ON auto_reply_logs(created_at);
CREATE TABLE IF NOT EXISTS index_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    peer_id BIGINT NOT NULL,
    last_indexed_message_id BIGINT NOT NULL DEFAULT 0,
    last_indexed_at {TS},
    UNIQUE (user_id, peer_id)
);
CREATE TABLE IF NOT EXISTS transcription_cache (
    file_id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    duration_seconds REAL,
    created_at {TS}
);
CREATE TABLE IF NOT EXISTS pending_actions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at {TS}
);
CREATE TABLE IF NOT EXISTS news_topics (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    topic TEXT NOT NULL,
    hours INTEGER NOT NULL DEFAULT 24,
    enabled BOOLEAN NOT NULL DEFAULT 1,
    created_at {TS}
);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    text, transcript, extracted_text,
    content='messages', content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, text, transcript, extracted_text)
    VALUES (new.id, new.text, new.transcript, new.extracted_text);
END;
CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, text, transcript, extracted_text)
    VALUES ('delete', old.id, old.text, old.transcript, old.extracted_text);
END;
CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, text, transcript, extracted_text)
    VALUES ('delete', old.id, old.text, old.transcript, old.extracted_text);
    INSERT INTO messages_fts(rowid, text, transcript, extracted_text)
    VALUES (new.id, new.text, new.transcript, new.extracted_text);
END;

-- analytics (new in the Rust rewrite)
CREATE TABLE IF NOT EXISTS llm_usage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts {TS},
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    purpose TEXT NOT NULL,
    prompt_tokens INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    latency_ms INTEGER NOT NULL DEFAULT 0,
    ok BOOLEAN NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS ix_llm_usage_ts ON llm_usage(ts);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts {TS},
    kind TEXT NOT NULL,
    peer_id BIGINT,
    detail TEXT
);
CREATE INDEX IF NOT EXISTS ix_events_ts ON events(ts);
"#
    )
}

/// v2: chat categories (auto-classified), per-chat mirror switch, and dedupe of news already delivered.
fn migration_v2(conn: &Connection) -> rusqlite::Result<()> {
    let has = |col: &str| -> rusqlite::Result<bool> {
        let mut st = conn.prepare("SELECT 1 FROM pragma_table_info('contacts') WHERE name = ?")?;
        st.exists([col])
    };
    // ALTER has no IF NOT EXISTS, so guard for DBs that already have the column.
    if !has("category")? {
        conn.execute_batch("ALTER TABLE contacts ADD COLUMN category TEXT;")?;
    }
    if !has("mirror")? {
        conn.execute_batch("ALTER TABLE contacts ADD COLUMN mirror BOOLEAN NOT NULL DEFAULT 1;")?;
    }
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS news_sent (
            user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            peer_id BIGINT NOT NULL,
            message_id BIGINT NOT NULL,
            sent_at {TS},
            PRIMARY KEY (user_id, peer_id, message_id)
        );"
    ))
}

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        // IF NOT EXISTS everywhere: safe on top of a DB created by the Python app.
        conn.execute_batch(&format!("BEGIN;{}PRAGMA user_version = 1;COMMIT;", migration_v1()))?;
    }
    if version < 2 {
        migration_v2(conn)?;
        conn.execute_batch("PRAGMA user_version = 2;")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_and_fts() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // idempotent
        conn.execute("INSERT INTO users(telegram_id) VALUES (1)", []).unwrap();
        conn.execute(
            "INSERT INTO messages(user_id, peer_id, message_id, date, text) VALUES (1, 10, 1, '2026-01-01 10:00:00', 'buy milk tomorrow')",
            [],
        )
        .unwrap();
        let n = |q: &str| -> i64 {
            conn.query_row(&format!("SELECT count(*) FROM messages_fts WHERE messages_fts MATCH '{q}'"), [], |r| r.get(0)).unwrap()
        };
        assert_eq!(n("milk"), 1);
        conn.execute("UPDATE messages SET text = 'buy bread' WHERE message_id = 1", []).unwrap();
        assert_eq!(n("milk"), 0);
        assert_eq!(n("bread"), 1);
    }
}
