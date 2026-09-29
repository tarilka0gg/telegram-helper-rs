//! Read-only queries behind the localhost dashboard. Every function returns JSON
//! shaped exactly as `crates/server/web/app.js` expects.

use rusqlite::{params, Connection, Params, Result, Row};
use serde_json::{json, Value};

fn clamp(v: i64, lo: i64, hi: i64) -> i64 {
    v.clamp(lo, hi)
}

fn window(days: i64) -> String {
    format!("-{} days", clamp(days, 1, 365))
}

fn rows<P: Params>(c: &Connection, sql: &str, p: P, f: impl Fn(&Row) -> Result<Value>) -> Result<Value> {
    let mut st = c.prepare(sql)?;
    let out = st.query_map(p, |r| f(r))?.collect::<Result<Vec<_>>>()?;
    Ok(Value::Array(out))
}

fn one(c: &Connection, sql: &str, p: impl Params) -> Result<i64> {
    c.query_row(sql, p, |r| r.get(0))
}

pub fn overview(c: &Connection, userbot_connected: bool) -> Result<Value> {
    let d = "-1 days";
    Ok(json!({
        "messages_total": one(c, "SELECT count(*) FROM messages", [])?,
        "messages_24h": one(c, "SELECT count(*) FROM messages WHERE date >= datetime('now', ?)", [d])?,
        "contacts": one(c, "SELECT count(*) FROM contacts", [])?,
        "open_commitments": one(c, "SELECT count(*) FROM commitments WHERE status = 'open'", [])?,
        "autoreplies_24h": one(c, "SELECT count(*) FROM auto_reply_logs WHERE created_at >= datetime('now', ?)", [d])?,
        "llm_calls_24h": one(c, "SELECT count(*) FROM llm_usage WHERE ts >= datetime('now', ?)", [d])?,
        "llm_tokens_24h": one(c, "SELECT COALESCE(sum(prompt_tokens + completion_tokens), 0) FROM llm_usage WHERE ts >= datetime('now', ?)", [d])?,
        "llm_errors_24h": one(c, "SELECT count(*) FROM llm_usage WHERE ts >= datetime('now', ?) AND ok = 0", [d])?,
        "userbot_connected": userbot_connected,
    }))
}

pub fn messages_daily(c: &Connection, days: i64) -> Result<Value> {
    rows(
        c,
        "SELECT date(date) AS day, sum(is_outgoing = 0), sum(is_outgoing = 1) FROM messages
         WHERE date >= datetime('now', ?) GROUP BY day ORDER BY day",
        [window(days)],
        |r| Ok(json!({"day": r.get::<_, String>(0)?, "incoming": r.get::<_, i64>(1)?, "outgoing": r.get::<_, i64>(2)?})),
    )
}

pub fn messages_hourly(c: &Connection, days: i64) -> Result<Value> {
    rows(
        c,
        "SELECT CAST(strftime('%H', date) AS INTEGER) AS h, count(*) FROM messages
         WHERE date >= datetime('now', ?) GROUP BY h ORDER BY h",
        [window(days)],
        |r| Ok(json!({"hour": r.get::<_, i64>(0)?, "count": r.get::<_, i64>(1)?})),
    )
}

pub fn top_chats(c: &Connection, days: i64, limit: i64) -> Result<Value> {
    rows(
        c,
        "SELECT m.peer_id, COALESCE(MAX(k.display_name), CAST(m.peer_id AS TEXT)), count(*) AS n
         FROM messages m LEFT JOIN contacts k ON k.peer_id = m.peer_id AND k.user_id = m.user_id
         WHERE m.date >= datetime('now', ?) GROUP BY m.peer_id ORDER BY n DESC LIMIT ?",
        params![window(days), clamp(limit, 1, 500)],
        |r| Ok(json!({"peer_id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?, "count": r.get::<_, i64>(2)?})),
    )
}

pub fn llm_daily(c: &Connection, days: i64) -> Result<Value> {
    rows(
        c,
        "SELECT date(ts) AS day, count(*), sum(prompt_tokens), sum(completion_tokens), sum(ok = 0), avg(latency_ms)
         FROM llm_usage WHERE ts >= datetime('now', ?) GROUP BY day ORDER BY day",
        [window(days)],
        |r| {
            Ok(json!({"day": r.get::<_, String>(0)?, "calls": r.get::<_, i64>(1)?, "prompt_tokens": r.get::<_, i64>(2)?,
                "completion_tokens": r.get::<_, i64>(3)?, "errors": r.get::<_, i64>(4)?, "avg_latency_ms": r.get::<_, f64>(5)?}))
        },
    )
}

pub fn llm_by_purpose(c: &Connection, days: i64) -> Result<Value> {
    rows(
        c,
        "SELECT purpose, count(*) AS n, sum(prompt_tokens + completion_tokens), avg(latency_ms)
         FROM llm_usage WHERE ts >= datetime('now', ?) GROUP BY purpose ORDER BY n DESC",
        [window(days)],
        |r| {
            Ok(
                json!({"purpose": r.get::<_, String>(0)?, "calls": r.get::<_, i64>(1)?, "tokens": r.get::<_, i64>(2)?, "avg_latency_ms": r.get::<_, f64>(3)?}),
            )
        },
    )
}

pub fn autoreply_recent(c: &Connection, limit: i64) -> Result<Value> {
    rows(
        c,
        "SELECT created_at, COALESCE(peer_name, ''), COALESCE(incoming_text, ''), reply_text
         FROM auto_reply_logs ORDER BY created_at DESC, id DESC LIMIT ?",
        [clamp(limit, 1, 500)],
        |r| {
            Ok(
                json!({"created_at": r.get::<_, String>(0)?, "peer_name": r.get::<_, String>(1)?, "incoming_text": r.get::<_, String>(2)?, "reply_text": r.get::<_, String>(3)?}),
            )
        },
    )
}

pub fn commitments(c: &Connection, status: &str) -> Result<Value> {
    rows(
        c,
        "SELECT id, COALESCE(peer_name, ''), direction, text, deadline_at, status FROM commitments
         WHERE status = ? ORDER BY deadline_at IS NULL, deadline_at, id",
        [status],
        |r| {
            Ok(json!({"id": r.get::<_, i64>(0)?, "peer_name": r.get::<_, String>(1)?, "direction": r.get::<_, String>(2)?,
                "text": r.get::<_, String>(3)?, "deadline_at": r.get::<_, Option<String>>(4)?, "status": r.get::<_, String>(5)?}))
        },
    )
}

pub fn events(c: &Connection, limit: i64) -> Result<Value> {
    rows(c, "SELECT ts, kind, peer_id, detail FROM events ORDER BY ts DESC, id DESC LIMIT ?", [clamp(limit, 1, 500)], |r| {
        Ok(
            json!({"ts": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "peer_id": r.get::<_, Option<i64>>(2)?, "detail": r.get::<_, Option<String>>(3)?}),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::migrate;

    #[test]
    fn dashboard_queries() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute("INSERT INTO users(telegram_id) VALUES (1)", []).unwrap();
        c.execute("INSERT INTO contacts(user_id, peer_id, peer_kind, display_name) VALUES (1, 10, 'user', 'Olga')", []).unwrap();
        for (mid, out) in [(1, 0), (2, 0), (3, 1)] {
            c.execute(
                "INSERT INTO messages(user_id, peer_id, message_id, is_outgoing, date, text) VALUES (1, 10, ?, ?, datetime('now'), 'hi')",
                [mid, out],
            )
            .unwrap();
        }
        c.execute("INSERT INTO llm_usage(provider, model, purpose, prompt_tokens, completion_tokens, latency_ms, ok) VALUES ('openai', 'm', 'agent', 100, 50, 800, 1)", []).unwrap();
        c.execute("INSERT INTO llm_usage(provider, model, purpose, prompt_tokens, completion_tokens, latency_ms, ok) VALUES ('openai', 'm', 'agent', 10, 5, 200, 0)", []).unwrap();
        c.execute("INSERT INTO auto_reply_logs(user_id, peer_id, peer_name, reply_text) VALUES (1, 10, 'Olga', 'busy')", []).unwrap();

        let o = overview(&c, true).unwrap();
        assert_eq!((o["messages_total"].as_i64(), o["messages_24h"].as_i64(), o["contacts"].as_i64()), (Some(3), Some(3), Some(1)));
        assert_eq!(o["llm_tokens_24h"], 165);
        assert_eq!(o["llm_errors_24h"], 1);
        assert_eq!(o["autoreplies_24h"], 1);

        let d = messages_daily(&c, 7).unwrap();
        assert_eq!(d.as_array().unwrap().len(), 1);
        assert_eq!((d[0]["incoming"].as_i64(), d[0]["outgoing"].as_i64()), (Some(2), Some(1)));
        assert_eq!(top_chats(&c, 7, 5).unwrap()[0]["name"], "Olga");
        let p = llm_by_purpose(&c, 7).unwrap();
        assert_eq!((p[0]["calls"].as_i64(), p[0]["tokens"].as_i64()), (Some(2), Some(165)));
        assert_eq!(autoreply_recent(&c, 10).unwrap()[0]["incoming_text"], "");
        assert_eq!(messages_hourly(&c, 7).unwrap().as_array().unwrap().len(), 1);
        assert!(commitments(&c, "open").unwrap().as_array().unwrap().is_empty());
        assert!(events(&c, 10).unwrap().as_array().unwrap().is_empty());
        assert_eq!(llm_daily(&c, 7).unwrap()[0]["errors"], 1);
    }
}
