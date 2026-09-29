//! LLM providers (OpenAI, Gemini) over plain HTTPS. Every call is recorded in `llm_usage`
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
}

impl Provider {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai" => Some(Self::OpenAi),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self { Self::OpenAi => "openai", Self::Gemini => "gemini" }
    }
    fn chat_model(self, heavy: bool) -> &'static str {
        match (self, heavy) {
            (Self::OpenAi, false) => m::OPENAI_CHAT_LIGHT,
            (Self::OpenAi, true) => m::OPENAI_CHAT_HEAVY,
            (Self::Gemini, false) => m::GEMINI_CHAT_LIGHT,
            (Self::Gemini, true) => m::GEMINI_CHAT_HEAVY,
        }
    }
    fn embed_model(self) -> &'static str {
        match self { Self::OpenAi => m::OPENAI_EMBED, Self::Gemini => m::GEMINI_EMBED }
    }
}

#[derive(Debug, PartialEq)]
pub struct Completion {
    pub text: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}

pub struct LlmClient {
    http: reqwest::Client,
    provider: Provider,
    key: String,
    base: String,
    db: Db,
}

impl LlmClient {
    pub fn new(provider: Provider, key: String, db: Db) -> Self {
        let base = match provider {
            Provider::OpenAi => "https://api.openai.com/v1",
            Provider::Gemini => "https://generativelanguage.googleapis.com/v1beta",
        };
        Self::with_base(provider, key, db, base)
    }

    pub fn with_base(provider: Provider, key: String, db: Db, base: &str) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(90)).build().expect("reqwest client");
        Self { http, provider, key, base: base.trim_end_matches('/').to_string(), db }
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    /// One chat completion; `purpose` labels the call in analytics ("agent", "summary", ...).
    pub async fn chat(&self, purpose: &str, messages: &[ChatMessage], heavy: bool) -> Result<String> {
        let model = self.provider.chat_model(heavy);
        let started = Instant::now();
        let res = self.chat_inner(model, messages).await;
        let (p, c, ok) = res.as_ref().map(|r| (r.prompt_tokens, r.completion_tokens, true)).unwrap_or((0, 0, false));
        self.record(purpose, model, p, c, started.elapsed(), ok).await;
        Ok(res?.text)
    }

    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let model = self.provider.embed_model();
        let started = Instant::now();
        let res = self.embed_inner(model, text).await;
        self.record("embed", model, 0, 0, started.elapsed(), res.is_ok()).await;
        res
    }

    /// Cheap key check used when the owner saves a key.
    pub async fn validate(&self) -> bool {
        self.chat_inner(self.provider.chat_model(false), &[ChatMessage::user("ping")]).await.is_ok()
    }

    async fn record(&self, purpose: &str, model: &str, p: i64, c: i64, took: Duration, ok: bool) {
        let (prov, model, purpose) = (self.provider.name(), model.to_string(), purpose.to_string());
        let ms = took.as_millis() as i64;
        if let Err(e) = self.db.call(move |db| repo::log_llm_usage(db, prov, &model, &purpose, p, c, ms, ok)).await {
            tracing::warn!("cannot record llm usage: {e:#}");
        }
    }

    async fn chat_inner(&self, model: &str, messages: &[ChatMessage]) -> Result<Completion> {
        let req = match self.provider {
            Provider::OpenAi => self
                .http
                .post(format!("{}/chat/completions", self.base))
                .bearer_auth(&self.key)
                .json(&openai_chat_body(model, messages)),
            Provider::Gemini => self
                .http
                .post(format!("{}/models/{model}:generateContent", self.base))
                .header("x-goog-api-key", &self.key)
                .json(&gemini_chat_body(messages)),
        };
        let v = send(req).await?;
        match self.provider {
            Provider::OpenAi => parse_openai_chat(&v),
            Provider::Gemini => parse_gemini_chat(&v),
        }
    }

    async fn embed_inner(&self, model: &str, text: &str) -> Result<Vec<f32>> {
        let (req, path): (_, &[&str]) = match self.provider {
            Provider::OpenAi => (
                self.http.post(format!("{}/embeddings", self.base)).bearer_auth(&self.key).json(&json!({"model": model, "input": text})),
                &["data", "0", "embedding"],
            ),
            Provider::Gemini => (
                self.http
                    .post(format!("{}/models/{model}:embedContent", self.base))
                    .header("x-goog-api-key", &self.key)
                    .json(&json!({"model": format!("models/{model}"), "content": {"parts": [{"text": text}]}})),
                &["embedding", "values"],
            ),
        };
        let v = send(req).await?;
        let mut cur = &v;
        for k in path {
            cur = cur.get(*k).or_else(|| k.parse::<usize>().ok().and_then(|i| cur.get(i))).context("embedding missing in response")?;
        }
        cur.as_array().context("embedding is not an array")?.iter().map(|x| x.as_f64().map(|f| f as f32).context("non-numeric embedding")).collect()
    }
}

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
    let parts = v.pointer("/candidates/0/content/parts").and_then(Value::as_array).context("no candidates[0].content.parts")?;
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
}
