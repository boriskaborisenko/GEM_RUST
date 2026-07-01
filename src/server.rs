//! Read-only HTTP dashboard (replaces terminal rendering in `--server` mode).

use crate::dashboard::DashboardSnapshot;
use axum::{
    extract::State,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Json,
    },
    routing::get,
    Router,
};
use futures_util::stream::Stream;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::StreamExt;

#[derive(Clone)]
pub struct HttpServerState {
    pub snapshot_tx: watch::Sender<Arc<DashboardSnapshot>>,
    pub started_at_ms: i64,
}

pub async fn run(bind: SocketAddr, snapshot_tx: watch::Sender<Arc<DashboardSnapshot>>) {
    let state = Arc::new(HttpServerState {
        snapshot_tx,
        started_at_ms: crate::client::get_now_ms(),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/state", get(api_state))
        .route("/api/events", get(api_events))
        .route("/api/health", get(api_health))
        .with_state(state);

    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[SERVER] Failed to bind {bind}: {e}");
            return;
        }
    };
    eprintln!("[SERVER] Dashboard at http://{bind} (read-only, stop via terminal)");

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("[SERVER] HTTP server error: {e}");
    }
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/dashboard.html"))
}

async fn api_state(State(state): State<Arc<HttpServerState>>) -> Json<Arc<DashboardSnapshot>> {
    Json(state.snapshot_tx.borrow().clone())
}

async fn api_health(State(state): State<Arc<HttpServerState>>) -> impl IntoResponse {
    let snap = state.snapshot_tx.borrow();
    let now = crate::client::get_now_ms();
    Json(serde_json::json!({
        "ok": true,
        "uptime_ms": now.saturating_sub(state.started_at_ms),
        "shutdown_pending": snap.meta.shutdown_pending,
        "updated_at_ms": snap.updated_at_ms,
    }))
}

async fn api_events(
    State(state): State<Arc<HttpServerState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.snapshot_tx.subscribe();
    let stream = WatchStream::new(rx).map(|snap| {
        let data = serde_json::to_string(&*snap).unwrap_or_else(|_| "{}".to_string());
        Ok(Event::default().data(data))
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    )
}
