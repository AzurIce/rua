//! Multi-graph management (user-side only): graphs live in
//! `<project>/.rua/graphs/<name>/`, each a self-contained Store
//! (`nodes/` + `journal.jsonl`). The daemon serves exactly one active graph;
//! switching swaps the `Graph` under the state mutex.
//!
//! Safety rules:
//! - Every mutation (create/activate/rename/delete) is rejected with 409
//!   while any turn is in flight — a running turn commits into whatever graph
//!   is current at commit time, so the graph must not move under it.
//! - Delete never removes data: the directory is moved into
//!   `.rua/graphs/.trash/<name>-<millis>`.

use std::path::{Path, PathBuf};

use rua_core::graph::Graph;

use crate::events::ServerEvent;
use crate::state::SharedState;

pub const DEFAULT_GRAPH: &str = "default";
const TRASH_DIR: &str = ".trash";

#[derive(Debug)]
pub enum GraphOpError {
    InvalidName(String),
    NotFound(String),
    AlreadyExists(String),
    Busy,
    Io(std::io::Error),
    Core(rua_core::Error),
}

impl std::fmt::Display for GraphOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(n) => write!(f, "invalid graph name: {n:?}"),
            Self::NotFound(n) => write!(f, "graph not found: {n}"),
            Self::AlreadyExists(n) => write!(f, "graph already exists: {n}"),
            Self::Busy => write!(f, "有进行中的回合，结束后才能操作图"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Core(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for GraphOpError {}

type Result<T> = std::result::Result<T, GraphOpError>;

/// 名字校验：非空、无路径分隔符、不以点开头（`.trash` 保留）、长度有限。
pub fn validate_name(name: &str) -> Result<&str> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && !name.contains(['/', '\\'])
        && name != "..";
    if ok { Ok(name) } else { Err(GraphOpError::InvalidName(name.to_string())) }
}

fn graph_dir(root: &Path, name: &str) -> PathBuf {
    root.join(name)
}

/// List graph names (dot-dirs like `.trash` are skipped), sorted.
pub fn list_graphs(root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    if root.is_dir() {
        for entry in std::fs::read_dir(root).map_err(GraphOpError::Io)? {
            let entry = entry.map_err(GraphOpError::Io)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir() && !name.starts_with('.') {
                names.push(name);
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Boot-time migration: legacy `<project>/.rua/graph/` becomes
/// `graphs/default` (rename, no copy).
pub fn migrate_legacy(rua_dir: &Path) -> Result<()> {
    let legacy = rua_dir.join("graph");
    let default = rua_dir.join("graphs").join(DEFAULT_GRAPH);
    if legacy.is_dir() && !default.exists() {
        std::fs::create_dir_all(default.parent().expect("graphs dir")).map_err(GraphOpError::Io)?;
        std::fs::rename(&legacy, &default).map_err(GraphOpError::Io)?;
        eprintln!("rua: migrated {} -> {}", legacy.display(), default.display());
    }
    Ok(())
}

fn ensure_not_busy(graph: &Graph) -> Result<()> {
    if graph.cursors.in_flight_all().next().is_some() {
        return Err(GraphOpError::Busy);
    }
    Ok(())
}

/// Swap the active graph under the mutex: drop the old Graph handle, open the
/// new one, clear per-graph runtime state, and notify all clients to resync.
async fn swap_active(state: &SharedState, name: &str, graph: Graph) {
    {
        let mut slot = state.graph.lock().await;
        *slot = graph;
        state.cancels.lock().await.clear();
        *state.current_graph.lock().await = name.to_string();
    }
    state.broadcast(ServerEvent::GraphSwitched {
        name: name.to_string(),
    });
}

pub async fn create_graph(state: &SharedState, name: &str) -> Result<()> {
    let name = validate_name(name)?;
    let dir = graph_dir(&state.graphs_root, name);
    if dir.exists() {
        return Err(GraphOpError::AlreadyExists(name.to_string()));
    }
    {
        let graph = state.graph.lock().await;
        ensure_not_busy(&graph)?;
    }
    let graph = Graph::open(&dir).map_err(GraphOpError::Core)?;
    swap_active(state, name, graph).await;
    Ok(())
}

pub async fn activate_graph(state: &SharedState, name: &str) -> Result<()> {
    let name = validate_name(name)?;
    if *state.current_graph.lock().await == name {
        return Ok(());
    }
    let dir = graph_dir(&state.graphs_root, name);
    if !dir.is_dir() {
        return Err(GraphOpError::NotFound(name.to_string()));
    }
    {
        let graph = state.graph.lock().await;
        ensure_not_busy(&graph)?;
    }
    let graph = Graph::open(&dir).map_err(GraphOpError::Core)?;
    swap_active(state, name, graph).await;
    Ok(())
}

pub async fn rename_graph(state: &SharedState, from: &str, to: &str) -> Result<()> {
    let from = validate_name(from)?;
    let to = validate_name(to)?;
    if from == to {
        return Ok(());
    }
    let src = graph_dir(&state.graphs_root, from);
    let dst = graph_dir(&state.graphs_root, to);
    if !src.is_dir() {
        return Err(GraphOpError::NotFound(from.to_string()));
    }
    if dst.exists() {
        return Err(GraphOpError::AlreadyExists(to.to_string()));
    }
    {
        let graph = state.graph.lock().await;
        ensure_not_busy(&graph)?;
    }
    std::fs::rename(&src, &dst).map_err(GraphOpError::Io)?;
    // 目录rename不影响已打开的 Graph（fd/路径只在写入时用……实际 Store
    // 持有路径！所以当前图被重命名后必须重新打开）。
    if *state.current_graph.lock().await == from {
        let graph = Graph::open(&dst).map_err(GraphOpError::Core)?;
        swap_active(state, to, graph).await;
    }
    Ok(())
}

pub async fn delete_graph(state: &SharedState, name: &str) -> Result<()> {
    let name = validate_name(name)?;
    let dir = graph_dir(&state.graphs_root, name);
    if !dir.is_dir() {
        return Err(GraphOpError::NotFound(name.to_string()));
    }
    let is_current = *state.current_graph.lock().await == name;
    {
        let graph = state.graph.lock().await;
        ensure_not_busy(&graph)?;
    }

    // 移入回收站而不是真删。
    let trash = state.graphs_root.join(TRASH_DIR);
    std::fs::create_dir_all(&trash).map_err(GraphOpError::Io)?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    std::fs::rename(&dir, trash.join(format!("{name}-{millis}"))).map_err(GraphOpError::Io)?;

    if is_current {
        // 切到剩下的第一个图；一个都不剩就开一个新的 default。
        let remaining = list_graphs(&state.graphs_root)?;
        let next = remaining.first().cloned().unwrap_or_else(|| DEFAULT_GRAPH.to_string());
        let graph = Graph::open(graph_dir(&state.graphs_root, &next)).map_err(GraphOpError::Core)?;
        swap_active(state, &next, graph).await;
    }
    Ok(())
}
