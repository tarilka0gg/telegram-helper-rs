//! SQLite schema. Table/column names match the Python original so an existing
//! `app.db` keeps working; `llm_usage` and `events` are new (analytics).

use rusqlite::Connection;

const TS: &str = "TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%S', 'now'))";

/// Tables and indexes (everything except the FTS index, which depends on what already exists).
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

/// Full-text index over messages. A database created by the Python original already has a
/// `messages_fts` (with an extra `sender_name` column) and its own `messages_fts_*` triggers; in that case
/// we keep them — adding ours as well would index every message twice.
fn ensure_fts(conn: &Connection) -> rusqlite::Result<()> {
    let exists = |kind: &str, name: &str| -> rusqlite::Result<bool> {
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type = ? AND name = ?")?.exists([kind, name])
    };
    let had_table = exists("table", "messages_fts")?;
    let python_triggers = exists("trigger", "messages_fts_ai")?;
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            text, transcript, extracted_text,
            content='messages', content_rowid='id'
        );",
    )?;
    if !python_triggers {
        conn.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
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
            END;",
        )?;
    }
    if !had_table {
        // Brand-new index over a database that already holds messages: index them.
        conn.execute_batch("INSERT INTO messages_fts(messages_fts) VALUES ('rebuild');")?;
    }
    Ok(())
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
    // Cheap and idempotent: also repairs a database whose FTS objects are missing.
    ensure_fts(conn)?;
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

    fn count(c: &Connection, sql: &str) -> i64 {
        c.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// A database as the Python original leaves it: no user_version, no category/mirror columns,
    /// its own FTS table (extra `sender_name` column) and `messages_fts_*` triggers, real data inside.
    fn python_original_db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, telegram_id BIGINT UNIQUE, created_at DATETIME);
             CREATE TABLE contacts (id INTEGER PRIMARY KEY, user_id INTEGER, peer_id BIGINT, peer_kind VARCHAR(16), is_bot BOOLEAN, is_archived BOOLEAN,
                is_news_source BOOLEAN, display_name VARCHAR(256), username VARCHAR(128), phone VARCHAR(32), style_profile TEXT, style_updated_at DATETIME, last_seen_message_id BIGINT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, user_id INTEGER, peer_id BIGINT, message_id BIGINT, sender_id BIGINT, sender_name VARCHAR(256),
                is_outgoing BOOLEAN, date DATETIME, kind VARCHAR(16), text TEXT, transcript TEXT, media_path TEXT, extracted_text TEXT, indexed_in_vector BOOLEAN,
                UNIQUE (user_id, peer_id, message_id));
             CREATE VIRTUAL TABLE messages_fts USING fts5(text, transcript, extracted_text, sender_name, content='messages', content_rowid='id', tokenize='unicode61 remove_diacritics 2');
             CREATE TRIGGER messages_fts_ai AFTER INSERT ON messages BEGIN
                INSERT INTO messages_fts(rowid, text, transcript, extracted_text, sender_name) VALUES (new.id, new.text, new.transcript, new.extracted_text, new.sender_name);
             END;
             CREATE TRIGGER messages_fts_ad AFTER DELETE ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, text, transcript, extracted_text, sender_name) VALUES ('delete', old.id, old.text, old.transcript, old.extracted_text, old.sender_name);
             END;
             CREATE TRIGGER messages_fts_au AFTER UPDATE ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, text, transcript, extracted_text, sender_name) VALUES ('delete', old.id, old.text, old.transcript, old.extracted_text, old.sender_name);
                INSERT INTO messages_fts(rowid, text, transcript, extracted_text, sender_name) VALUES (new.id, new.text, new.transcript, new.extracted_text, new.sender_name);
             END;
             INSERT INTO users(telegram_id, created_at) VALUES (7, '2026-01-01 00:00:00.123456');
             INSERT INTO contacts(user_id, peer_id, peer_kind, is_bot, is_archived, is_news_source, display_name) VALUES (1, 10, 'user', 0, 0, 0, 'Оля');
             INSERT INTO messages(user_id, peer_id, message_id, sender_name, is_outgoing, date, kind, text) VALUES (1, 10, 1, 'Оля', 0, '2026-01-01 10:00:00.500000', 'text', 'привіт, купи молоко');",
        )
        .unwrap();
        c
    }

    #[test]
    fn migrates_python_original_without_duplicating_fts() {
        let c = python_original_db();
        migrate(&c).unwrap();
        migrate(&c).unwrap(); // idempotent
        // data survived, new columns exist with sensible defaults
        assert_eq!(count(&c, "SELECT count(*) FROM messages"), 1);
        assert_eq!(count(&c, "SELECT mirror FROM contacts WHERE peer_id = 10"), 1);
        assert_eq!(count(&c, "SELECT count(*) FROM contacts WHERE category IS NULL"), 1);
        assert_eq!(count(&c, "PRAGMA user_version"), 2);
        // only the Python triggers exist: ours must not be added on top (would double-index)
        assert_eq!(count(&c, "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name IN ('messages_ai','messages_ad','messages_au')"), 0);
        assert_eq!(count(&c, "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'messages_fts_%'"), 3);
        // a new message through our repo layer is indexed exactly once
        crate::db::repo::ensure_user(&c, 7).unwrap();
        c.execute("INSERT INTO messages(user_id, peer_id, message_id, is_outgoing, date, kind, text) VALUES (1, 10, 2, 0, '2026-01-02 10:00:00', 'text', 'молоко закінчилось')", []).unwrap();
        assert_eq!(count(&c, "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'молоко'"), 2);
        let hits = crate::db::repo::search_messages(&c, 1, "молоко", 10).unwrap();
        assert_eq!(hits.len(), 2, "each message must be found once, not twice");
        // the microsecond timestamp written by Python still parses
        assert!(crate::db::repo::parse_ts(&c.query_row::<String, _, _>("SELECT date FROM messages WHERE message_id = 1", [], |r| r.get(0)).unwrap()).is_some());
    }

    #[test]
    fn existing_messages_are_indexed_when_the_index_is_new() {
        // messages exist but there is no FTS yet (e.g. DB from an older Rust build)
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(&format!("PRAGMA foreign_keys=OFF;{}", migration_v1())).unwrap();
        c.execute("INSERT INTO users(telegram_id) VALUES (1)", []).unwrap();
        c.execute("INSERT INTO messages(user_id, peer_id, message_id, date, text) VALUES (1, 5, 1, '2026-01-01 10:00:00', 'старе повідомлення про дзвінок')", []).unwrap();
        migrate(&c).unwrap();
        assert_eq!(count(&c, "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'дзвінок'"), 1);
    }

    #[test]
    fn migrate_survives_repeated_and_partial_runs() {
        let c = Connection::open_in_memory().unwrap();
        for _ in 0..3 {
            migrate(&c).unwrap();
        }
        c.execute_batch("PRAGMA user_version = 1; ALTER TABLE contacts DROP COLUMN mirror;").unwrap(); // half-migrated state
        migrate(&c).unwrap();
        assert_eq!(count(&c, "SELECT count(*) FROM pragma_table_info('contacts') WHERE name IN ('category','mirror')"), 2);
    }
}
