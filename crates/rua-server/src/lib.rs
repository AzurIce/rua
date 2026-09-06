//! rua-server: the rua daemon (binary `rua`).
//!
//! Owns the graph (`<project-root>/.rua/graph/`) and the agent runtime behind
//! an axum REST + WebSocket API bound to loopback only. See `api` for the
//! endpoint contract and `events` for the WS wire protocol.

pub mod api;
pub mod engine;
pub mod events;
pub mod graphs;
mod runtime;
pub mod script;
pub mod spawn;
pub mod state;
pub mod turn;
pub mod view;

use std::path::PathBuf;

use axum::response::Html;
use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::{DefaultMakeSpan, DefaultOnFailure, DefaultOnResponse, TraceLayer};

use crate::state::SharedState;

const PLACEHOLDER_HTML: &str = "<!doctype html><html><body><h1>rua</h1>\
    <p>Web UI not built (expected <code>crates/rua-ui/dist</code>). \
    The API is available under <code>/api</code>.</p></body></html>";

/// Assemble the full router: `/api` + static UI (or a placeholder page),
/// with permissive CORS for the dev-time UI on another port. HTTP 请求经
/// TraceLayer 记 info 级日志（method/path/status/耗时），失败升 warn。
pub fn build_router(state: SharedState, ui_dist: Option<PathBuf>) -> Router {
    let app = Router::new().nest("/api", api::api_router(state));
    let app = match ui_dist {
        Some(dist) if dist.join("index.html").is_file() => app.fallback_service(
            // SPA fallback: unknown paths serve index.html.
            ServeDir::new(&dist).fallback(ServeFile::new(dist.join("index.html"))),
        ),
        _ => app.fallback(|| async { Html(PLACEHOLDER_HTML) }),
    };
    app.layer(CorsLayer::permissive()).layer(
        TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::new().level(tracing::Level::INFO))
            .on_response(DefaultOnResponse::new().level(tracing::Level::INFO))
            .on_failure(DefaultOnFailure::new().level(tracing::Level::WARN)),
    )
}
