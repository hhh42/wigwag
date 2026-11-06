//! The HTTP side, at signal-cli's paths: `/api/v1/check`, `/api/v1/rpc`, `/api/v1/events`.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::stream::{self, Stream, StreamExt};
use tokio_stream::wrappers::BroadcastStream;

use crate::events::Hub;
use crate::rpc::{self, Backend};

pub struct AppState<B> {
    pub backend: B,
    pub hub: Arc<Hub>,
    /// False while the Signal connection is down for good (e.g. this device was unlinked).
    pub healthy: Arc<AtomicBool>,
}

pub fn router<B: Backend>(state: Arc<AppState<B>>) -> Router {
    Router::new()
        .route("/api/v1/check", get(check::<B>))
        .route("/api/v1/rpc", post(rpc_call::<B>))
        .route("/api/v1/events", get(events::<B>))
        .with_state(state)
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
async fn events<B: Backend>(
    State(s): State<Arc<AppState<B>>>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let (waiting, rx) = s.hub.subscribe();
    let live = BroadcastStream::new(rx).filter_map(|r| async move {
        r.inspect_err(|e| tracing::warn!("events subscriber fell behind: {e}"))
            .ok()
    });
    let stream = stream::iter(waiting)
        .chain(live)
        .map(|data| Ok(SseEvent::default().event("receive").data(&*data)));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
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
        let hub = Arc::new(Hub::new(8));
        let state = Arc::new(AppState {
            backend: Fake::default(),
            hub: hub.clone(),
            healthy: Arc::new(AtomicBool::new(healthy)),
        });
        (router(state), hub)
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
