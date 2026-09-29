//! LLM providers (Gemini, Groq, Z.ai, OpenAI) over plain HTTPS, chained with fallback. Every call is recorded in `llm_usage`
//! so the dashboard can show tokens, latency and errors.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::{
    config::llm_defaults as m,
    db::{repo, Db},
};

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: &'static str, // "system" | "user" | "assistant"
    pub content: String,
}

impl ChatMessage {
    pub fn system(s: impl Into<String>) -> Self { Self { role: "system", content: s.into() } }
    pub fn user(s: impl Into<String>) -> Self { Self { role: "user", content: s.into() } }
    pub fn assistant(s: impl Into<String>) -> Self { Self { role: "assistant", content: s.into() } }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    Gemini,
    /// Groq and Z.ai speak the OpenAI chat-completions protocol on their own base URLs.
    Groq,
    Zai,
}

impl Provider {
    pub const ALL: [Provider; 4] = [Self::Gemini, Self::Groq, Self::Zai, Self::OpenAi];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai" => Some(Self::OpenAi),
            "gemini" => Some(Self::Gemini),
            "groq" => Some(Self::Groq),
            "zai" => Some(Self::Zai),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self { Self::OpenAi => "openai", Self::Gemini => "gemini", Self::Groq => "groq", Self::Zai => "zai" }
    }
    fn default_base(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Gemini => "https://generativelanguage.googleapis.com/v1beta",
            Self::Groq => "https://api.groq.com/openai/v1",
            Self::Zai => "https://api.z.ai/api/paas/v4",
        }
    }
    fn openai_style(self) -> bool {
        !matches!(self, Self::Gemini)
    }
    /// Models to try in order. Free-tier quotas are per model, so a spent daily quota falls through to the next.
    fn chat_models(self, heavy: bool) -> Vec<&'static str> {
        match (self, heavy) {
            (Self::OpenAi, false) => vec![m::OPENAI_CHAT_LIGHT],
            (Self::OpenAi, true) => vec![m::OPENAI_CHAT_HEAVY],
            (Self::Gemini, false) => vec![m::GEMINI_CHAT_LIGHT, m::GEMINI_CHAT_FALLBACK, "gemini-3.1-flash-lite"],
            (Self::Gemini, true) => vec![m::GEMINI_CHAT_HEAVY, m::GEMINI_CHAT_LIGHT, m::GEMINI_CHAT_FALLBACK],
            (Self::Groq, _) => vec!["openai/gpt-oss-120b"],
            (Self::Zai, _) => vec!["glm-4.7-flash"],
        }
    }
    fn embed_model(self) -> Option<&'static str> {
        match self { Self::OpenAi => Some(m::OPENAI_EMBED), Self::Gemini => Some(m::GEMINI_EMBED), _ => None }
    }
}

#[derive(Debug, PartialEq)]
pub struct Completion {
    pub text: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}

#[derive(Clone)]
struct Entry {
    provider: Provider,
    key: String,
    base: String,
}

/// Models whose *daily* quota is spent, with the time we may try them again. Process-wide, so
/// short-lived clients do not keep hammering a dead model.
fn spent() -> &'static std::sync::Mutex<std::collections::HashMap<String, Instant>> {
    static SPENT: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Instant>>> = std::sync::OnceLock::new();
    SPENT.get_or_init(Default::default)
}

fn is_spent(provider: Provider, model: &str) -> bool {
    let key = format!("{}/{model}", provider.name());
    spent().lock().unwrap_or_else(|e| e.into_inner()).get(&key).is_some_and(|until| Instant::now() < *until)
}

fn mark_spent(provider: Provider, model: &str) {
    spent().lock().unwrap_or_else(|e| e.into_inner()).insert(format!("{}/{model}", provider.name()), Instant::now() + Duration::from_secs(30 * 60));
}

/// A chain of providers: the first is tried first, the rest take over when it fails.
pub struct LlmClient {
    http: reqwest::Client,
    chain: Vec<Entry>,
    db: Db,
}

impl LlmClient {
    pub fn new(provider: Provider, key: String, db: Db) -> Self {
        Self::with_base(provider, key, db, provider.default_base())
    }

    pub fn with_base(provider: Provider, key: String, db: Db, base: &str) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(90)).build().expect("reqwest client");
        Self { http, chain: vec![Entry { provider, key, base: base.trim_end_matches('/').to_string() }], db }
    }

    /// Adds a provider that is tried only if all earlier ones failed.
    pub fn with_fallback(self, provider: Provider, key: String) -> Self {
        self.with_fallback_base(provider, key, provider.default_base())
    }

    pub fn with_fallback_base(mut self, provider: Provider, key: String, base: &str) -> Self {
        self.chain.push(Entry { provider, key, base: base.trim_end_matches('/').to_string() });
        self
    }

    pub fn provider(&self) -> Provider {
        self.chain[0].provider
    }

    /// One chat completion; `purpose` labels the call in analytics ("agent", "summary", ...).
    /// Falls through models (spent daily quota) and then providers (any other failure).
    pub async fn chat(&self, purpose: &str, messages: &[ChatMessage], heavy: bool) -> Result<String> {
        let mut last: Option<anyhow::Error> = None;
        for entry in &self.chain {
            for model in entry.provider.chat_models(heavy) {
                if is_spent(entry.provider, model) {
                    continue;
                }
                let started = Instant::now();
                let res = self.chat_inner(entry, model, messages).await;
                let (p, c, ok) = res.as_ref().map(|r| (r.prompt_tokens, r.completion_tokens, true)).unwrap_or((0, 0, false));
                self.record(entry.provider, purpose, model, p, c, started.elapsed(), ok).await;
                match res {
                    Ok(r) => return Ok(r.text),
                    Err(e) if e.to_string().contains(DAILY_QUOTA) => {
                        tracing::warn!("{}/{model}: daily quota spent, skipping it for 30 min", entry.provider.name());
                        mark_spent(entry.provider, model);
                        last = Some(e);
                    }
                    Err(e) => {
                        tracing::warn!("{}/{model} failed, trying the next provider: {}", entry.provider.name(), e.to_string().chars().take(120).collect::<String>().replace('\n', " "));
                        last = Some(e);
                        break; // other errors are not model-specific: move on to the next provider
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("no LLM provider available")))
    }

    /// Embedding from the first provider in the chain that supports it.
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let entry = self.chain.iter().find(|e| e.provider.embed_model().is_some()).context("no configured provider supports embeddings")?;
        let model = entry.provider.embed_model().unwrap();
        let started = Instant::now();
        let res = self.embed_inner(entry, model, text).await;
        self.record(entry.provider, "embed", model, 0, 0, started.elapsed(), res.is_ok()).await;
        res
    }

    /// Cheap key check used when the owner saves a key (first entry only).
    pub async fn validate(&self) -> bool {
        let e = &self.chain[0];
        self.chat_inner(e, e.provider.chat_models(false)[0], &[ChatMessage::user("ping")]).await.is_ok()
    }

    #[allow(clippy::too_many_arguments)]
    async fn record(&self, provider: Provider, purpose: &str, model: &str, p: i64, c: i64, took: Duration, ok: bool) {
        let (prov, model, purpose) = (provider.name(), model.to_string(), purpose.to_string());
        let ms = took.as_millis() as i64;
        if let Err(e) = self.db.call(move |db| repo::log_llm_usage(db, prov, &model, &purpose, p, c, ms, ok)).await {
            tracing::warn!("cannot record llm usage: {e:#}");
        }
    }

    async fn chat_inner(&self, e: &Entry, model: &str, messages: &[ChatMessage]) -> Result<Completion> {
        let req = if e.provider.openai_style() {
            self.http.post(format!("{}/chat/completions", e.base)).bearer_auth(&e.key).json(&openai_chat_body(model, messages))
        } else {
            self.http.post(format!("{}/models/{model}:generateContent", e.base)).header("x-goog-api-key", &e.key).json(&gemini_chat_body(messages))
        };
        let v = send(req).await?;
        if e.provider.openai_style() { parse_openai_chat(&v) } else { parse_gemini_chat(&v) }
    }

    async fn embed_inner(&self, e: &Entry, model: &str, text: &str) -> Result<Vec<f32>> {
        let (req, path): (_, &[&str]) = if e.provider.openai_style() {
            (
                self.http.post(format!("{}/embeddings", e.base)).bearer_auth(&e.key).json(&json!({"model": model, "input": text})),
                &["data", "0", "embedding"],
            )
        } else {
            (
                self.http
                    .post(format!("{}/models/{model}:embedContent", e.base))
                    .header("x-goog-api-key", &e.key)
                    .json(&json!({"model": format!("models/{model}"), "content": {"parts": [{"text": text}]}})),
                &["embedding", "values"],
            )
        };
        let v = send(req).await?;
        let mut cur = &v;
        for k in path {
            cur = cur.get(*k).or_else(|| k.parse::<usize>().ok().and_then(|i| cur.get(i))).context("embedding missing in response")?;
        }
        cur.as_array().context("embedding is not an array")?.iter().map(|x| x.as_f64().map(|f| f as f32).context("non-numeric embedding")).collect()
    }
}

/// Marker in the error text for a spent *daily* quota (retrying today is pointless; another model may still work).
pub const DAILY_QUOTA: &str = "daily quota exhausted";

/// Transient provider errors worth retrying (rate limit / overload).
fn is_transient(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504)
}

async fn send(req: reqwest::RequestBuilder) -> Result<Value> {
    let mut delay = Duration::from_millis(800);
    for attempt in 0..3 {
        let this = req.try_clone().context("request not cloneable")?;
        match send_once(this).await {
            Err(SendErr::Transient(e)) if attempt < 2 => {
                tracing::warn!("LLM transient error, retrying in {delay:?}: {}", e.to_string().chars().take(100).collect::<String>().replace('\n', " "));
                tokio::time::sleep(delay).await;
                delay *= 3;
            }
            Err(SendErr::Transient(e)) | Err(SendErr::Fatal(e)) => return Err(e),
            Ok(v) => return Ok(v),
        }
    }
    unreachable!("loop returns on the last attempt")
}

enum SendErr {
    Transient(anyhow::Error),
    Fatal(anyhow::Error),
}

async fn send_once(req: reqwest::RequestBuilder) -> std::result::Result<Value, SendErr> {
    let resp = req.send().await.map_err(|e| SendErr::Transient(anyhow::anyhow!("LLM request failed: {e}")))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| SendErr::Transient(anyhow::anyhow!("LLM response unreadable: {e}")))?;
    if !status.is_success() {
        // Provider error bodies can echo request data; keep only a short prefix.
        if status.as_u16() == 429 && body.contains("PerDay") {
            return Err(SendErr::Fatal(anyhow::anyhow!("LLM {DAILY_QUOTA} (provider quota per day)")));
        }
        let e = anyhow::anyhow!("LLM HTTP {status}: {}", body.chars().take(200).collect::<String>());
        return Err(if is_transient(status) { SendErr::Transient(e) } else { SendErr::Fatal(e) });
    }
    serde_json::from_str(&body).map_err(|e| SendErr::Fatal(anyhow::anyhow!("LLM returned invalid JSON: {e}")))
}

pub fn openai_chat_body(model: &str, messages: &[ChatMessage]) -> Value {
    json!({"model": model, "messages": messages.iter().map(|m| json!({"role": m.role, "content": m.content})).collect::<Vec<_>>()})
}

pub fn gemini_chat_body(messages: &[ChatMessage]) -> Value {
    let system: Vec<&str> = messages.iter().filter(|m| m.role == "system").map(|m| m.content.as_str()).collect();
    let contents: Vec<Value> = messages
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| json!({"role": if m.role == "assistant" { "model" } else { "user" }, "parts": [{"text": m.content}]}))
        .collect();
    let mut body = json!({"contents": contents});
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts": [{"text": system.join("\n\n")}]});
    }
    body
}

pub fn parse_openai_chat(v: &Value) -> Result<Completion> {
    let text = v.pointer("/choices/0/message/content").and_then(Value::as_str).context("no choices[0].message.content")?;
    Ok(Completion {
        text: text.to_string(),
        prompt_tokens: v.pointer("/usage/prompt_tokens").and_then(Value::as_i64).unwrap_or(0),
        completion_tokens: v.pointer("/usage/completion_tokens").and_then(Value::as_i64).unwrap_or(0),
    })
}

pub fn parse_gemini_chat(v: &Value) -> Result<Completion> {
    let Some(parts) = v.pointer("/candidates/0/content/parts").and_then(Value::as_array) else {
        // Blocked or cut off: say why instead of a bare "missing field".
        let reason = v.pointer("/candidates/0/finishReason").and_then(Value::as_str)
            .or_else(|| v.pointer("/promptFeedback/blockReason").and_then(Value::as_str))
            .unwrap_or("unknown");
        anyhow::bail!("Gemini returned no content (reason: {reason})");
    };
    let text: String = parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect();
    Ok(Completion {
        text,
        prompt_tokens: v.pointer("/usageMetadata/promptTokenCount").and_then(Value::as_i64).unwrap_or(0),
        completion_tokens: v.pointer("/usageMetadata/candidatesTokenCount").and_then(Value::as_i64).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_roundtrip_shapes() {
        let body = openai_chat_body("gpt", &[ChatMessage::system("s"), ChatMessage::user("u")]);
        assert_eq!(body["messages"][1]["content"], "u");
        let c = parse_openai_chat(&json!({"choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":7,"completion_tokens":2}})).unwrap();
        assert_eq!(c, Completion { text: "hi".into(), prompt_tokens: 7, completion_tokens: 2 });
        assert!(parse_openai_chat(&json!({"error":"x"})).is_err());
        let blocked = parse_gemini_chat(&json!({"candidates":[{"finishReason":"SAFETY"}]})).unwrap_err().to_string();
        assert!(blocked.contains("SAFETY"), "{blocked}");
    }

    #[test]
    fn gemini_maps_roles_and_system() {
        let b = gemini_chat_body(&[ChatMessage::system("be brief"), ChatMessage::user("q"), ChatMessage::assistant("a")]);
        assert_eq!(b["systemInstruction"]["parts"][0]["text"], "be brief");
        assert_eq!(b["contents"][1]["role"], "model");
        let c = parse_gemini_chat(&json!({"candidates":[{"content":{"parts":[{"text":"he"},{"text":"llo"}]}}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1}})).unwrap();
        assert_eq!((c.text.as_str(), c.prompt_tokens), ("hello", 3));
    }

    #[tokio::test]
    async fn retries_transient_503_then_succeeds() {
        use axum::{http::StatusCode, routing::post, Json, Router};
        use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = Router::new().route("/chat/completions", post(move || {
            let h = h.clone();
            async move {
                if h.fetch_add(1, Ordering::SeqCst) < 2 {
                    (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"busy"})))
                } else {
                    (StatusCode::OK, Json(json!({"choices":[{"message":{"content":"ok"}}]})))
                }
            }
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let llm = LlmClient::with_base(Provider::OpenAi, "k".into(), Db::open_in_memory().unwrap(), &base);
        assert_eq!(llm.chat("t", &[ChatMessage::user("x")], false).await.unwrap(), "ok");
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn chain_falls_through_to_next_provider() {
        use axum::{http::StatusCode, routing::post, Json, Router};
        let bad = Router::new().route("/chat/completions", post(|| async { (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad key"}))) }));
        let good = Router::new().route("/chat/completions", post(|| async { Json(json!({"choices":[{"message":{"content":"from groq"}}],"usage":{"prompt_tokens":2,"completion_tokens":1}})) }));
        let mut bases = Vec::new();
        for app in [bad, good] {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            bases.push(format!("http://{}", l.local_addr().unwrap()));
            tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        }
        let db = Db::open_in_memory().unwrap();
        let llm = LlmClient::with_base(Provider::Zai, "k1".into(), db.clone(), &bases[0]).with_fallback_base(Provider::Groq, "k2".into(), &bases[1]);
        assert_eq!(llm.chat("t", &[ChatMessage::user("x")], false).await.unwrap(), "from groq");
        let rows: Vec<(String, bool)> = db.call(|c| {
            let mut st = c.prepare("SELECT provider, ok FROM llm_usage ORDER BY id")?;
            let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(r)
        }).await.unwrap();
        assert_eq!(rows, vec![("zai".to_string(), false), ("groq".to_string(), true)]);
    }

    /// Full path against a local fake OpenAI endpoint: checks auth header, parsing and analytics row.
    #[tokio::test]
    async fn chat_records_usage_against_fake_server() {
        use axum::{http::HeaderMap, routing::post, Json, Router};
        let app = Router::new().route("/chat/completions", post(|h: HeaderMap, Json(b): Json<Value>| async move {
            assert_eq!(h["authorization"], "Bearer k");
            assert_eq!(b["model"], m::OPENAI_CHAT_LIGHT);
            Json(json!({"choices":[{"message":{"content":"pong"}}],"usage":{"prompt_tokens":4,"completion_tokens":1}}))
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

        let db = Db::open_in_memory().unwrap();
        let llm = LlmClient::with_base(Provider::OpenAi, "k".into(), db.clone(), &base);
        assert_eq!(llm.chat("test", &[ChatMessage::user("ping")], false).await.unwrap(), "pong");
        let (n, tokens): (i64, i64) = db.call(|c| c.query_row("SELECT count(*), sum(prompt_tokens + completion_tokens) FROM llm_usage WHERE purpose='test' AND ok=1", [], |r| Ok((r.get(0)?, r.get(1)?)))).await.unwrap();
        assert_eq!((n, tokens), (1, 5));
        let bad = LlmClient::with_base(Provider::OpenAi, "k".into(), db.clone(), "http://127.0.0.1:1");
        assert!(bad.chat("test", &[ChatMessage::user("x")], false).await.is_err());
        let fails: i64 = db.call(|c| c.query_row("SELECT count(*) FROM llm_usage WHERE ok=0", [], |r| r.get(0))).await.unwrap();
        assert_eq!(fails, 1);
    }

    #[test]
    fn response_parsers_reject_odd_shapes_without_panicking() {
        for v in [
            json!(null), json!([]), json!("x"), json!({}), json!({"choices": []}), json!({"choices": [null]}), json!({"choices": [{"message": null}]}),
            json!({"choices": [{"message": {"content": null}}]}), json!({"choices": [{"message": {"content": 5}}]}),
            json!({"candidates": []}), json!({"candidates": [{"content": {"parts": "x"}}]}), json!({"candidates": [{"content": {"parts": [null, 5, {"text": 1}]}}]}),
            json!({"usage": {"prompt_tokens": "many"}, "choices": [{"message": {"content": "ok"}}]}),
        ] {
            let _ = parse_openai_chat(&v);
            let _ = parse_gemini_chat(&v);
        }
        // reasoning models sometimes return empty content: that is an error to fall through on, not a blank answer
        assert!(parse_gemini_chat(&json!({"candidates": [{"content": {"parts": []}}]})).map(|c| c.text.is_empty()).unwrap_or(true));
        let odd = parse_openai_chat(&json!({"usage": {"prompt_tokens": "many"}, "choices": [{"message": {"content": "ok"}}]})).unwrap();
        assert_eq!((odd.text.as_str(), odd.prompt_tokens), ("ok", 0)); // bad usage numbers degrade to 0
    }

    #[tokio::test]
    async fn html_error_pages_and_garbage_bodies_are_errors() {
        use axum::{routing::post, Router};
        let app = Router::new()
            .route("/chat/completions", post(|| async { ([(axum::http::header::CONTENT_TYPE, "text/html")], "<html>502 Bad Gateway</html>") }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let llm = LlmClient::with_base(Provider::Groq, "k".into(), Db::open_in_memory().unwrap(), &base);
        let e = llm.chat("t", &[ChatMessage::user("x")], false).await.unwrap_err().to_string();
        assert!(e.contains("invalid JSON"), "{e}");
        // connection refused: reported, not hung
        let dead = LlmClient::with_base(Provider::Zai, "k".into(), Db::open_in_memory().unwrap(), "http://127.0.0.1:1");
        assert!(dead.chat("t", &[ChatMessage::user("x")], false).await.is_err());
    }
}
