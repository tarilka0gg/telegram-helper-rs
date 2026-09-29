//! Live self-test run inside the server (`POST /api/selftest`). It exercises the real Telegram
//! session, the LLM chain and the bot end to end. Only harmless, read-only bot commands are sent,
//! from the owner's own account to the owner's own bot.

use std::{sync::Arc, time::Duration};

use anyhow::{bail, Result};
use serde::Serialize;
use tgh_core::{db::repo, llm::ChatMessage};

use crate::{manager::Manager, userbot};

#[derive(Serialize)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

async fn check(name: &'static str, fut: impl std::future::Future<Output = Result<String>>) -> Check {
    match fut.await {
        Ok(detail) => Check { name, ok: true, detail },
        Err(e) => Check { name, ok: false, detail: format!("{e:#}").chars().take(300).collect() },
    }
}

pub async fn run(mgr: &Arc<Manager>) -> Vec<Check> {
    let ctx = mgr.ctx().clone();
    let mut out = Vec::new();

    out.push(check("database integrity", async {
        let (integrity, fk, fts): (String, i64, String) = ctx
            .db
            .call(|c| {
                let integrity: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
                let fk: i64 = c.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0))?;
                let fts = match c.execute_batch("INSERT INTO messages_fts(messages_fts) VALUES ('integrity-check');") {
                    Ok(()) => "ok".to_string(),
                    Err(e) => e.to_string(),
                };
                Ok((integrity, fk, fts))
            })
            .await?;
        if integrity != "ok" || fk != 0 || fts != "ok" {
            bail!("integrity={integrity} fk_violations={fk} fts={fts}");
        }
        Ok("sqlite integrity, foreign keys and FTS index are consistent".into())
    }).await);

    out.push(check("secrets round-trip", async {
        let probe = ctx.crypto.encrypt("selftest");
        if ctx.crypto.decrypt(&probe)? != "selftest" {
            bail!("decrypt mismatch");
        }
        let uid = ctx.user_id;
        let n = ctx.db.call(move |c| Ok(repo::get_api_key(c, uid, "gemini")?.is_some() as i64 + repo::get_api_key(c, uid, "groq")?.is_some() as i64 + repo::get_api_key(c, uid, "zai")?.is_some() as i64 + repo::get_api_key(c, uid, "openai")?.is_some() as i64)).await?;
        Ok(format!("Fernet ok; {n} provider key(s) stored and decryptable"))
    }).await);

    out.push(check("llm chain", async {
        let Some(llm) = ctx.llm().await? else { bail!("no LLM key stored") };
        let answer = llm.chat("selftest", &[ChatMessage::user("Reply with exactly the single word: OK")], false).await?;
        let uid_provider: String = ctx.db.call(|c| c.query_row("SELECT provider || '/' || model FROM llm_usage WHERE ok = 1 ORDER BY id DESC LIMIT 1", [], |r| r.get(0))).await?;
        if !answer.to_uppercase().contains("OK") {
            bail!("unexpected answer {answer:?} from {uid_provider}");
        }
        Ok(format!("answered by {uid_provider}"))
    }).await);

    let client = mgr.client().await;
    out.push(check("telegram userbot session", async {
        let Some(c) = &client else { bail!("userbot is not logged in") };
        let me = c.get_me().await.map_err(|e| anyhow::anyhow!("get_me: {e}"))?;
        Ok(format!("connected as {}", me.full_name()))
    }).await);

    out.push(check("peer references for news sources", async {
        let uid = ctx.user_id;
        let sources = ctx.db.call(move |c| repo::news_sources(c, uid)).await?;
        if sources.is_empty() {
            return Ok("no news sources configured (nothing to check)".into());
        }
        let mut missing = Vec::new();
        for (id, kind, name) in &sources {
            if mgr.peer_ref(kind, *id).await.is_none() {
                missing.push(name.clone());
            }
        }
        if !missing.is_empty() {
            bail!("{} of {} not in the session cache: {}", missing.len(), sources.len(), missing.join(", "));
        }
        Ok(format!("all {} sources resolve to usable peer refs", sources.len()))
    }).await);

    out.push(check("live fetch (no DB write)", async {
        let Some(c) = &client else { bail!("userbot is not logged in") };
        let uid = ctx.user_id;
        let sources = ctx.db.call(move |cn| repo::news_sources(cn, uid)).await?;
        let Some((id, kind, name)) = sources.first() else { return Ok("skipped: no sources".into()) };
        let before: i64 = ctx.db.call(|cn| cn.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))).await?;
        let Some(pref) = mgr.peer_ref(kind, *id).await else { bail!("no peer ref for {name}") };
        let msgs = userbot::fetch_recent(c, pref, *id, 3).await?;
        let after: i64 = ctx.db.call(|cn| cn.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))).await?;
        if after < before {
            bail!("message count went down ({before} -> {after})");
        }
        Ok(format!("fetched {} message(s) from '{name}' straight from Telegram", msgs.len()))
    }).await);

    // End to end: the owner's account messages the owner's bot; the bot must answer. Read-only commands only
    // (no /send, no /news: those have side effects for other people or mark posts as delivered).
    let scenarios: [(&'static str, &str, &str); 6] = [
        ("bot e2e: /status", "/status", "Userbot:"),
        ("bot e2e: /help", "/help", "TelegramHelper"),
        ("bot e2e: /settings", "/settings", "Налаштування"),
        ("bot e2e: /topics", "/topics", "новин"),
        // the needle is echoed back if a previous run left it in the DB (the mirror stores our own probe), else "nothing found"
        ("bot e2e: /search", "/search selftest-needle-xyz", "needle|Нічого не знайшов"),
        ("bot e2e: free text -> agent -> LLM", "Відповідай одним коротким реченням: скільки буде 2+2? Це тест.", "4"),
    ];
    for (name, cmd, expect) in scenarios {
        out.push(check(name, async {
            let Some(c) = &client else { bail!("userbot is not logged in") };
            let Some(bot) = ctx.bot_username.get() else { bail!("bot username unknown") };
            let peer = c.resolve_username(bot).await.map_err(|e| anyhow::anyhow!("resolve @{bot}: {e}"))?.ok_or_else(|| anyhow::anyhow!("@{bot} not found"))?;
            let bot_id = peer.id().bare_id().ok_or_else(|| anyhow::anyhow!("no bot id"))?;
            let since = repo::fmt_ts(chrono::Utc::now() - chrono::Duration::seconds(2));
            c.send_message(userbot::peer_ref(&peer).await?, grammers_client::message::InputMessage::new().text(cmd)).await.map_err(|e| anyhow::anyhow!("send: {e}"))?;
            let uid = ctx.user_id;
            let deadline = std::time::Instant::now() + Duration::from_secs(if cmd.starts_with('/') { 15 } else { 45 });
            while std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(750)).await;
                let s = since.clone();
                let hit: Option<String> = ctx
                    .db
                    .call(move |cn| {
                        cn.query_row(
                            "SELECT group_concat(text, ' ') FROM messages WHERE user_id = ?1 AND peer_id = ?2 AND is_outgoing = 0 AND date >= ?3",
                            rusqlite::params![uid, bot_id, s],
                            |r| r.get(0),
                        )
                    })
                    .await?;
                if let Some(text) = hit {
                    if expect.split('|').any(|e| text.contains(e)) {
                        return Ok(format!("bot replied ({} chars)", text.chars().count()));
                    }
                }
            }
            bail!("no reply containing {expect:?} in time (is mirror enabled for the bot chat?)")
        }).await);
    }

    out.push(check("news digest build (not sent, not marked)", async {
        match crate::news::build(&ctx, mgr, None).await? {
            crate::news::News::Digest(p) => {
                // Owner-facing text must be Ukrainian: Russian-only letters (ы э ъ) must not outnumber Ukrainian ones (і ї є ґ).
                let count = |set: &str| p.html.chars().filter(|c| set.contains(*c)).count();
                let (uk, ru) = (count("іїєґІЇЄҐ"), count("ыэъЫЭЪ"));
                if ru > uk {
                    bail!("digest looks Russian (uk letters {uk}, ru letters {ru})");
                }
                Ok(format!("digest built from {} post(s), {} chars, language ok (uk {uk} / ru {ru})", p.posts.len(), p.html.chars().count()))
            }
            crate::news::News::Nothing(why) => Ok(format!("nothing to send: {why}")),
        }
    }).await);
    out
}
