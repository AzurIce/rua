//! Global UI state: one struct of dioxus signals, provided via context.
//!
//! UI = a view onto the graph + a set of cursors (each cursor is a session).
//! The graph itself lives in the daemon; here we keep:
//! - `metas`: every known node meta (drives the graph view),
//! - `chain`: the current cursor's chain with full bodies (drives chat),
//! - `inflights`: in-flight turn accumulators keyed by node id — multiple
//!   cursors may run turns in parallel on different branches.

use std::collections::HashMap;

use dioxus::prelude::*;

use crate::api;
use crate::types::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Chat,
    Graph,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connected,
    Connecting,
    Disconnected,
}

impl ConnState {
    pub fn label(self) -> &'static str {
        match self {
            ConnState::Connected => "已连接",
            ConnState::Connecting => "连接中",
            ConnState::Disconnected => "已断开",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InflightTool {
    pub call_id: String,
    pub name: String,
    pub args: serde_json::Value,
    pub output_preview: Option<String>,
    pub duration_ms: Option<u64>,
}

/// 在飞 turn 的有序内容项：按 WS 到达顺序交错记录，渲染时保持
/// 「思考 → 文本 → 工具调用 → 思考 …」的真实顺序（不再分组堆叠）。
#[derive(Debug, Clone, PartialEq)]
pub enum InflightItem {
    Reasoning(String),
    Text(String),
    Tool(InflightTool),
}

/// A turn in flight: accumulated from WS deltas, keyed by its pre-allocated
/// turn node id. `parent` is the cursor's tip when the turn started (used to
/// place the placeholder card in the graph view).
#[derive(Debug, Clone, PartialEq)]
pub struct Inflight {
    pub cursor_id: String,
    pub node_id: String,
    pub parent: Option<String>,
    pub items: Vec<InflightItem>,
}

impl Inflight {
    pub fn new(cursor_id: String, node_id: String, parent: Option<String>) -> Self {
        Self {
            cursor_id,
            node_id,
            parent,
            items: vec![],
        }
    }

    /// 追加流式 delta：与末尾同类项合并，否则新开一项。
    fn push_delta(&mut self, delta: &str, reasoning: bool) {
        let can_merge = matches!(
            (self.items.last_mut(), reasoning),
            (Some(InflightItem::Text(_)), false) | (Some(InflightItem::Reasoning(_)), true)
        );
        if can_merge {
            match self.items.last_mut() {
                Some(InflightItem::Text(t) | InflightItem::Reasoning(t)) => t.push_str(delta),
                _ => unreachable!(),
            }
        } else if reasoning {
            self.items.push(InflightItem::Reasoning(delta.to_string()));
        } else {
            self.items.push(InflightItem::Text(delta.to_string()));
        }
    }

    /// 图占位卡片的 preview：已生成文本的开头。
    pub fn text_preview(&self, max: usize) -> String {
        let text: String = self
            .items
            .iter()
            .filter_map(|item| match item {
                InflightItem::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        text.chars().take(max).collect()
    }
}

#[derive(Clone, Copy)]
pub struct AppState {
    pub view: Signal<View>,
    /// 所有图名与当前活跃图（服务端唯一权威，切换后广播 graph_switched）。
    pub graphs: Signal<Vec<String>>,
    pub current_graph: Signal<String>,
    pub cursors: Signal<Vec<Cursor>>,
    /// `None` = draft state: a purely local "new session" with no server-side
    /// cursor yet (created lazily on the first send).
    pub current_cursor: Signal<Option<String>>,
    /// Draft state's pending attach node (a Turn id): the first send forks
    /// from it. Cleared on send or when switching to another cursor.
    pub pending_attach: Signal<Option<String>>,
    /// 可用模型列表（GET /api/models）与本次发送的模型覆盖
    ///（None = daemon 默认模型）。
    pub models: Signal<Vec<String>>,
    pub default_model: Signal<String>,
    pub selected_model: Signal<Option<String>>,
    /// 被关掉的工具（默认全开 = 发 None）。关任意一个会改变请求前缀 →
    /// 前缀缓存失效，开发测试时这正是目的。
    pub tools_off: Signal<std::collections::HashSet<String>>,
    pub metas: Signal<HashMap<String, NodeMeta>>,
    pub chain: Signal<Vec<Node>>,
    /// In-flight turns keyed by turn node id (one per running cursor).
    pub inflights: Signal<HashMap<String, Inflight>>,
    pub conn: Signal<ConnState>,
    pub error: Signal<Option<String>>,
    /// Graph view: selected node id and its (lazily fetched) body.
    pub selected: Signal<Option<String>>,
    pub selected_body: Signal<Option<Node>>,
    /// Graph view: manual drag positions, overriding the auto layout for the
    /// whole session.
    pub graph_positions: Signal<HashMap<String, dioxus_flow::Point>>,
    /// Graph view: collapsed spawn groups, keyed by creator turn id
    /// (its spawned subtree is hidden while collapsed).
    pub collapsed_spawns: Signal<std::collections::HashSet<String>>,
    /// Graph view: box-selected node ids and the clipboard for cross-graph
    /// clone ((source graph name, selected ids)).
    pub selection: Signal<std::collections::HashSet<String>>,
    pub clipboard: Signal<Option<(String, Vec<String>)>>,
    pub draft: Signal<String>,
    pub booted: Signal<bool>,
}

impl AppState {
    pub fn current(&self) -> Option<Cursor> {
        let id = self.current_cursor.read();
        let cursors = self.cursors.read();
        id.as_ref()
            .and_then(|id| cursors.iter().find(|c| &c.id == id).cloned())
    }

    /// A cursor's tip node id from the cursor list.
    pub fn cursor_tip(&self, cursor_id: &str) -> Option<String> {
        self.cursors
            .read()
            .iter()
            .find(|c| c.id == cursor_id)
            .and_then(|c| c.node.clone())
    }

    /// The current cursor's in-flight turn, if any.
    pub fn current_inflight(&self) -> Option<Inflight> {
        let current = self.current_cursor.read();
        let inflights = self.inflights.read();
        current
            .as_ref()
            .and_then(|cid| inflights.values().find(|t| &t.cursor_id == cid))
            .cloned()
    }

    /// Busy = the current cursor has a turn in flight (other cursors may be
    /// running in parallel without blocking this session).
    pub fn busy(&self) -> bool {
        self.current_inflight().is_some()
    }

    pub fn set_error(&mut self, msg: String) {
        self.error.set(Some(msg));
    }

    /// 全部可用工具。spawn 工具的实际可用性还受 spawner/深度门控。
    pub const ALL_TOOLS: &'static [&'static str] = &["bash", "spawn_turn", "inspect"];

    /// 本次发送的工具覆盖：全开 → None；有关掉的 → Some(剩余列表)。
    pub fn tools_override(&self) -> Option<Vec<String>> {
        let off = self.tools_off.read();
        if off.is_empty() {
            return None;
        }
        Some(
            Self::ALL_TOOLS
                .iter()
                .filter(|t| !off.contains(**t))
                .map(|t| t.to_string())
                .collect(),
        )
    }
}

/// Startup: load the cursor list and pick the current one. No cursor is
/// auto-created — an empty list means draft state (the first message creates
/// the cursor lazily). Then load graph + chain.
pub async fn bootstrap(mut state: AppState) {
    // 模型列表并行拉，不占启动关键路径：/api/models 要代理 provider，
    // provider 不可达时会阻塞到超时（数秒），不该卡住「正在连接」。
    spawn(async move {
        match api::get_models().await {
            Ok(m) => {
                state.models.set(m.models);
                state.default_model.set(m.default);
            }
            Err(e) => state.set_error(format!("获取模型列表失败: {e}")),
        }
    });
    match api::get_graphs().await {
        Ok(g) => {
            state.graphs.set(g.graphs);
            state.current_graph.set(g.current);
        }
        Err(e) => {
            state.set_error(format!("无法连接 rua-server ({}): {e}", api::API_BASE));
            return;
        }
    }
    match api::get_cursors().await {
        Ok(cursors) => {
            let keep = state
                .current_cursor
                .read()
                .as_ref()
                .is_some_and(|id| cursors.iter().any(|c| &c.id == id));
            if !keep {
                state.current_cursor.set(cursors.first().map(|c| c.id.clone()));
                if cursors.is_empty() {
                    state.pending_attach.set(None);
                    state.chain.set(Vec::new());
                }
            }
            state.cursors.set(cursors);
        }
        Err(e) => {
            state.set_error(format!("无法连接 rua-server ({}): {e}", api::API_BASE));
            return;
        }
    }
    resync(state).await;
    state.booted.set(true);
}

/// Full resync: graph snapshot + current cursor's chain. Used at startup and
/// after every WS reconnect.
pub async fn resync(mut state: AppState) {
    match api::get_graph().await {
        Ok(graph) => {
            state
                .metas
                .set(graph.nodes.into_iter().map(|m| (m.id.clone(), m)).collect());
            state.cursors.set(graph.cursors.clone());
            // Seed placeholders for every turn the server reports as running:
            // keep accumulators for turns still in flight, drop the rest.
            let mut map = state.inflights.write();
            map.retain(|node_id, _| graph.in_flight.iter().any(|h| &h.node_id == node_id));
            for h in &graph.in_flight {
                let parent = graph
                    .cursors
                    .iter()
                    .find(|c| c.id == h.cursor_id)
                    .and_then(|c| c.node.clone());
                map.entry(h.node_id.clone()).or_insert_with(|| {
                    Inflight::new(h.cursor_id.clone(), h.node_id.clone(), parent)
                });
            }
        }
        Err(e) => state.set_error(format!("同步图数据失败: {e}")),
    }
    refresh_chain(state).await;
}

pub async fn refresh_chain(mut state: AppState) {
    let Some(cid) = state.current_cursor.read().clone() else {
        return;
    };
    match api::get_chain(&cid).await {
        Ok(nodes) => state.chain.set(nodes),
        Err(e) => state.set_error(format!("获取会话链失败: {e}")),
    }
}

/// Apply one WS event. Body-less events that change the visible chain trigger
/// a chain refetch instead of patching node bodies locally.
pub async fn handle_event(mut state: AppState, ev: WsEvent) {
    let current = state.current_cursor.read().clone();
    match ev {
        WsEvent::TurnStarted {
            cursor_id,
            node_id,
        } => {
            let parent = state.cursor_tip(&cursor_id);
            state
                .inflights
                .write()
                .insert(node_id.clone(), Inflight::new(cursor_id, node_id, parent));
        }
        WsEvent::TextDelta { node_id, delta, .. } => {
            if let Some(t) = state.inflights.write().get_mut(&node_id) {
                t.push_delta(&delta, false);
            }
        }
        WsEvent::ReasoningDelta { node_id, delta, .. } => {
            if let Some(t) = state.inflights.write().get_mut(&node_id) {
                t.push_delta(&delta, true);
            }
        }
        WsEvent::ToolExecStarted {
            node_id,
            call_id,
            name,
            args,
            ..
        } => {
            if let Some(t) = state.inflights.write().get_mut(&node_id) {
                t.items.push(InflightItem::Tool(InflightTool {
                    call_id,
                    name,
                    args,
                    output_preview: None,
                    duration_ms: None,
                }));
            }
        }
        WsEvent::ToolExecFinished {
            node_id,
            call_id,
            output_preview,
            duration_ms,
            ..
        } => {
            let mut map = state.inflights.write();
            if let Some(t) = map.get_mut(&node_id)
                && let Some(InflightItem::Tool(tool)) = t
                    .items
                    .iter_mut()
                    .rev()
                    .find(|item| matches!(item, InflightItem::Tool(tool) if tool.call_id == call_id))
            {
                tool.output_preview = Some(output_preview);
                tool.duration_ms = Some(duration_ms);
            }
        }
        WsEvent::TurnCommitted {
            ref cursor_id,
            ref node_id,
            ..
        } => {
            state.inflights.write().remove(node_id);
            if current.as_ref() == Some(cursor_id) {
                refresh_chain(state).await;
            }
        }
        WsEvent::NodeCommitted { meta } => {
            state.metas.write().insert(meta.id.clone(), meta);
        }
        WsEvent::CursorCreated { cursor } => {
            let mut cursors = state.cursors.write();
            if !cursors.iter().any(|c| c.id == cursor.id) {
                cursors.push(cursor);
            }
        }
        WsEvent::CursorMoved { cursor_id, node } => {
            // A move ends any in-flight bookkeeping for that cursor (its turn
            // either committed or was superseded).
            state
                .inflights
                .write()
                .retain(|_, t| t.cursor_id != cursor_id);
            if let Some(c) = state
                .cursors
                .write()
                .iter_mut()
                .find(|c| c.id == cursor_id)
            {
                c.node = node;
            }
            if current.as_ref() == Some(&cursor_id) {
                // node = None (detach) yields an empty chain from the server.
                refresh_chain(state).await;
            }
        }
        WsEvent::GraphSwitched { .. } => {
            // 活跃图被换（任何客户端发起）：丢弃所有图派生状态，从头来。
            state.metas.set(Default::default());
            state.chain.set(Vec::new());
            state.inflights.set(Default::default());
            state.selected.set(None);
            state.selected_body.set(None);
            state.graph_positions.set(Default::default());
            state.collapsed_spawns.set(Default::default());
            // 换图后选中集失效；剪贴板保留（跨图粘贴正是它的用途）。
            state.selection.set(Default::default());
            state.pending_attach.set(None);
            state.current_cursor.set(None);
            bootstrap(state).await;
        }
    }
}

/// 图管理操作（新建/切换/重命名/删除）。切换类的副作用由服务端
/// graph_switched 广播驱动重同步；这里刷新图列表并显示错误。
pub async fn run_graph_op(
    mut state: AppState,
    op: impl std::future::Future<Output = Result<(), String>>,
) {
    if let Err(e) = op.await {
        state.set_error(e);
    }
    match api::get_graphs().await {
        Ok(g) => {
            state.graphs.set(g.graphs);
            state.current_graph.set(g.current);
        }
        Err(e) => state.set_error(format!("刷新图列表失败: {e}")),
    }
}

/// Move the current cursor onto `node_id` (fork / attach). The server
/// broadcasts `cursor_moved`, which refreshes the chain; we also apply the
/// result directly so the UI does not depend on the echo.
pub async fn move_current_cursor(mut state: AppState, node_id: &str) {
    let Some(cid) = state.current_cursor.read().clone() else {
        return;
    };
    match api::move_cursor(&cid, node_id).await {
        Ok(cursor) => {
            if let Some(c) = state
                .cursors
                .write()
                .iter_mut()
                .find(|c| c.id == cursor.id)
            {
                *c = cursor;
            }
            refresh_chain(state).await;
        }
        Err(e) => state.set_error(format!("移动游标失败: {e}")),
    }
}

/// Detach the current cursor from the graph: the pointer goes to null and
/// the next input starts a fresh root. The server broadcasts
/// `cursor_moved(node=None)`, which refreshes the (now empty) chain.
pub async fn detach_current_cursor(mut state: AppState) {
    let Some(cid) = state.current_cursor.read().clone() else {
        return;
    };
    match api::detach_cursor(&cid).await {
        Ok(cursor) => {
            if let Some(c) = state
                .cursors
                .write()
                .iter_mut()
                .find(|c| c.id == cursor.id)
            {
                *c = cursor;
            }
            refresh_chain(state).await;
        }
        Err(e) => state.set_error(format!("detach 失败: {e}")),
    }
}

pub async fn send_current_input(mut state: AppState, text: String) {
    let current = state.current_cursor.read().clone();
    let model = state.selected_model.read().clone();
    let tools = state.tools_override();
    let input_node = match current {
        // Draft state: atomically create cursor + root input + started turn.
        None => {
            let parent = state.pending_attach.read().clone();
            match api::post_root_input(&text, parent, model.as_deref(), tools.as_deref()).await {
                Ok(resp) => {
                    state.pending_attach.set(None);
                    if !state.cursors.read().iter().any(|c| c.id == resp.cursor.id) {
                        state.cursors.write().push(resp.cursor.clone());
                    }
                    state.current_cursor.set(Some(resp.cursor.id));
                    resp.input_node
                }
                Err(e) => {
                    state.set_error(format!("发送失败: {e}"));
                    return;
                }
            }
        }
        Some(cid) => match api::send_input(&cid, &text, model.as_deref(), tools.as_deref()).await {
            Ok(resp) => resp.input_node,
            Err(e) => {
                state.set_error(format!("发送失败: {e}"));
                return;
            }
        },
    };
    state.draft.set(String::new());
    // The input node is committed synchronously; make it visible
    // immediately instead of waiting for the turn to finish.
    state
        .metas
        .write()
        .insert(input_node.id.clone(), input_node);
    refresh_chain(state).await;
}

pub async fn cancel_current_turn(mut state: AppState) {
    let Some(cid) = state.current_cursor.read().clone() else {
        return;
    };
    if let Err(e) = api::cancel_turn(&cid).await {
        state.set_error(format!("取消失败: {e}"));
    }
}

/// Enter draft state: a purely local "new session" — nothing is created on
/// the server until the first message is sent. `attach_to` is the pending
/// landing node (a Turn); the first send then forks from it.
pub fn enter_draft(mut state: AppState, attach_to: Option<String>) {
    state.current_cursor.set(None);
    state.pending_attach.set(attach_to);
    state.chain.set(Vec::new());
}
