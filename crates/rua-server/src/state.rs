//! Shared daemon state: the graph (single writer under one mutex), the
//! engine, the WS event bus, and cancellation tokens for in-flight turns.

use std::collections::HashMap;
use std::sync::Arc;

use rua_graph::graph::Graph;
use rua_graph::id::CursorId;
use tokio::sync::{broadcast, Mutex};
use tokio_util::sync::CancellationToken;

use crate::engine::AgentEngine;
use crate::events::ServerEvent;

/// Capacity of the broadcast bus. Lagging WS clients skip missed events.
pub const EVENT_BUS_CAPACITY: usize = 1024;

pub struct AppState {
    /// Single writer: every graph mutation takes this lock.
    pub graph: Mutex<Graph>,
    /// 所有图的根目录（`<project>/.rua/graphs/`）。
    pub graphs_root: std::path::PathBuf,
    /// 当前活跃图名。
    pub current_graph: Mutex<String>,
    pub engine: Arc<dyn AgentEngine>,
    /// JSON-serialized `ServerEvent`s; all WS clients subscribe to this bus.
    pub events: broadcast::Sender<String>,
    /// Cancellation token per in-flight turn, keyed by cursor.
    pub cancels: Mutex<HashMap<CursorId, CancellationToken>>,
    /// 默认模型（默认 provider 的裸模型名；model ref 规则见
    /// `rua_engine::config::parse_model_ref`）。
    pub default_model: String,
    /// 全部 provider（默认在前，名为 "default"）：/api/models 聚合代理和
    /// Engine 的 model ref 解析都要用。
    pub providers: Vec<(String, rua_engine::config::ProviderConfig)>,
    /// /api/models 按 provider 的缓存：provider 不可达时代理请求会挂到
    /// 超时（数秒），UI 每次刷新都调这个端点，不能每次都等。
    pub models_cache: Mutex<HashMap<String, (std::time::Instant, Vec<String>)>>,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    pub fn new(
        graph: Graph,
        engine: Arc<dyn AgentEngine>,
        default_model: String,
        graphs_root: std::path::PathBuf,
        current_graph: String,
        providers: Vec<(String, rua_engine::config::ProviderConfig)>,
    ) -> Self {
        Self {
            graph: Mutex::new(graph),
            graphs_root,
            current_graph: Mutex::new(current_graph),
            engine,
            events: broadcast::channel(EVENT_BUS_CAPACITY).0,
            cancels: Mutex::new(HashMap::new()),
            default_model,
            providers,
            models_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Broadcast an event to all WS subscribers; no subscribers is fine.
    pub fn broadcast(&self, event: ServerEvent) {
        let _ = self.events.send(event.to_json());
    }
}
