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
        let origin_ok = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()).is_none_or(|o| {
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
        .route(
            "/api/contacts",
            get(|State(s): State<AppState>| async move {
                let uid = s.user_id;
                ok(s.db.call(move |c| Ok(serde_json::to_value(repo::list_contacts_full(c, uid)?).unwrap_or_default())).await)
            }),
        )
        .route("/api/contacts/{peer_id}", axum::routing::post(update_contact))
        .route("/avatars/{peer_id}", get(avatar))
        .route("/login", get(|| asset(include_str!("../web/login.html"), "text/html; charset=utf-8")))
        .route("/api/selftest", axum::routing::post(selftest))
        .route("/api/qr", get(|State(s): State<AppState>| async move { Json(qr_json(&s)) }))
        .route(
            "/api/overview",
            get(|State(s): State<AppState>| async move {
                let up = s.status.userbot_connected.load(Ordering::Relaxed);
                ok(s.db.call(move |c| a::overview(c, up)).await)
            }),
        )
        .route(
            "/api/messages/daily",
            get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
                let d = q.days.unwrap_or(30);
                ok(s.db.call(move |c| a::messages_daily(c, d)).await)
            }),
        )
        .route(
            "/api/messages/hourly",
            get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
                let d = q.days.unwrap_or(30);
                ok(s.db.call(move |c| a::messages_hourly(c, d)).await)
            }),
        )
        .route(
            "/api/messages/top_chats",
            get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
                let (d, l) = (q.days.unwrap_or(30), q.limit.unwrap_or(10));
                ok(s.db.call(move |c| a::top_chats(c, d, l)).await)
            }),
        )
        .route(
            "/api/llm/daily",
            get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
                let d = q.days.unwrap_or(30);
                ok(s.db.call(move |c| a::llm_daily(c, d)).await)
            }),
        )
        .route(
            "/api/llm/by_purpose",
            get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
                let d = q.days.unwrap_or(30);
                ok(s.db.call(move |c| a::llm_by_purpose(c, d)).await)
            }),
        )
        .route(
            "/api/autoreply/recent",
            get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
                let l = q.limit.unwrap_or(50);
                ok(s.db.call(move |c| a::autoreply_recent(c, l)).await)
            }),
        )
        .route(
            "/api/commitments",
            get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
                let st = q.status.unwrap_or_else(|| "open".into());
                ok(s.db.call(move |c| a::commitments(c, &st)).await)
            }),
        )
        .route(
            "/api/events",
            get(|State(s): State<AppState>, Query(q): Query<Q>| async move {
                let l = q.limit.unwrap_or(100);
                ok(s.db.call(move |c| a::events(c, l)).await)
            }),
        )
        .layer(middleware::from_fn(csrf_guard))
        .layer(middleware::from_fn(local_host_only))
        .with_state(state)
}

/// Live end-to-end check of Telegram, the LLM chain, the DB and the bot (see `tgh_tg::selftest`).
async fn selftest(State(s): State<AppState>) -> Response {
    let Some(mgr) = &s.mgr else { return (StatusCode::SERVICE_UNAVAILABLE, "no Telegram side in demo mode").into_response() };
    let checks = tgh_tg::selftest::run(mgr).await;
    let all_ok = checks.iter().all(|c| c.ok);
    (if all_ok { StatusCode::OK } else { StatusCode::INTERNAL_SERVER_ERROR }, Json(serde_json::json!({"ok": all_ok, "checks": checks})))
        .into_response()
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
        qrcode::QrCode::new(u.as_bytes())
            .ok()
            .map(|c| c.render::<qrcode::render::svg::Color>().min_dimensions(300, 300).quiet_zone(false).build())
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
        AppState {
            db: Db::open_in_memory().unwrap(),
            status: Arc::new(Status::default()),
            mgr: None,
            user_id: 1,
            avatars: std::env::temp_dir().join("tgh-test-avatars-none"),
        }
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
        let mut b = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("host", "localhost:8787")
            .header("content-type", "application/json");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        router(state).call(b.body(Body::from(body.to_string())).unwrap()).await.unwrap().status()
    }

    #[tokio::test]
    async fn chats_api_toggles_and_blocks_cross_site() {
        let st = test_state();
        st.db
            .call(|c| {
                let uid = repo::ensure_user(c, 5)?;
                assert_eq!(uid, 1);
                repo::upsert_contact(
                    c,
                    uid,
                    &repo::ContactRow {
                        peer_id: 10,
                        peer_kind: "channel".into(),
                        is_bot: false,
                        is_archived: false,
                        display_name: "News".into(),
                        username: None,
                    },
                )
            })
            .await
            .unwrap();
        let ok = [("x-requested-with", "tgh")];
        assert_eq!(post(st.clone(), "/api/contacts/10", &ok, r#"{"news_source":true}"#).await, StatusCode::NO_CONTENT);
        assert_eq!(post(st.clone(), "/api/contacts/999", &ok, r#"{"mirror":false}"#).await, StatusCode::NOT_FOUND);
        // no custom header / foreign origin -> refused, and nothing changed
        assert_eq!(post(st.clone(), "/api/contacts/10", &[], r#"{"mirror":false}"#).await, StatusCode::FORBIDDEN);
        assert_eq!(
            post(st.clone(), "/api/contacts/10", &[("x-requested-with", "tgh"), ("origin", "https://evil.example")], r#"{"mirror":false}"#)
                .await,
            StatusCode::FORBIDDEN
        );
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
        assert_eq!(get(st, "/avatars/..%2f..%2fetc%2fpasswd").await, StatusCode::BAD_REQUEST);
        // not an integer
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

    async fn status_of(method: &str, uri: &str, headers: &[(&str, &str)], body: Vec<u8>) -> StatusCode {
        let mut b = axum::http::Request::builder().method(method).uri(uri).header("host", "localhost:8787");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        app().call(b.body(Body::from(body)).unwrap()).await.unwrap().status()
    }

    #[tokio::test]
    async fn hostile_query_parameters_never_crash() {
        // non-numeric -> 400 from the extractor; numeric extremes are clamped in SQL helpers
        for uri in [
            "/api/messages/daily?days=abc",
            "/api/messages/daily?days=",
            "/api/messages/daily?days=1.5",
            "/api/llm/daily?days=%00",
            "/api/messages/top_chats?limit=NaN",
            "/api/events?limit=99999999999999999999999",
            "/api/messages/daily?days=7&days=8", // duplicate key
        ] {
            assert_eq!(status_of("GET", uri, &[], vec![]).await, StatusCode::BAD_REQUEST, "{uri}");
        }
        for uri in [
            "/api/messages/daily?days=-5",
            "/api/messages/daily?days=0",
            "/api/messages/daily?days=9223372036854775807",
            "/api/llm/by_purpose?days=-9223372036854775808",
            "/api/events?limit=0",
            "/api/events?limit=-1",
            "/api/autoreply/recent?limit=9223372036854775807",
            "/api/messages/top_chats?days=7&limit=-3",
            "/api/commitments?status=%27%3B%20DROP%20TABLE%20users%3B--",
            "/api/commitments?status=%F0%9F%98%80",
        ] {
            assert_eq!(status_of("GET", uri, &[], vec![]).await, StatusCode::OK, "{uri}");
        }
    }

    #[tokio::test]
    async fn routing_methods_and_bodies() {
        assert_eq!(status_of("GET", "/nope", &[], vec![]).await, StatusCode::NOT_FOUND);
        assert_eq!(status_of("GET", "/../../etc/passwd", &[], vec![]).await, StatusCode::NOT_FOUND);
        assert_eq!(status_of("GET", "/avatars/..%2f..%2fetc%2fpasswd", &[], vec![]).await, StatusCode::BAD_REQUEST);
        assert_eq!(status_of("GET", "/avatars/99999999999999999999", &[], vec![]).await, StatusCode::BAD_REQUEST);
        assert_eq!(status_of("DELETE", "/api/overview", &[("x-requested-with", "tgh")], vec![]).await, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(status_of("POST", "/api/overview", &[("x-requested-with", "tgh")], vec![]).await, StatusCode::METHOD_NOT_ALLOWED);
        let ok = [("x-requested-with", "tgh"), ("content-type", "application/json")];
        assert_eq!(status_of("POST", "/api/contacts/1", &ok, b"{not json".to_vec()).await, StatusCode::BAD_REQUEST);
        assert_eq!(status_of("POST", "/api/contacts/1", &ok, br#"{"mirror":"yes"}"#.to_vec()).await, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(status_of("POST", "/api/contacts/abc", &ok, b"{}".to_vec()).await, StatusCode::BAD_REQUEST);
        assert_eq!(
            status_of("POST", "/api/contacts/1", &[("x-requested-with", "tgh")], b"{}".to_vec()).await,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        // oversized body is rejected by axum's default limit instead of being buffered
        assert_eq!(status_of("POST", "/api/contacts/1", &ok, vec![b' '; 3 * 1024 * 1024]).await, StatusCode::PAYLOAD_TOO_LARGE);
        // Host header variants
        for host in ["", "localhost.evil.com", "evil.com:8787", "127.0.0.1.evil.com", "[::1]x", "localhost:8787@evil.com"] {
            let req = axum::http::Request::builder().uri("/api/overview").header("host", host).body(Body::empty()).unwrap();
            assert_eq!(app().call(req).await.unwrap().status(), StatusCode::FORBIDDEN, "host {host:?}");
        }
        let req = axum::http::Request::builder().uri("/api/overview").body(Body::empty()).unwrap(); // no Host at all
        assert_eq!(app().call(req).await.unwrap().status(), StatusCode::FORBIDDEN);
        for host in ["localhost", "localhost:1", "127.0.0.1:65535", "[::1]:8787"] {
            let req = axum::http::Request::builder().uri("/api/overview").header("host", host).body(Body::empty()).unwrap();
            assert_eq!(app().call(req).await.unwrap().status(), StatusCode::OK, "host {host:?}");
        }
    }

    #[tokio::test]
    async fn concurrent_requests_do_not_deadlock() {
        let st = test_state();
        st.db
            .call(|c| {
                let uid = repo::ensure_user(c, 5)?;
                for i in 0..50 {
                    repo::save_message(
                        c,
                        uid,
                        &repo::MessageRow {
                            peer_id: 1,
                            message_id: i,
                            sender_id: None,
                            sender_name: None,
                            is_outgoing: i % 2 == 0,
                            date: "2026-09-01 10:00:00".into(),
                            kind: "text".into(),
                            text: Some("x".into()),
                        },
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let router = router(st.clone());
        let mut tasks = Vec::new();
        for i in 0..300 {
            let mut r = router.clone();
            tasks.push(tokio::spawn(async move {
                let uri = [
                    "/api/overview",
                    "/api/messages/daily?days=30",
                    "/api/messages/hourly?days=7",
                    "/api/events?limit=10",
                    "/api/contacts",
                ][i % 5];
                let req = axum::http::Request::builder().uri(uri).header("host", "localhost").body(Body::empty()).unwrap();
                r.call(req).await.unwrap().status()
            }));
        }
        for t in tasks {
            assert_eq!(tokio::time::timeout(std::time::Duration::from_secs(20), t).await.expect("deadlock").unwrap(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn xss_payloads_are_returned_as_inert_json() {
        let st = test_state();
        let evil = "<img src=x onerror=alert(1)>\"'</script>";
        st.db
            .call(move |c| {
                let uid = repo::ensure_user(c, 5)?;
                repo::upsert_contact(
                    c,
                    uid,
                    &repo::ContactRow {
                        peer_id: 1,
                        peer_kind: "user".into(),
                        is_bot: false,
                        is_archived: false,
                        display_name: evil.into(),
                        username: Some(evil.into()),
                    },
                )
            })
            .await
            .unwrap();
        let req = axum::http::Request::builder().uri("/api/contacts").header("host", "localhost").body(Body::empty()).unwrap();
        let resp = router(st).call(req).await.unwrap();
        assert!(resp.headers()[header::CONTENT_TYPE].to_str().unwrap().starts_with("application/json"));
        let body = String::from_utf8(to_bytes(resp.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v[0]["name"], evil); // round-trips intact; escaping is the frontend's job (see scripts/js-smoke.mjs)
    }
}
