//! `tgh-server --demo`: serves the dashboard from a throw-away DB filled with synthetic data.
//! Needs no Telegram credentials, so the UI can be tried (and screenshotted) safely.

use tgh_core::db::{repo, Db};

pub async fn seed(db: &Db) -> anyhow::Result<()> {
    db.call(|c| {
        let uid = repo::ensure_user(c, 1)?;
        let names = ["Оля Іванова", "Максим", "Робоча група", "Мама", "Новини AI"];
        for (i, n) in names.iter().enumerate() {
            let kind = if i == 4 { "channel" } else { "user" };
            repo::upsert_contact(c, uid, &repo::ContactRow { peer_id: 100 + i as i64, peer_kind: kind.into(), is_bot: false, is_archived: false, display_name: (*n).into(), username: None })?;
        }
        // deterministic pseudo-random spread over 30 days
        let mut x: u64 = 42;
        let mut rnd = |m: u64| { x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (x >> 33) % m };
        for id in 0..900i64 {
            let (days, hour) = (rnd(30), 8 + rnd(15));
            c.execute(
                "INSERT INTO messages(user_id, peer_id, message_id, is_outgoing, date, text) VALUES (?1, ?2, ?3, ?4, datetime('now', ?5, ?6), 'demo')",
                rusqlite::params![uid, 100 + rnd(5) as i64, id, rnd(10) < 4, format!("-{days} days"), format!("-{} hours", (24 + hour as i64 - 12).rem_euclid(24))],
            )?;
        }
        for d in 0..14 {
            for (purpose, n) in [("agent", 3), ("summary", 1), ("auto_reply", 2)] {
                for k in 0..(n + rnd(3)) {
                    repo::log_llm_usage(c, "openai", "gpt-5-mini", purpose, 400 + rnd(900) as i64, 60 + rnd(300) as i64, 500 + rnd(1800) as i64, k % 17 != 16)?;
                    c.execute("UPDATE llm_usage SET ts = datetime('now', ?1) WHERE id = last_insert_rowid()", [format!("-{d} days")])?;
                }
            }
        }
        repo::log_auto_reply(c, uid, 100, "Оля Іванова", "Ти тут? Є хвилинка?", "Зараз не біля телефону, відповім, як зможу.")?;
        repo::log_auto_reply(c, uid, 101, "Максим", "скинь файл із вчорашнього", "Передам, зараз зайнятий.")?;
        repo::add_commitment(c, uid, 100, "Оля Іванова", "mine", "Надіслати договір", Some("2026-10-01 09:00:00"))?;
        repo::add_commitment(c, uid, 101, "Максим", "theirs", "Скинути реквізити", None)?;
        for k in ["userbot_online", "sync", "auto_reply", "digest_sent"] {
            repo::log_event(c, k, None, Some("demo"))?;
        }
        Ok(())
    })
    .await
}
