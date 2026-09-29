//! Headless dashboard: JSON analytics API + static UI, loopback only.

use std::{
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
};

use axum::{
    extract::{Path as UrlPath, Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::Value;
use tgh_core::{db::analytics as a, db::repo, db::Db, Status};
use tgh_tg::manager::{Manager, QrStatus};

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub status: Arc<Status>,
    /// `None` in `--demo` mode (no Telegram side).
    pub mgr: Option<Arc<Manager>>,
    /// Internal `users.id` of the owner and the avatar cache directory.
    pub user_id: i64,
    pub avatars: std::path::PathBuf,
}

#[derive(Deserialize)]
struct Q {
    days: Option<i64>,
    limit: Option<i64>,
    status: Option<String>,
}

type ApiResult = Result<Json<Value>, (StatusCode, String)>;

fn ok(r: anyhow::Result<Value>) -> ApiResult {
    r.map(Json).map_err(|e| {
        tracing::error!("api error: {e:#}");
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
    })
}

/// State-changing requests must carry our custom header (a cross-site form or `fetch` cannot add it
/// without a CORS preflight, which we never grant) and, if an `Origin` is sent, it must be local.
async fn csrf_guard(req: Request, next: Next) -> Response {
    if !matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD) {
        let has_header = req.headers().get("x-requested-with").and_then(|v| v.to_str().ok()) == Some("tgh");
        let origin_ok = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()).map_or(true, |o| {
            let host = o.split("://").nth(1).unwrap_or("");
            let name = host.rsplit_once(':').map_or(host, |(h, _)| h);
            matches!(name, "localhost" | "127.0.0.1" | "[::1]")
        });
        if !has_header || !origin_ok {
            return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
        }
    }
    next.run(req).await
}

/// Reject requests whose Host is not loopback: blocks DNS-rebinding against an unauthenticated UI.
async fn local_host_only(req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let name = host.rsplit_once(':').map_or(host, |(h, p)| if p.chars().all(|c| c.is_ascii_digit()) { h } else { host });
    if matches!(name, "localhost" | "127.0.0.1" | "[::1]") {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "forbidden host").into_response()
    }
}

async fn asset(body: &'static str, ctype: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, ctype), (header::CACHE_CONTROL, "no-cache")], body)
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| asset(include_str!("../web/index.html"), "text/html; charset=utf-8")))
        .route("/app.js", get(|| asset(include_str!("../web/app.js"), "text/javascript; charset=utf-8")))
        .route("/style.css", get(|| asset(include_str!("../web/style.css"), "text/css; charset=utf-8")))
        .route("/chats", get(|| asset(include_str!("../web/chats.html"), "text/html; charset=utf-8")))
        .route("/chats.js", get(|| asset(include_str!("../web/chats.js"), "text/javascript; charset=utf-8")))
        .route("/api/contacts", get(|State(s): State<AppState>| async move {
            let uid = s.user_id;
            ok(s.db.call(move |c| Ok(serde_json::to_value(repo::list_contacts_full(c, uid)?).unwrap_or_default())).await)
        }))
        .route("/api/contacts/{peer_id}", axum::routing::post(update_contact))
        .route("/avatars/{peer_id}", get(avatar))
        .route("/login", get(|| asset(include_str!("../web/login.html"), "text/html; charset=utf-8")))
        .route("/api/qr", get(|State(s): State<AppState>| async move { Json(qr_json(&s)) }))
        .route("/api/overview", get(|State(s): State<AppState>| async move {
            let up = s.status.userbot_connected.load(Ordering::Relaxed);
            ok(s.db.call(move |c| a::overview(c, up)).await)
        }))
        .route("/api/messages/daily", get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
            let d = q.days.unwrap_or(30);
            ok(s.db.call(move |c| a::messages_daily(c, d)).await)
        }))
        .route("/api/messages/hourly", get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
            let d = q.days.unwrap_or(30);
            ok(s.db.call(move |c| a::messages_hourly(c, d)).await)
        }))
        .route("/api/messages/top_chats", get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
            let (d, l) = (q.days.unwrap_or(30), q.limit.unwrap_or(10));
            ok(s.db.call(move |c| a::top_chats(c, d, l)).await)
        }))
        .route("/api/llm/daily", get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
            let d = q.days.unwrap_or(30);
            ok(s.db.call(move |c| a::llm_daily(c, d)).await)
        }))
        .route("/api/llm/by_purpose", get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
            let d = q.days.unwrap_or(30);
            ok(s.db.call(move |c| a::llm_by_purpose(c, d)).await)
        }))
        .route("/api/autoreply/recent", get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
            let l = q.limit.unwrap_or(50);
            ok(s.db.call(move |c| a::autoreply_recent(c, l)).await)
        }))
        .route("/api/commitments", get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
            let st = q.status.unwrap_or_else(|| "open".into());
            ok(s.db.call(move |c| a::commitments(c, &st)).await)
        }))
        .route("/api/events", get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
            let l = q.limit.unwrap_or(100);
            ok(s.db.call(move |c| a::events(c, l)).await)
        }))
        .layer(middleware::from_fn(csrf_guard))
        .layer(middleware::from_fn(local_host_only))
        .with_state(state)
}

#[derive(Deserialize)]
struct ContactPatch {
    news_source: Option<bool>,
    mirror: Option<bool>,
    category: Option<String>,
}

async fn update_contact(State(s): State<AppState>, UrlPath(peer_id): UrlPath<i64>, Json(p): Json<ContactPatch>) -> Response {
    let uid = s.user_id;
    match s.db.call(move |c| repo::update_contact_flags(c, uid, peer_id, p.news_source, p.mirror, p.category.as_deref())).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("update_contact: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Cached profile picture. The id is parsed as an integer, so the path cannot escape the directory.
async fn avatar(State(s): State<AppState>, UrlPath(peer_id): UrlPath<i64>) -> Response {
    match tokio::fs::read(s.avatars.join(format!("{peer_id}.jpg"))).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "private, max-age=86400")], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn qr_json(s: &AppState) -> Value {
    let Some(mgr) = &s.mgr else { return serde_json::json!({"status": "idle"}) };
    let qr = mgr.qr();
    let url = qr.url.lock().unwrap().clone();
    let svg = url.and_then(|u| {
        qrcode::QrCode::new(u.as_bytes()).ok().map(|c| c.render::<qrcode::render::svg::Color>().min_dimensions(300, 300).quiet_zone(false).build())
    });
    let (status, message) = match mgr.qr_status() {
        QrStatus::Idle => ("idle", None),
        QrStatus::Waiting => ("waiting", None),
        QrStatus::PasswordNeeded(_) => ("password", None),
        QrStatus::Done(n) => ("done", Some(n)),
        QrStatus::Failed(e) => ("failed", Some(e)),
    };
    serde_json::json!({"status": status, "svg": svg, "message": message})
}

/// Bind refuses non-loopback addresses unless `ALLOW_REMOTE_UI=1` (the UI has no authentication).
pub async fn serve(addr: &str, state: AppState) -> anyhow::Result<()> {
    let sock: SocketAddr = addr.parse()?;
    if !sock.ip().is_loopback() && std::env::var("ALLOW_REMOTE_UI").as_deref() != Ok("1") {
        anyhow::bail!("refusing to bind {sock}: the web UI has no authentication (set ALLOW_REMOTE_UI=1 to override)");
    }
    let listener = tokio::net::TcpListener::bind(sock).await?;
    tracing::info!("web UI on http://{sock}");
    axum::serve(listener, router(state)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use tower_service::Service;

    fn test_state() -> AppState {
        AppState { db: Db::open_in_memory().unwrap(), status: Arc::new(Status::default()), mgr: None, user_id: 1, avatars: std::env::temp_dir().join("tgh-test-avatars-none") }
    }

    fn app() -> Router {
        router(test_state())
    }

    async fn get_json(host: &str, uri: &str) -> (StatusCode, String) {
        let req = axum::http::Request::builder().uri(uri).header("host", host).body(Body::empty()).unwrap();
        let resp = app().call(req).await.unwrap();
        (resp.status(), String::from_utf8(to_bytes(resp.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap())
    }

    #[tokio::test]
    async fn serves_api_and_ui_on_localhost() {
        let (s, b) = get_json("localhost:8787", "/api/overview").await;
        assert_eq!(s, StatusCode::OK);
        assert!(b.contains("\"messages_total\":0") && b.contains("\"userbot_connected\":false"));
        assert_eq!(get_json("127.0.0.1:8787", "/api/messages/daily?days=7").await.1, "[]");
        let (s, b) = get_json("localhost", "/").await;
        assert!(s.is_success() && b.contains("chart-daily"));
    }

    #[tokio::test]
    async fn login_page_and_idle_qr() {
        let (s, b) = get_json("localhost", "/login").await;
        assert!(s.is_success() && b.contains("/api/qr"));
        assert_eq!(get_json("localhost", "/api/qr").await.1, "{\"status\":\"idle\"}");
    }

    async fn post(state: AppState, uri: &str, headers: &[(&str, &str)], body: &str) -> StatusCode {
        let mut b = axum::http::Request::builder().method("POST").uri(uri).header("host", "localhost:8787").header("content-type", "application/json");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        router(state).call(b.body(Body::from(body.to_string())).unwrap()).await.unwrap().status()
    }

    #[tokio::test]
    async fn chats_api_toggles_and_blocks_cross_site() {
        let st = test_state();
        st.db.call(|c| {
            let uid = repo::ensure_user(c, 5)?;
            assert_eq!(uid, 1);
            repo::upsert_contact(c, uid, &repo::ContactRow { peer_id: 10, peer_kind: "channel".into(), is_bot: false, is_archived: false, display_name: "News".into(), username: None })
        }).await.unwrap();
        let ok = [("x-requested-with", "tgh")];
        assert_eq!(post(st.clone(), "/api/contacts/10", &ok, r#"{"news_source":true}"#).await, StatusCode::NO_CONTENT);
        assert_eq!(post(st.clone(), "/api/contacts/999", &ok, r#"{"mirror":false}"#).await, StatusCode::NOT_FOUND);
        // no custom header / foreign origin -> refused, and nothing changed
        assert_eq!(post(st.clone(), "/api/contacts/10", &[], r#"{"mirror":false}"#).await, StatusCode::FORBIDDEN);
        assert_eq!(post(st.clone(), "/api/contacts/10", &[("x-requested-with", "tgh"), ("origin", "https://evil.example")], r#"{"mirror":false}"#).await, StatusCode::FORBIDDEN);
        let full = st.db.call(|c| repo::list_contacts_full(c, 1)).await.unwrap();
        assert!(full[0].is_news_source && full[0].mirror);
    }

    #[tokio::test]
    async fn avatar_serves_file_and_404s() {
        let dir = std::env::temp_dir().join(format!("tgh-avatars-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("7.jpg"), b"\xff\xd8jpeg").unwrap();
        let mut st = test_state();
        st.avatars = dir;
        let get = |st: AppState, uri: &'static str| async move {
            let req = axum::http::Request::builder().uri(uri).header("host", "localhost").body(Body::empty()).unwrap();
            router(st).call(req).await.unwrap().status()
        };
        assert_eq!(get(st.clone(), "/avatars/7").await, StatusCode::OK);
        assert_eq!(get(st.clone(), "/avatars/8").await, StatusCode::NOT_FOUND);
        assert_eq!(get(st, "/avatars/..%2f..%2fetc%2fpasswd").await, StatusCode::BAD_REQUEST); // not an integer
    }

    #[tokio::test]
    async fn rejects_foreign_host() {
        assert_eq!(get_json("evil.example:8787", "/api/overview").await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn refuses_public_bind() {
        let st = test_state();
        assert!(serve("0.0.0.0:0", st).await.is_err());
    }
}
