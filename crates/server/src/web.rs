//! Headless dashboard: JSON analytics API + static UI, loopback only.

use std::{
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
};

use axum::{
    extract::{Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::Value;
use tgh_core::{db::analytics as a, db::Db, Status};

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub status: Arc<Status>,
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
        .layer(middleware::from_fn(local_host_only))
        .with_state(state)
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

    fn app() -> Router {
        router(AppState { db: Db::open_in_memory().unwrap(), status: Arc::new(Status::default()) })
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
    async fn rejects_foreign_host() {
        assert_eq!(get_json("evil.example:8787", "/api/overview").await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn refuses_public_bind() {
        let st = AppState { db: Db::open_in_memory().unwrap(), status: Arc::new(Status::default()) };
        assert!(serve("0.0.0.0:0", st).await.is_err());
    }
}
