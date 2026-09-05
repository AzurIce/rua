//! Multi-graph management (user-side only): graphs live in
//! `<project>/.rua/graphs/<name>/`, each a self-contained Store
//! (`journal.jsonl` + `turns/` + `contexts/`). The daemon serves exactly one
//! active graph; switching swaps the `Graph` under the state mutex.
//!
//! Safety rules:
//! - Every mutation (create/activate/rename/delete) is rejected with 409
//!   while any turn is in flight — a running turn commits into whatever graph
//!   is current at commit time, so the graph must not move under it.
//! - Delete never removes data: the directory is moved into
//!   `.rua/graphs/.trash/<name>-<millis>`.

use std::path::{Path, PathBuf};

use rua_graph::graph::Graph;

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
    Core(rua_graph::Error),
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

/// Duplicate a graph: recursive directory copy (nodes are immutable files,
/// so a plain copy is a perfect clone). Does not switch the active graph.
pub fn duplicate_graph(state: &SharedState, from: &str, to: &str) -> Result<()> {
    let from = validate_name(from)?;
    let to = validate_name(to)?;
    let src = graph_dir(&state.graphs_root, from);
    let dst = graph_dir(&state.graphs_root, to);
    if !src.is_dir() {
        return Err(GraphOpError::NotFound(from.to_string()));
    }
    if dst.exists() {
        return Err(GraphOpError::AlreadyExists(to.to_string()));
    }
    copy_dir(&src, &dst)?;
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(GraphOpError::Io)?;
    for entry in std::fs::read_dir(src).map_err(GraphOpError::Io)? {
        let entry = entry.map_err(GraphOpError::Io)?;
        let target = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target).map_err(GraphOpError::Io)?;
        }
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

/// 跨图克隆子树：把 `from_graph` 里选中的节点（含其后继子树、以及它们
/// 引用的 context 材料节点）以新 id 复制进**当前活跃图**。
///
/// 语义：
/// - 克隆集 = 选中节点 + 结构后继（parent 链向下）+ 每个 Turn 回应的 Input
///   （相继边必选——克隆不能造出 parentless Turn）+ 一层被引用的 context 节点；
/// - parent / context_refs / created_by / sources 只保留指向克隆集内部的边
///   （重映射到新 id），指向集外的引用一律丢弃（溯源断链是可接受的：跨图复制
///   本来就是「脱离上下文另起炉灶」）；Context.distilled_from 不在集内则原样
///   保留（允许悬空，同 sources）；
/// - cursor 不克隆；节点本体不可变所以 meta 原样复制，id/归属边换新，正文经
///   数据面重写入目标图（Turn 的 Init 锚点不复制——与直接提交路径同形）；
/// - 返回克隆的节点数。
pub async fn clone_subgraph(
    state: &SharedState,
    from_graph: &str,
    selected: Vec<rua_graph::Ulid>,
) -> Result<usize> {
    use rua_graph::node::{Context, ContextData, Input, Meta, Turn, TurnData};
    use rua_graph::{NodeId, Ulid};

    let from_graph = validate_name(from_graph)?;
    let src_dir = graph_dir(&state.graphs_root, from_graph);
    if !src_dir.is_dir() {
        return Err(GraphOpError::NotFound(from_graph.to_string()));
    }
    if selected.is_empty() {
        return Ok(0);
    }
    let src = Graph::open(&src_dir).map_err(GraphOpError::Core)?;

    // 1. 扩张克隆集：选中节点 + 全部结构后继。
    let mut set: std::collections::HashSet<Ulid> = selected.into_iter().collect();
    let mut stack: Vec<Ulid> = set.iter().copied().collect();
    while let Some(id) = stack.pop() {
        for &child in src.children(id) {
            if set.insert(child) {
                stack.push(child);
            }
        }
    }
    // 每个 Turn 回应的 Input 必须随集（Turn.parent 必选）。
    let snapshot: Vec<Ulid> = set.iter().copied().collect();
    for id in snapshot {
        if let Some(Meta::Turn(t)) = src.meta(id) {
            set.insert(t.kind.parent.raw());
        }
    }
    // 一层被引用的 context 材料。
    let snapshot: Vec<Ulid> = set.iter().copied().collect();
    for id in snapshot {
        if let Some(meta) = src.meta(id) {
            for r in meta.material_refs() {
                if src.meta(r.raw()).is_some() {
                    set.insert(r.raw());
                }
            }
        }
    }

    // 2. 按 created_at 升序（父必先于子 commit）依次重建节点。
    let mut ordered: Vec<Meta> = set
        .iter()
        .filter_map(|id| src.meta(*id).cloned())
        .collect();
    ordered.sort_by_key(Meta::created_at);

    let mut graph = state.graph.lock().await;

    // 新 id 按 kind 铸造：DataStore::allocate 是 crate 外获得 typed id 的
    // 唯一通道（NodeId::from_raw 是 pub(crate)——typed id 必须由图亲手发）。
    // remap 是它们的裸 Ulid 形态，供 sources 这类裸边域过滤。
    let new_inputs: std::collections::HashMap<Ulid, NodeId<Input>> = ordered
        .iter()
        .filter_map(|m| match m {
            Meta::Input(n) => Some((n.id.raw(), graph.data().allocate::<Input>())),
            _ => None,
        })
        .collect();
    let new_turns: std::collections::HashMap<Ulid, NodeId<Turn>> = ordered
        .iter()
        .filter_map(|m| match m {
            Meta::Turn(n) => Some((n.id.raw(), graph.data().allocate::<Turn>())),
            _ => None,
        })
        .collect();
    let new_ctxs: std::collections::HashMap<Ulid, NodeId<Context>> = ordered
        .iter()
        .filter_map(|m| match m {
            Meta::Context(n) => Some((n.id.raw(), graph.data().allocate::<Context>())),
            _ => None,
        })
        .collect();
    let remap: std::collections::HashMap<Ulid, Ulid> = new_inputs
        .iter()
        .map(|(k, v)| (*k, v.raw()))
        .chain(new_turns.iter().map(|(k, v)| (*k, v.raw())))
        .chain(new_ctxs.iter().map(|(k, v)| (*k, v.raw())))
        .collect();

    let mut count = 0usize;
    for old in ordered {
        // 节点本体不可变所以 meta 原样复制，id/归属边换新（只保留指向
        // 克隆集内部的边）；正文经数据面重写入目标图。
        let node: Meta = match &old {
            Meta::Input(n) => {
                let mut new = Input::node(
                    new_inputs[&n.id.raw()],
                    n.kind.parent.and_then(|p| new_turns.get(&p.raw()).copied()),
                    n.kind.text.clone(),
                    n.kind.actor.clone(),
                    n.kind.tools.clone(),
                    n.kind.created_by.and_then(|c| new_turns.get(&c.raw()).copied()),
                );
                new.kind.context_refs = n
                    .kind
                    .context_refs
                    .iter()
                    .filter_map(|r| new_ctxs.get(&r.raw()).copied())
                    .collect();
                Meta::from(new)
            }
            Meta::Turn(n) => {
                let steps = src
                    .data()
                    .entry(n.id)
                    .and_then(|e| e.cloned())
                    .map_err(GraphOpError::Core)?
                    .steps;
                let new_id: NodeId<Turn> = new_turns[&n.id.raw()];
                graph
                    .data()
                    .create(new_id, TurnData { steps: steps.clone() })
                    .map_err(GraphOpError::Core)?;
                Meta::from(Turn::node(
                    new_id,
                    *new_inputs
                        .get(&n.kind.parent.raw())
                        .expect("克隆集扩张已纳入每个 Turn 的父 Input"),
                    n.kind.outcome,
                    n.kind.actor.clone(),
                    n.kind.model.clone(),
                    n.kind.usage,
                    n.kind.tools.clone(),
                    &steps,
                ))
            }
            Meta::Context(n) => {
                let body = src
                    .data()
                    .entry(n.id)
                    .and_then(|e| e.cloned())
                    .map_err(GraphOpError::Core)?
                    .body;
                let new_id: NodeId<Context> = new_ctxs[&n.id.raw()];
                graph
                    .data()
                    .create(new_id, ContextData { body: body.clone() })
                    .map_err(GraphOpError::Core)?;
                Meta::from(Context::node(
                    new_id,
                    n.kind
                        .sources
                        .iter()
                        .filter_map(|s| remap.get(s).copied())
                        .collect(),
                    // distilled_from 在集内则重映射，否则原样保留（允许悬空）。
                    n.kind
                        .distilled_from
                        .map(|d| new_turns.get(&d.raw()).copied().unwrap_or(d)),
                    n.kind.model.clone(),
                    &body,
                ))
            }
        };
        let node = node.with_created_at(old.created_at());
        let id = node.id();
        graph.commit(node).map_err(GraphOpError::Core)?;
        let meta = graph.meta(id).expect("just committed").header_value();
        state.broadcast(ServerEvent::NodeCommitted { meta });
        count += 1;
    }
    Ok(count)
}
