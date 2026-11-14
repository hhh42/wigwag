//! The HTTP side, at signal-cli's paths: `/api/v1/check`, `/api/v1/rpc`, `/api/v1/events`.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::stream::{self, StreamExt};
use tokio_stream::wrappers::BroadcastStream;

use crate::events::Hub;
use crate::rpc::{self, Backend};

pub struct AppState<B> {
    pub backend: B,
    pub hub: Arc<Hub>,
    /// False while the Signal connection is down for good (e.g. this device was unlinked).
    pub healthy: Arc<AtomicBool>,
    pub guard: Guard,
    /// Open `/api/v1/events` streams allowed at once.
    pub streams: Arc<tokio::sync::Semaphore>,
}

/// More event streams than any real setup needs, few enough that a runaway client
/// can't exhaust memory or file descriptors.
pub const MAX_STREAMS: usize = 16;

/// Who may talk to the API. The API itself has no notion of users, so this is the
/// only gate between the port and "read every message, send as this account".
#[derive(Debug, Clone, Default)]
pub struct Guard {
    /// When set, every request needs `Authorization: Bearer <token>`.
    pub token: Option<String>,
    /// Accept only `Host: localhost`/`127.0.0.1`/`[::1]`. On for loopback binds: it
    /// stops DNS rebinding, where a web page re-points its own hostname at 127.0.0.1.
    pub loopback_host_only: bool,
}

pub fn router<B: Backend>(state: Arc<AppState<B>>) -> Router {
    let guard = Arc::new(state.guard.clone());
    Router::new()
        .route("/api/v1/check", get(check::<B>))
        .route("/api/v1/rpc", post(rpc_call::<B>))
        .route("/api/v1/events", get(events::<B>))
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            let guard = guard.clone();
            async move {
                let mut resp = match guard.refuse(req.headers()) {
                    Some(status) => status.into_response(),
                    None => next.run(req).await,
                };
                // Messages must never sit in a cache or be sniffed into something else.
                let h = resp.headers_mut();
                h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                h.insert(
                    header::X_CONTENT_TYPE_OPTIONS,
                    HeaderValue::from_static("nosniff"),
                );
                resp
            }
        }))
        .with_state(state)
}

impl Guard {
    fn refuse(&self, h: &HeaderMap) -> Option<StatusCode> {
        // Browsers send Origin on every cross-site POST, fetch and EventSource; bots,
        // curl and signal-cli clients don't. Without this, any web page you open could
        // POST a `send` (a no-cors text/plain POST needs no preflight) to localhost.
        let browser = h.contains_key(header::ORIGIN)
            || h.keys().any(|k| k.as_str().starts_with("sec-fetch-"));
        if browser {
            tracing::warn!("refused a request carrying an Origin header (a web page?)");
            return Some(StatusCode::FORBIDDEN);
        }
        if self.loopback_host_only {
            let host = h
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !is_loopback_host(host) {
                tracing::warn!("refused a request for Host {host:?}");
                return Some(StatusCode::FORBIDDEN);
            }
        }
        if let Some(want) = &self.token {
            let got = h
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or("");
            if !constant_time_eq(got.as_bytes(), want.as_bytes()) {
                return Some(StatusCode::UNAUTHORIZED);
            }
        }
        None
    }
}

fn is_loopback_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map_or(host, |(h, _)| h)
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn check<B: Backend>(State(s): State<Arc<AppState<B>>>) -> StatusCode {
    if s.healthy.load(Ordering::Relaxed) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn rpc_call<B: Backend>(State(s): State<Arc<AppState<B>>>, body: Bytes) -> Response {
    match rpc::handle(&s.backend, &body).await {
        Some(v) => ([(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

/// `?account=` is accepted and ignored: one daemon serves one account.
async fn events<B: Backend>(State(s): State<Arc<AppState<B>>>) -> Response {
    let Ok(permit) = s.streams.clone().try_acquire_owned() else {
        tracing::warn!("refused an event stream: {MAX_STREAMS} already open");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let (waiting, rx) = s.hub.subscribe();
    let live = BroadcastStream::new(rx).filter_map(|r| async move {
        r.inspect_err(|e| tracing::warn!("events subscriber fell behind: {e}"))
            .ok()
    });
    let stream = stream::iter(waiting)
        .chain(live)
        // The permit lives as long as the stream, and is released when it ends.
        .map(move |data| {
            let _held = &permit;
            Ok::<_, Infallible>(SseEvent::default().event("receive").data(&*data))
        });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::tests::Fake;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app(healthy: bool) -> (Router, Arc<Hub>) {
        guarded(healthy, Guard::default())
    }

    fn guarded(healthy: bool, guard: Guard) -> (Router, Arc<Hub>) {
        let hub = Arc::new(Hub::new(8));
        let state = Arc::new(AppState {
            backend: Fake::default(),
            hub: hub.clone(),
            healthy: Arc::new(AtomicBool::new(healthy)),
            guard,
            streams: Arc::new(tokio::sync::Semaphore::new(2)),
        });
        (router(state), hub)
    }

    async fn status(app: Router, req: Request<Body>) -> StatusCode {
        app.oneshot(req).await.unwrap().status()
    }

    fn rpc_req() -> axum::http::request::Builder {
        Request::post("/api/v1/rpc").header("content-type", "text/plain")
    }
    const VERSION: &str = r#"{"jsonrpc":"2.0","method":"version","id":1}"#;

    #[tokio::test]
    async fn refuses_browsers() {
        for (k, v) in [
            ("origin", "https://evil.example"),
            ("sec-fetch-site", "cross-site"),
        ] {
            let (app, _) = app(true);
            let req = rpc_req().header(k, v).body(Body::from(VERSION)).unwrap();
            assert_eq!(status(app, req).await, StatusCode::FORBIDDEN, "{k}");
        }
    }

    #[tokio::test]
    async fn over_a_real_unix_socket() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let path =
            std::env::temp_dir().join(format!("wigwag-test-http-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let g = Guard {
            token: Some("0123456789abcdef".into()),
            loopback_host_only: true,
        };
        let (app, _) = guarded(true, g);
        tokio::spawn(async move { axum::serve(listener, app).await });
        let ask = |extra: &'static str| {
            let path = path.clone();
            async move {
                let mut c = tokio::net::UnixStream::connect(&path).await.unwrap();
                let req = format!(
                    "POST /api/v1/rpc HTTP/1.1\r\nHost: localhost\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{VERSION}",
                    VERSION.len()
                );
                c.write_all(req.as_bytes()).await.unwrap();
                let mut out = String::new();
                c.read_to_string(&mut out).await.unwrap();
                out.lines().next().unwrap().to_string()
            }
        };
        assert!(ask("").await.contains("401"));
        assert!(
            ask("Authorization: Bearer 0123456789abcdef\r\n")
                .await
                .contains("200")
        );
        let browser = "Authorization: Bearer 0123456789abcdef\r\nOrigin: https://evil.example\r\n";
        assert!(ask(browser).await.contains("403"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn responses_are_not_cacheable() {
        let (app, _) = app(true);
        let r = app
            .oneshot(rpc_req().body(Body::from(VERSION)).unwrap())
            .await
            .unwrap();
        assert_eq!(r.headers()["cache-control"], "no-store");
        assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    }

    #[tokio::test]
    async fn caps_open_event_streams() {
        let (app, _) = app(true);
        let get = || Request::get("/api/v1/events").body(Body::empty()).unwrap();
        let a = app.clone().oneshot(get()).await.unwrap();
        let b = app.clone().oneshot(get()).await.unwrap();
        assert_eq!(b.status(), StatusCode::OK);
        assert_eq!(
            status(app.clone(), get()).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        drop(a);
        assert_eq!(
            status(app, get()).await,
            StatusCode::OK,
            "a closed stream frees its slot"
        );
        drop(b);
    }

    #[tokio::test]
    async fn refuses_rebound_hostnames_on_loopback() {
        let g = Guard {
            token: None,
            loopback_host_only: true,
        };
        for (host, want) in [
            ("evil.example:8080", StatusCode::FORBIDDEN),
            ("", StatusCode::FORBIDDEN),
            ("127.0.0.1:8080", StatusCode::OK),
            ("localhost:8080", StatusCode::OK),
            ("[::1]:8080", StatusCode::OK),
        ] {
            let (app, _) = guarded(true, g.clone());
            let req = rpc_req()
                .header("host", host)
                .body(Body::from(VERSION))
                .unwrap();
            assert_eq!(status(app, req).await, want, "Host {host:?}");
        }
    }

    #[tokio::test]
    async fn token_is_required_when_set() {
        let g = Guard {
            token: Some("s3cret".into()),
            loopback_host_only: false,
        };
        for (auth, want) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("Bearer wrong!"), StatusCode::UNAUTHORIZED),
            (Some("Bearer s3cret"), StatusCode::OK),
        ] {
            let (app, _) = guarded(true, g.clone());
            let mut req = rpc_req();
            if let Some(a) = auth {
                req = req.header("authorization", a);
            }
            let r = req.body(Body::from(VERSION)).unwrap();
            assert_eq!(status(app, r).await, want, "{auth:?}");
        }
    }

    #[tokio::test]
    async fn check_reflects_health() {
        let (ok, _) = app(true);
        let r = ok
            .oneshot(Request::get("/api/v1/check").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let (down, _) = app(false);
        let r = down
            .oneshot(Request::get("/api/v1/check").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn rpc_over_http() {
        let (app, _) = app(true);
        let req = Request::post("/api/v1/rpc")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","method":"version","id":"v"}"#,
            ))
            .unwrap();
        let r = app.oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = r.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["id"], "v");
        assert!(
            v["result"]["version"]
                .as_str()
                .unwrap()
                .starts_with("wigwag")
        );
    }

    #[tokio::test]
    async fn events_stream_as_sse_receive_lines() {
        let (app, hub) = app(true);
        hub.publish(r#"{"envelope":{"source":"+1"}}"#.into());
        let r = app
            .oneshot(
                Request::get("/api/v1/events?account=%2B1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.headers()["content-type"], "text/event-stream");
        let mut body = r.into_body();
        let frame = body.frame().await.unwrap().unwrap();
        let text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        assert_eq!(
            text,
            "event: receive\ndata: {\"envelope\":{\"source\":\"+1\"}}\n\n"
        );
    }
}
