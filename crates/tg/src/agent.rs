//! Executes parsed intents and confirmed actions. Anything visible to other people
//! (sending) goes through a stored pending action + inline confirmation.

use anyhow::Result;
use grammers_client::{message::InputMessage, session::types::PeerRef};
use serde::{Deserialize, Serialize};
use tgh_core::{db::repo, intent::Intent};

use crate::{
    bot::{btn, clean, esc, Bot, Kb},
    features,
};

#[derive(Serialize, Deserialize)]
struct SendPayload {
    text: String,
    peer_id: Option<i64>,
    name: Option<String>,
    kind: Option<String>,
}

impl Bot {
    pub(crate) async fn run_intent(&self, it: Intent, peer: PeerRef) -> Result<()> {
        match it {
            Intent::Chat { reply } => self.say(peer, &clean(&reply)).await,
            Intent::Unknown => self.say(peer, "Не зрозумів. Спробуй переформулювати або /help.").await,
            Intent::ListTodos => self.command_todos(peer).await,
            Intent::SetSetting { key, value } => {
                let r = self.apply_setting(&key, &value).await?;
                self.say(peer, &r).await
            }
            Intent::Search { query } => self.command_search(&query, peer).await,
            Intent::SendMessage { recipient, text } => self.propose_send(&recipient, &text, peer).await,
            Intent::SummarizeChat { contact } => self.chat_pick(&contact, "summary", peer).await,
            Intent::TasksForChat { contact } | Intent::AddRemindersFromChat { contact } => self.chat_pick(&contact, "tasks", peer).await,
            Intent::Catchup { contact } => self.chat_pick(&contact, "catchup", peer).await,
            Intent::DraftReply { contact, .. } => self.chat_pick(&contact, "draft", peer).await,
            Intent::FindInChats { query, action } => self.find_in_chats(&query, action.as_deref().unwrap_or("catchup"), peer).await,
            Intent::AddNewsTopic { topic, hours } => {
                let (uid, t, h) = (self.ctx.user_id, topic.clone(), hours.unwrap_or(24).clamp(1, 168));
                self.ctx.db.call(move |c| repo::add_news_topic(c, uid, &t, h)).await?;
                self.say(peer, &format!("📰 Тему «{}» додано.", esc(&topic))).await
            }
            Intent::RemoveNewsTopic { topic } => {
                let (uid, t) = (self.ctx.user_id, topic.clone());
                let n = self.ctx.db.call(move |c| repo::remove_news_topics(c, uid, &t)).await?;
                self.say(peer, &format!("Видалено тем: {n}")).await
            }
            Intent::NewsDigest { topic, hours } => self.news_digest(&topic, hours.unwrap_or(24), peer).await,
            Intent::AddReminder { text, when, peer_query } => self.add_reminder(&text, when.as_deref(), peer_query.as_deref(), peer).await,
            Intent::RemoveReminder { query } => {
                let (uid, q) = (self.ctx.user_id, query.clone());
                let gone = self.ctx.db.call(move |c| repo::cancel_commitments_matching(c, uid, &q)).await?;
                let msg = if gone.is_empty() { "Нічого не знайшов за цим запитом.".to_string() } else { format!("Скасовано:\n{}", gone.iter().map(|t| format!("• {}", esc(t))).collect::<Vec<_>>().join("\n")) };
                self.say(peer, &msg).await
            }
            Intent::Multi { actions } => {
                for a in intent_flat(actions) {
                    Box::pin(self.run_intent(a, peer)).await?;
                }
                Ok(())
            }
        }
    }

    pub(crate) async fn command_todos(&self, peer: PeerRef) -> Result<()> {
        self.command_by_name("todos", "", peer).await
    }

    pub(crate) async fn command_search(&self, q: &str, peer: PeerRef) -> Result<()> {
        self.command_by_name("search", q, peer).await
    }

    async fn propose_send(&self, recipient: &str, text: &str, peer: PeerRef) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return self.say(peer, "Не зрозумів, що саме відправити. Напиши текст повідомлення.").await;
        }
        if text.chars().count() > 4000 {
            return self.say(peer, "Повідомлення задовге для одного відправлення (ліміт Telegram — 4096 символів).").await;
        }
        let found = features::find_contacts(&self.ctx, recipient).await?;
        if found.is_empty() {
            return self.say(peer, &format!("Не знайшов контакт «{}». Спробуй /sync.", esc(recipient))).await;
        }
        let payload = |k: Option<&repo::ContactRow>| serde_json::to_string(&SendPayload { text: text.to_string(), peer_id: k.map(|k| k.peer_id), name: k.map(|k| k.display_name.clone()), kind: k.map(|k| k.peer_kind.clone()) }).unwrap_or_default();
        let uid = self.ctx.user_id;
        let clear = found.len() == 1 || (found[0].1 >= 90 && found[1].1 + 10 <= found[0].1);
        let p = payload(if clear { Some(&found[0].0) } else { None });
        let pid = self.ctx.db.call(move |c| repo::pending_add(c, uid, "send_message", &p)).await?;
        if clear {
            self.ask_confirm(pid, &found[0].0.display_name, text, peer).await
        } else {
            let rows: Kb = found.iter().map(|(k, _)| vec![btn(k.display_name.clone(), format!("sel:{pid}:{}", k.peer_id))]).collect();
            self.say_with(peer, "Кому саме?", Some(rows)).await
        }
    }

    async fn ask_confirm(&self, pid: i64, name: &str, text: &str, peer: PeerRef) -> Result<()> {
        let kb = vec![vec![btn("✅ Відправити", format!("ok:{pid}")), btn("✖ Відхилити", format!("no:{pid}"))]];
        self.say_with(peer, &format!("Відправити <b>{}</b>:\n\n{}", esc(name), esc(text)), Some(kb)).await
    }

    pub(crate) async fn select_recipient(&self, pid: i64, peer_id: i64, peer: PeerRef) -> Result<()> {
        let uid = self.ctx.user_id;
        let Some((_, raw)) = self.ctx.db.call(move |c| repo::pending_get(c, uid, pid)).await? else { return self.say(peer, "Ця дія вже недійсна.").await };
        let mut p: SendPayload = serde_json::from_str(&raw)?;
        let contacts = self.ctx.db.call(move |c| repo::list_contacts(c, uid)).await?;
        let Some(k) = contacts.into_iter().find(|k| k.peer_id == peer_id) else { return self.say(peer, "Контакт зник зі списку.").await };
        (p.peer_id, p.name, p.kind) = (Some(k.peer_id), Some(k.display_name.clone()), Some(k.peer_kind));
        let s = serde_json::to_string(&p)?;
        self.ctx.db.call(move |c| repo::pending_set_payload(c, uid, pid, &s)).await?;
        self.ask_confirm(pid, &k.display_name, &p.text, peer).await
    }

    pub(crate) async fn confirm_send(&self, pid: i64, peer: PeerRef) -> Result<()> {
        let uid = self.ctx.user_id;
        // take() makes the action single-use: a double click cannot send twice.
        let Some((kind, raw)) = self.ctx.db.call(move |c| repo::pending_take(c, uid, pid)).await? else { return self.say(peer, "Ця дія вже виконана або застаріла.").await };
        if kind != "send_message" {
            return Ok(());
        }
        let p: SendPayload = serde_json::from_str(&raw)?;
        let (Some(peer_id), Some(kind)) = (p.peer_id, p.kind.as_deref()) else { return self.say(peer, "Не вибрано отримувача.").await };
        let Some(client) = self.mgr.client().await else { return self.say(peer, "Userbot не підключено — /login.").await };
        let Some(target) = self.mgr.peer_ref(kind, peer_id).await else { return self.say(peer, "Цей чат ще не в кеші сесії — виконай /sync і спробуй знову.").await };
        match client.send_message(target, InputMessage::new().text(p.text.clone())).await {
            Ok(_) => {
                self.ctx.event("send", Some(peer_id), None).await;
                self.say(peer, &format!("✅ Відправлено: <b>{}</b>", esc(p.name.as_deref().unwrap_or("?")))).await
            }
            Err(e) => self.say(peer, &format!("Не вдалося відправити: {}", esc(&e.to_string()))).await,
        }
    }

    pub(crate) async fn run_chat_action(&self, action: &str, peer_id: i64, name: &str, peer: PeerRef) -> Result<()> {
        if action == "menu" {
            let rows: Kb = vec![
                vec![btn("📝 Саммарі", format!("c:summary:{peer_id}")), btn("🎯 Задачі", format!("c:tasks:{peer_id}"))],
                vec![btn("✍ Чернетка", format!("c:draft:{peer_id}")), btn("⏪ Де зупинились", format!("c:catchup:{peer_id}"))],
            ];
            return self.say_with(peer, &format!("Чат: <b>{}</b>", esc(name)), Some(rows)).await;
        }
        let Some(llm) = self.ctx.llm().await? else { return self.say(peer, "Спершу додай LLM-ключ: <code>/key openai sk-…</code>").await };
        let msgs = self.recent_messages(peer_id, name).await?;
        if msgs.is_empty() {
            return self.say(peer, "У цьому чаті немає повідомлень (або Telegram їх не віддав).").await;
        }
        let heavy = self.ctx.settings().await?.use_heavy_model;
        let out = match action {
            "summary" => features::summarize(&llm, heavy, name, &msgs).await?,
            "draft" => features::draft_reply(&llm, heavy, name, &msgs, None).await?,
            "catchup" => features::catchup(&llm, heavy, name, &msgs).await?,
            "tasks" => {
                let saved = features::extract_commitments(&self.ctx, &llm, peer_id, name, &msgs).await?;
                if saved.is_empty() { "Явних обіцянок не знайшов.".into() } else {
                    format!("Збережено ({}):\n{}\n\n/todos — керувати", saved.len(), saved.iter().map(|(d, t, dl)| format!("• {} {}{}", if d == "mine" { "я →" } else { "мені:" }, esc(t), dl.as_deref().map_or(String::new(), |d| format!(" (до {d} UTC)")))).collect::<Vec<_>>().join("\n"))
                }
            }
            _ => return Ok(()),
        };
        self.say(peer, &out).await
    }

    /// Messages for an explicit request about one chat. The DB copy is refreshed from Telegram first,
    /// so it works for chats that were never mirrored. If the chat is switched off (mirror off, or
    /// archived), the messages are fetched for this answer only and never stored.
    async fn recent_messages(&self, peer_id: i64, name: &str) -> Result<Vec<tgh_core::db::repo::MessageRow>> {
        let uid = self.ctx.user_id;
        let (kind, mirror) = self
            .ctx
            .db
            .call(move |c| {
                let kind = repo::list_contacts(c, uid)?.into_iter().find(|k| k.peer_id == peer_id).map(|k| k.peer_kind);
                Ok((kind, repo::mirror_enabled(c, uid, peer_id)?))
            })
            .await?;
        if let (Some(kind), Some(client)) = (kind, self.mgr.client().await) {
            match self.mgr.peer_ref(&kind, peer_id).await {
                None => tracing::warn!("'{name}' is not in the session cache (run /sync); using stored messages"),
                Some(pref) => {
                    let live = if mirror {
                        crate::userbot::backfill_peer(&self.ctx, &client, pref, peer_id, 60).await.map(|_| None)
                    } else {
                        crate::userbot::fetch_recent(&client, pref, peer_id, 60).await.map(Some)
                    };
                    match live {
                        Ok(Some(rows)) => return Ok(rows),
                        Ok(None) => {}
                        Err(e) => tracing::warn!("live fetch for '{name}' failed, using stored messages: {e:#}"),
                    }
                }
            }
        }
        features::history(&self.ctx, peer_id, 60).await
    }

    /// "Which chat was that?": local full-text search first, then Telegram's own global search
    /// (which also covers chats that were never mirrored).
    async fn find_in_chats(&self, query: &str, action: &str, peer: PeerRef) -> Result<()> {
        let (uid, q) = (self.ctx.user_id, query.to_string());
        let mut hits = self.ctx.db.call(move |c| repo::chats_matching(c, uid, &q, 5)).await?;
        if hits.is_empty() {
            hits = self.live_chat_search(query).await;
        }
        if hits.is_empty() {
            return self.say(peer, "Не знайшов розмов за цим запитом ні в базі, ні в Telegram.").await;
        }
        let action = match action { "summary" | "tasks" | "draft" | "catchup" => action, _ => "catchup" };
        let rows: Kb = hits.iter().map(|(id, name, n)| vec![btn(format!("{name} ({n})"), format!("c:{action}:{id}"))]).collect();
        self.say_with(peer, "Знайшов такі чати — обери:", Some(rows)).await
    }

    /// Telegram-side search across all chats: (peer_id, name, hits), best first.
    async fn live_chat_search(&self, query: &str) -> Vec<(i64, String, i64)> {
        let Some(client) = self.mgr.client().await else { return vec![] };
        let mut found: std::collections::HashMap<i64, (String, i64)> = Default::default();
        let mut it = client.search_all_messages().query(query).limit(40);
        loop {
            match it.next().await {
                Ok(Some(m)) => {
                    if let (Some(p), Some(id)) = (m.peer(), m.peer_id().bare_id()) {
                        found.entry(id).or_insert_with(|| (p.name().unwrap_or("?").to_string(), 0)).1 += 1;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("global search failed: {e}");
                    break;
                }
            }
        }
        let mut v: Vec<(i64, String, i64)> = found.into_iter().map(|(id, (n, c))| (id, n, c)).collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.2));
        v.truncate(5);
        v
    }

    async fn add_reminder(&self, text: &str, when: Option<&str>, peer_query: Option<&str>, peer: PeerRef) -> Result<()> {
        let deadline = when.and_then(features::parse_deadline);
        let (peer_id, name) = match peer_query {
            Some(q) => match features::find_contacts(&self.ctx, q).await?.first() { Some((k, _)) => (k.peer_id, k.display_name.clone()), None => (0, String::new()) },
            None => (0, String::new()),
        };
        let (uid, t, d) = (self.ctx.user_id, text.to_string(), deadline.clone());
        self.ctx.db.call(move |c| repo::add_commitment(c, uid, peer_id, &name, "mine", &t, d.as_deref())).await?;
        let tz: chrono_tz::Tz = self.ctx.settings().await?.timezone.parse().unwrap_or(chrono_tz::UTC);
        let when_txt = deadline.and_then(|d| repo::parse_ts(&d)).map(|d| d.and_utc().with_timezone(&tz).format("%d.%m %H:%M").to_string());
        self.say(peer, &format!("⏰ Запам'ятав: {}{}", esc(text), when_txt.map_or(" (без дати)".into(), |w| format!(" — {w}")))).await
    }

    /// `/news [topic]`: fetch, dedupe, digest. Marks posts as sent only after delivery succeeded.
    pub(crate) async fn news_digest(&self, topic: &str, _hours: i64, peer: PeerRef) -> Result<()> {
        let topic = Some(topic.trim()).filter(|t| !t.is_empty());
        match crate::news::build(&self.ctx, &self.mgr, topic).await? {
            crate::news::News::Nothing(why) => self.say(peer, &esc(&why)).await,
            crate::news::News::Digest(pack) => {
                self.say(peer, &pack.html).await?;
                crate::news::mark_sent(&self.ctx, pack.posts).await
            }
        }
    }

    pub(crate) async fn extra_command(&self, cmd: &str, arg: &str, peer: PeerRef) -> Result<()> {
        match cmd {
            "send" if arg.is_empty() => self.say(peer, "Формат: <code>/send скажи Олі, що дзвінок о 8</code>").await,
            "send" => self.free_text_public(&format!("Напиши: {arg}"), peer).await,
            "digest" => {
                let s = self.ctx.settings().await?;
                match arg.split_whitespace().collect::<Vec<_>>().as_slice() {
                    ["on"] | ["off"] => { let on = arg == "on"; let r = self.apply_setting("digest_enabled", &on.into()).await?; self.say(peer, &r).await }
                    ["at", t] => { let r = self.apply_setting("digest_time", &(*t).into()).await?; self.say(peer, &r).await }
                    _ => { let _ = s; let d = features::build_digest(&self.ctx).await?; self.say(peer, &d).await }
                }
            }
            "news" => self.news_digest(arg, 24, peer).await,
            "topics" => {
                let uid = self.ctx.user_id;
                let topics = self.ctx.db.call(move |c| repo::list_news_topics(c, uid)).await?;
                let list = if topics.is_empty() { "тем немає".into() } else { topics.iter().map(|(t, h)| format!("• {} ({h} год)", esc(t))).collect::<Vec<_>>().join("\n") };
                self.say(peer, &format!("<b>Теми ранкових новин</b>\n{list}\n\nЗараз: /news або <code>/news тема</code>. Канали-джерела — у веб-інтерфейсі /chats")).await
            }
            "classify" => {
                self.say(peer, "Розкладаю чати за категоріями…").await?;
                match crate::classify::run(&self.ctx, 400).await? {
                    None => self.say(peer, "Спершу додай LLM-ключ.").await,
                    Some((n, news)) => self.say(peer, &format!("Розкладено: {n}, з них джерел новин: {news}. Перевір і поправ на http://{}/chats", self.ctx.cfg.web_addr)).await,
                }
            }
            "sources" => {
                let found = features::find_contacts(&self.ctx, arg).await?;
                let Some((k, _)) = found.first() else { return self.say(peer, "Формат: <code>/sources Назва каналу</code> (перемикає джерело новин)").await };
                let (uid, id) = (self.ctx.user_id, k.peer_id);
                let cur: bool = self.ctx.db.call(move |c| c.query_row("SELECT is_news_source FROM contacts WHERE user_id = ? AND peer_id = ?", [uid, id], |r| r.get(0))).await?;
                self.ctx.db.call(move |c| repo::set_news_source(c, uid, id, !cur)).await?;
                self.say(peer, &format!("{} <b>{}</b> {} джерела новин", if cur { "✖" } else { "✅" }, esc(&k.display_name), if cur { "прибрано з" } else { "додано до" })).await
            }
            _ => self.say(peer, "Ця команда поки що недоступна.").await,
        }
    }
}

fn intent_flat(actions: Vec<Intent>) -> Vec<Intent> {
    actions.into_iter().flat_map(tgh_core::intent::flatten).take(5).collect()
}
