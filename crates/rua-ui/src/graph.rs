//! Graph view: the whole DAG on a dioxus-flow canvas, with a detail side panel.
//!
//! Layout is a left-to-right layered forest (edges bezier from a node's right
//! edge to its child's left edge); the caller (us) owns data and layout while
//! `FlowCanvas` owns pan/zoom, node dragging, and edge rendering.

use std::collections::{HashMap, HashSet};

use dioxus::prelude::*;
use dioxus_flow::{
    EdgeEmphasis, FlowCanvas, FlowEdge, FlowNode, NodeId, NodeMove, Point, RenderNode, Size,
    Viewport, fit_viewport, focus_viewport,
};
use wasm_bindgen::JsCast;

use crate::api;
use crate::chat::{compact_args, markdown_html};
use crate::state::{AppState, Inflight, InflightItem, detach_current_cursor, move_current_cursor, send_current_input};
use crate::types::*;

const CARD_W: f64 = 220.0;
const CARD_H: f64 = 96.0;
/// Column pitch: one depth level.
const COL_W: f64 = 300.0;
/// Lane pitch: one leaf row.
const LANE_H: f64 = 130.0;
const FIT_PADDING: f64 = 60.0;
const FIT_MIN_ZOOM: f64 = 0.2;
const FIT_MAX_ZOOM: f64 = 1.5;
const FOCUS_ZOOM: f64 = 1.0;

/// 左→右分层森林布局（纯函数）：
/// - depth(n) = 沿 parent 到根的边数，x = depth * COL_W；
/// - 根按 created_at 排序做 DFS（子节点同序），叶子依次占新 lane，
///   父 y = 子 lane 均值；
/// - Context 材料节点（无 parent）：x = 最深「引入者」（context_refs 含它的
///   input/turn 节点）的 x + COL_W；没有引入者时退回最深 source 的 x + COL_W，
///   y 排到主树最大 y 之下的独立车道带。
///
/// 只算自动布局；用户拖拽的手动覆盖在组件层应用。
fn is_context(m: &NodeMeta) -> bool {
    m.kind == NodeKindTag::Context
}

fn layout(metas: &HashMap<String, NodeMeta>) -> HashMap<String, Point> {
    // Structural adjacency. Roots split into two classes:
    // - 主树：parent=None 且 created_by=None（人类/直接操作发起的会话）；
    // - spawn 子树：parent=None 但 created_by=Some（某个 turn 内部
    //   spawn_turn 产生的会话）——不混进主树车道，贴着创建者向右下生长。
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    let mut roots: Vec<&NodeMeta> = Vec::new();
    let mut spawned_roots: Vec<&NodeMeta> = Vec::new();
    for meta in metas.values().filter(|m| !is_context(m)) {
        match meta.parent.as_deref().filter(|p| metas.contains_key(*p)) {
            Some(p) => children.entry(p.to_string()).or_default().push(meta.id.clone()),
            None if meta.created_by.is_some() => spawned_roots.push(meta),
            None => roots.push(meta),
        }
    }
    roots.sort_by_key(|m| m.created_at);
    spawned_roots.sort_by_key(|m| m.created_at);
    let by_created = |ids: &mut Vec<String>| {
        ids.sort_by_key(|id| metas.get(id).map(|m| m.created_at).unwrap_or(0));
    };
    for ids in children.values_mut() {
        by_created(ids);
    }

    let mut pos: HashMap<String, Point> = HashMap::new();
    let mut next_lane = 0usize;

    /// DFS: leaves take successive lanes; parents sit at the mean of their
    /// children's lanes. Returns the node's y.
    fn place(
        id: &str,
        depth: usize,
        children: &HashMap<String, Vec<String>>,
        pos: &mut HashMap<String, Point>,
        next_lane: &mut usize,
    ) -> f64 {
        if let Some(p) = pos.get(id) {
            return p.y;
        }
        let y = match children.get(id) {
            Some(kids) if !kids.is_empty() => {
                let mut sum = 0.0;
                for kid in kids {
                    sum += place(kid, depth + 1, children, pos, next_lane);
                }
                sum / kids.len() as f64
            }
            _ => {
                let y = *next_lane as f64 * LANE_H;
                *next_lane += 1;
                y
            }
        };
        pos.insert(id.to_string(), Point::new(depth as f64 * COL_W, y));
        y
    }
    for root in roots {
        place(&root.id, 0, &children, &mut pos, &mut next_lane);
    }
    // spawn 子树：根 input 从创建者的下一列开始，车道接着往下排。
    // 创建者一定先 commit（created_at 更小），按其 created_at 顺序处理时
    // 创建者的位置已经算好（含创建者自己在另一棵 spawn 子树里的情况）。
    for root in spawned_roots {
        let depth = root
            .created_by
            .as_deref()
            .and_then(|cb| pos.get(cb))
            .map(|p| (p.x / COL_W).round() as usize + 1)
            .unwrap_or(1);
        place(&root.id, depth, &children, &mut pos, &mut next_lane);
    }

    // Context materials: lane band below the main tree. x 基准优先取最深
    // 「引入者」（context_refs 包含它的结构节点），没有引入者再退回最深
    // source（context 节点自己的 context_refs）。
    let mut introducer_x: HashMap<String, f64> = HashMap::new();
    for meta in metas.values().filter(|m| !is_context(m)) {
        let Some(p) = pos.get(&meta.id) else {
            continue;
        };
        for r in &meta.context_refs {
            if metas.get(r).is_some_and(is_context) {
                let x = introducer_x.entry(r.clone()).or_insert(f64::NEG_INFINITY);
                *x = x.max(p.x);
            }
        }
    }
    let band_start = if pos.is_empty() {
        0.0
    } else {
        pos.values().map(|p| p.y).fold(0.0, f64::max) + LANE_H
    };
    let mut ctxs: Vec<&NodeMeta> = metas.values().filter(|m| is_context(m)).collect();
    ctxs.sort_by_key(|m| m.created_at);
    for (i, meta) in ctxs.iter().enumerate() {
        let x = match introducer_x.get(&meta.id) {
            Some(x) if x.is_finite() => x + COL_W,
            _ => {
                let x = meta
                    .context_refs
                    .iter()
                    .filter_map(|r| pos.get(r).map(|p| p.x))
                    .fold(f64::NEG_INFINITY, f64::max);
                if x.is_finite() { x + COL_W } else { 0.0 }
            }
        };
        pos.insert(
            meta.id.clone(),
            Point::new(x, band_start + i as f64 * LANE_H),
        );
    }
    pos
}

/// Hover 高亮：悬停节点的祖先链（含自身）上的边 Highlight，其余 Dim。
fn edge_emphasis(source: &str, target: &str, chain: &Option<HashSet<String>>) -> EdgeEmphasis {
    match chain {
        None => EdgeEmphasis::Normal,
        Some(c) if c.contains(source) && c.contains(target) => EdgeEmphasis::Highlight,
        Some(_) => EdgeEmphasis::Dim,
    }
}

/// 编程式视口跳转：短暂打开 animate 做平滑过渡，结束后关掉（用户拖拽时
/// 必须保持关闭）。
fn jump_to(mut viewport: Signal<Viewport>, mut animate: Signal<bool>, target: Viewport) {
    animate.set(true);
    viewport.set(target);
    spawn(async move {
        gloo_timers::future::TimeoutFuture::new(350).await;
        animate.set(false);
    });
}

#[component]
pub fn GraphView() -> Element {
    let mut state = use_context::<AppState>();

    let viewport = use_signal(Viewport::default);
    let mut canvas_size = use_signal(|| Size::new(0.0, 0.0));
    let animate = use_signal(|| false);
    let mut hovered = use_signal(|| None::<String>);
    let mut panning = use_signal(|| false);

    // 图数据快照。在飞 turn 的节点 id 是预分配的，meta 要等 commit 才到；
    // 为每个在飞 turn（任意 cursor，可并行）合成占位 meta 让"进行中"节点
    // 立刻可见，parent 取自 Inflight.parent（turn 开始时该 cursor 的 tip）。
    let mut metas = state.metas.read().clone();
    let inflights = state.inflights.read().clone();
    for turn in inflights.values() {
        if !metas.contains_key(&turn.node_id) {
            let preview = turn.text_preview(80);
            metas.insert(
                turn.node_id.clone(),
                NodeMeta {
                    id: turn.node_id.clone(),
                    parent: turn.parent.clone(),
                    context_refs: vec![],
                    kind: NodeKindTag::Turn,
                    outcome: None,
                    actor: "agent".to_string(),
                    created_at: u64::MAX,
                    usage: None,
                    context_tokens: None,
                    created_by: None,
                    preview,
                },
            );
        }
    }
    let inflight_ids: HashSet<String> = inflights.keys().cloned().collect();

    // ---- spawn 组折叠：collapsed_spawns 里的创建者，其 spawn 子树
    // （created_by = 创建者的根 + 整棵后继子树）整体不渲染。 ----
    let collapsed = state.collapsed_spawns.read().clone();
    // 每个创建者 turn 直接 spawn 出的根节点列表。
    let mut spawn_children: HashMap<String, Vec<String>> = HashMap::new();
    for meta in metas.values() {
        if let Some(cb) = meta.created_by.as_deref() {
            spawn_children.entry(cb.to_string()).or_default().push(meta.id.clone());
        }
    }
    if !collapsed.is_empty() {
        // 结构后继邻接（含跨子树），从被折叠的 spawn 根 DFS 收集隐藏集。
        let mut descendants: HashMap<String, Vec<String>> = HashMap::new();
        for meta in metas.values() {
            if let Some(p) = meta.parent.as_deref() {
                descendants.entry(p.to_string()).or_default().push(meta.id.clone());
            }
        }
        let mut hidden: HashSet<String> = HashSet::new();
        let mut stack: Vec<String> = collapsed
            .iter()
            .flat_map(|c| spawn_children.get(c).cloned().unwrap_or_default())
            .collect();
        while let Some(id) = stack.pop() {
            if hidden.insert(id.clone())
                && let Some(kids) = descendants.get(&id)
            {
                stack.extend(kids.iter().cloned());
            }
        }
        metas.retain(|id, _| !hidden.contains(id));
    }
    // 折叠徽章计数用的映射在过滤前算好（子树隐藏了，创建者还在）。
    let spawn_counts: HashMap<String, usize> =
        spawn_children.iter().map(|(k, v)| (k.clone(), v.len())).collect();

    let tip = state.current().and_then(|c| c.node);
    let pending_attach = state.pending_attach.read().clone();
    let selected = state.selected.read().clone();
    let overrides = state.graph_positions.read();

    // ---- nodes: auto layout + manual drag overrides + state classes ----
    let auto = layout(&metas);
    let card_size = Size::new(CARD_W, CARD_H);
    let mut ordered: Vec<&NodeMeta> = metas.values().collect();
    ordered.sort_by_key(|m| m.created_at);
    let nodes: Vec<FlowNode> = ordered
        .iter()
        .map(|meta| {
            let position = overrides
                .get(&meta.id)
                .copied()
                .or_else(|| auto.get(&meta.id).copied())
                .unwrap_or_default();
            let mut class = String::new();
            if tip.as_deref() == Some(meta.id.as_str()) {
                class.push_str("tip ");
            }
            if selected.as_deref() == Some(meta.id.as_str()) {
                class.push_str("selected ");
            }
            if inflight_ids.contains(&meta.id) {
                class.push_str("inflight ");
            }
            // 草稿态的待定落点：虚线描边，发送后或换 cursor 时消失。
            if pending_attach.as_deref() == Some(meta.id.as_str()) {
                class.push_str("pending ");
            }
            // 用户自己的输入（非 spawn 产物）：accent 强调，一眼区分
            // 「我说的话」和 agent 繁殖出的节点。
            if meta.kind == NodeKindTag::Input && meta.created_by.is_none() {
                class.push_str("user-authored ");
            }
            FlowNode::new(meta.id.clone(), position, card_size).with_class(class)
        })
        .collect();

    // ---- edges: parent edges solid; 引用边只画「引入」：结构节点
    // （input/turn）→ 它引入的 context 节点（虚线）。context 节点的溯源
    // 不画边，留在详情面板的「源自」列表里。 ----
    let hover_chain: Option<HashSet<String>> = hovered.read().as_ref().map(|id| {
        let mut set = HashSet::new();
        let mut cur = Some(id.clone());
        while let Some(cid) = cur {
            if !set.insert(cid.clone()) {
                break;
            }
            cur = metas.get(&cid).and_then(|m| m.parent.clone());
        }
        set
    });
    let mut edges: Vec<FlowEdge> = Vec::new();
    for meta in metas.values() {
        if let Some(parent) = meta.parent.as_deref().filter(|p| metas.contains_key(*p)) {
            edges.push(FlowEdge {
                id: format!("p-{}", meta.id),
                source: NodeId::from(parent),
                target: NodeId::from(meta.id.as_str()),
                emphasis: edge_emphasis(parent, &meta.id, &hover_chain),
                ..Default::default()
            });
        }
        // spawn 边（provenance）：创建者 turn → 它 spawn 出的根节点，虚线。
        // 与 parent（sequence）正交：spawn 根没有 parent，这条边是它和
        // 创建者之间唯一的视觉联系。
        if let Some(creator) = meta.created_by.as_deref().filter(|c| metas.contains_key(*c)) {
            edges.push(FlowEdge {
                id: format!("s-{}", meta.id),
                source: NodeId::from(creator),
                target: NodeId::from(meta.id.as_str()),
                label: Some("spawn".to_string()),
                dashed: true,
                emphasis: edge_emphasis(creator, &meta.id, &hover_chain),
                ..Default::default()
            });
        }
        if is_context(meta) {
            continue;
        }
        for r in meta
            .context_refs
            .iter()
            .filter(|r| metas.contains_key(r.as_str()))
        {
            edges.push(FlowEdge {
                id: format!("r-{}-{r}", meta.id),
                source: NodeId::from(meta.id.as_str()),
                target: NodeId::from(r.as_str()),
                label: Some("引用".to_string()),
                dashed: true,
                emphasis: edge_emphasis(&meta.id, r, &hover_chain),
                ..Default::default()
            });
        }
    }

    // ---- node cards: 捕获本次渲染的快照；状态变化时整体重建闭包
    // （RenderNode 按 Rc 指针判等，捕获旧状态的闭包不会更新，见 dioxus-flow
    // README）----
    let card_metas = metas.clone();
    let click_metas = metas.clone();
    let click_inflight = inflight_ids.clone();
    let card_tip = tip.clone();
    let card_selected = selected.clone();
    let card_inflight = inflight_ids.clone();
    let card_spawn_counts = spawn_counts.clone();
    let card_collapsed = collapsed.clone();
    let render_node = RenderNode::new(move |id: NodeId| {
        let Some(meta) = card_metas.get(&id.0) else {
            return rsx! {};
        };
        node_card(
            meta,
            card_tip.as_deref() == Some(id.0.as_str()),
            card_selected.as_deref() == Some(id.0.as_str()),
            card_inflight.contains(&id.0),
            card_spawn_counts.get(&id.0).copied().unwrap_or(0),
            card_collapsed.contains(&id.0),
        )
    });

    let fit_nodes = nodes.clone();
    let focus_nodes = nodes.clone();
    let focus_tip = tip.clone();

    rsx! {
        div { class: "graph-view",
            div {
                class: "graph-canvas-wrap",
                onmounted: move |event| {
                    if let Some(el) = event.data().downcast::<web_sys::Element>() {
                        let el = el.clone().unchecked_into::<web_sys::HtmlElement>();
                        canvas_size
                            .set(Size::new(el.client_width() as f64, el.client_height() as f64));
                    }
                },
                FlowCanvas {
                    class: "graph-canvas",
                    nodes,
                    edges,
                    viewport,
                    render_node,
                    edge_color: "#46536b",
                    animate: *animate.read(),
                    min_zoom: 0.15,
                    on_node_move: move |mv: NodeMove| {
                        state.graph_positions.write().insert(mv.id.0, mv.position);
                    },
                    on_node_click: move |id: NodeId| {
                        let id = id.0;
                        state.selected.set(Some(id.clone()));
                        state.selected_body.set(None);
                        // 选中即指针：Turn 节点是合法的游标落点，点击就是 fork。
                        // - 空闲会话：直接 move；
                        // - 草稿态（无游标）：记为待定落点，首发消息从这里分叉；
                        // - 会话忙 / Input / Context：只选中查看，不动指针。
                        if click_metas.get(&id).is_some_and(|m| m.kind == NodeKindTag::Turn) {
                            if state.current_cursor.read().is_some() {
                                if !state.busy() {
                                    let move_id = id.clone();
                                    spawn(async move {
                                        move_current_cursor(state, &move_id).await;
                                    });
                                }
                            } else {
                                state.pending_attach.set(Some(id.clone()));
                            }
                        }
                        // 在飞 turn 还没 commit，服务端取不到 body；
                        // 详情面板改从本地 inflight 累积器实时渲染。
                        if !click_inflight.contains(&id) {
                            spawn(async move {
                                match api::get_node(&id).await {
                                    Ok(node) => state.selected_body.set(Some(node)),
                                    Err(e) => state.set_error(format!("获取节点失败: {e}")),
                                }
                            });
                        }
                    },                    on_node_hover: move |id: Option<NodeId>| {
                        // 平移拖拽期间抑制 hover 更新：此时重渲会用旧 signal 值
                        // 重写视口 style，造成瞬跳。
                        if *panning.read() {
                            return;
                        }
                        hovered.set(id.map(|i| i.0));
                    },
                    on_pan_start: move |_| panning.set(true),
                    on_pan_end: move |_| {
                        panning.set(false);
                        hovered.set(None);
                    },
                    on_empty_click: move |_| {
                        // 点空白 = detach：取消选中，并把指针置空。
                        // - 空闲会话：服务端 detach，下一次输入从新的根开始；
                        // - 草稿态：清掉待定落点（本来就什么都没指）；
                        // - 会话忙：不动指针（服务端也会 409）。
                        state.selected.set(None);
                        state.selected_body.set(None);
                        if state.current_cursor.read().is_some() {
                            if !state.busy() {
                                spawn(async move {
                                    detach_current_cursor(state).await;
                                });
                            }
                        } else {
                            state.pending_attach.set(None);
                        }
                    },
                    empty: rsx! {
                        div { class: "chat-empty",
                            p { "图为空。" }
                            p { "回到聊天视图发送第一条消息。" }
                        }
                    },
                }
                div { class: "graph-toolbar",
                    button {
                        class: "graph-tool-btn",
                        disabled: fit_nodes.is_empty(),
                        onclick: move |_| {
                            let size = *canvas_size.read();
                            if size.width <= 0.0 {
                                return;
                            }
                            let vp =
                                fit_viewport(&fit_nodes, size, FIT_PADDING, FIT_MIN_ZOOM, FIT_MAX_ZOOM);
                            jump_to(viewport, animate, vp);
                        },
                        "适应全部"
                    }
                    button {
                        class: "graph-tool-btn",
                        disabled: focus_tip.is_none(),
                        onclick: move |_| {
                            let size = *canvas_size.read();
                            let Some(tip) = &focus_tip else { return };
                            let Some(node) = focus_nodes.iter().find(|n| n.id.0 == *tip) else {
                                return;
                            };
                            jump_to(viewport, animate, focus_viewport(node, size, FOCUS_ZOOM));
                        },
                        "定位 tip"
                    }
                }
            }
            if selected.is_some() {
                DetailPanel {}
            }
        }
    }
}

/// 单节点卡片：顶行 kind 徽标 + 短 id（在飞时带「进行中」）+ spawn 折叠
/// 徽章，中间 preview 两行截断，底行 actor · ctx · outcome 彩点。
fn node_card(
    meta: &NodeMeta,
    _is_tip: bool,
    _is_selected: bool,
    is_inflight: bool,
    spawn_count: usize,
    spawn_collapsed: bool,
) -> Element {
    let kind = kind_label(meta.kind);
    rsx! {
        div { class: "fcard fcard-{kind}",
            div { class: "fcard-top",
                span { class: "fcard-kind kind-{kind}", {kind_name(meta.kind)} }
                span { class: "fcard-id", "#{short_id(&meta.id)}" }
                if is_inflight {
                    span { class: "fcard-running", "进行中" }
                }
                if spawn_count > 0 {
                    SpawnToggle {
                        creator: meta.id.clone(),
                        count: spawn_count,
                        collapsed: spawn_collapsed,
                    }
                }
            }
            div { class: "fcard-preview", "{meta.preview}" }
            div { class: "fcard-bottom",
                span { class: "fcard-actor", "{meta.actor}" }
                if let Some(ctx) = meta.context_tokens {
                    span { class: "fcard-ctx", title: "上下文量（该回合最后一次调用的 input tokens）",
                        "ctx {fmt_tokens(ctx)}"
                    }
                }
                if let Some(outcome) = meta.outcome {
                    span { class: "fcard-outcome",
                        span { class: "outcome-dot outcome-{outcome.label()}" }
                        {outcome.label()}
                    }
                }
                if let Some(usage) = &meta.usage {
                    span { class: "fcard-usage", "↑{usage.input_tokens} ↓{usage.output_tokens}" }
                }
            }
        }
    }
}

/// spawn 组折叠徽章：点一下折叠/展开该回合 spawn 出的整棵子树。
/// 必须拦住 mousedown/mouseup，否则节点的拖拽/选中逻辑会抢事件。
#[component]
fn SpawnToggle(creator: String, count: usize, collapsed: bool) -> Element {
    let mut state = use_context::<AppState>();
    rsx! {
        button {
            class: "spawn-toggle",
            title: "折叠/展开此回合 spawn 出的子会话",
            onmousedown: move |e| e.stop_propagation(),
            onmouseup: move |e| e.stop_propagation(),
            onclick: move |e| {
                e.stop_propagation();
                let mut set = state.collapsed_spawns.write();
                if !set.remove(&creator) {
                    set.insert(creator.clone());
                }
            },
            if collapsed { "▸ {count} 子会话" } else { "▾ {count} 子会话" }
        }
    }
}

/// 紧凑的 token 量格式化：932 / 1.2K / 12K / 1.2M。
fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else if n < 1_000_000 {
        format!("{}K", n / 1_000)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

fn kind_label(kind: NodeKindTag) -> &'static str {
    match kind {
        NodeKindTag::Input => "input",
        NodeKindTag::Turn => "turn",
        NodeKindTag::Context => "context",
    }
}

fn kind_name(kind: NodeKindTag) -> &'static str {
    match kind {
        NodeKindTag::Input => "输入",
        NodeKindTag::Turn => "回合",
        NodeKindTag::Context => "材料",
    }
}

#[component]
fn DetailPanel() -> Element {
    let mut state = use_context::<AppState>();
    let Some(id) = state.selected.read().clone() else {
        return rsx! {};
    };
    let meta = state.metas.read().get(&id).cloned();
    let body = state.selected_body.read().clone();
    // 在飞 turn：本地累积器实时渲染（此时服务端还没 commit 该节点）。
    let inflight = state.inflights.read().get(&id).cloned();
    let is_context = meta
        .as_ref()
        .is_some_and(|m| m.kind == NodeKindTag::Context);
    // 指针（游标落点）只能是 Turn 节点（服务端对 Input/Context 一律 400）。
    let is_turn = meta
        .as_ref()
        .is_some_and(|m| m.kind == NodeKindTag::Turn);
    // 指针是否已落在选中节点上：选中即指针，点击 Turn 时已 move /
    // 记为待定落点；只有 landed 时面板的输入框才是「从这里继续」。
    let tip = state.current().and_then(|c| c.node);
    let pending_attach = state.pending_attach.read().clone();
    let landed =
        tip.as_deref() == Some(id.as_str()) || pending_attach.as_deref() == Some(id.as_str());
    let busy = state.busy();
    let mut panel_draft = use_signal(String::new);

    let mut send = move || {
        let text = panel_draft.read().trim().to_string();
        if text.is_empty() || state.busy() {
            return;
        }
        panel_draft.set(String::new());
        spawn(async move {
            send_current_input(state, text).await;
        });
    };

    rsx! {
        aside { class: "detail-panel",
            div { class: "detail-header",
                span { class: "detail-title", "节点 #{short_id(&id)}" }
                button {
                    class: "detail-close",
                    onclick: move |_| state.selected.set(None),
                    "×"
                }
            }
            // 中间内容区滚动；底部输入框固定。
            div { class: "detail-scroll",
                if let Some(meta) = &meta {
                    dl { class: "detail-meta",
                        dt { "类型" }
                        dd { {kind_label(meta.kind)} }
                        dt { "actor" }
                        dd { "{meta.actor}" }
                        dt { "创建时间" }
                        dd { "{meta.created_at}" }
                        dt { "parent" }
                        dd { {meta.parent.as_deref().map(short_id).unwrap_or("—").to_string()} }
                        if let Some(creator) = meta.created_by.clone() {
                            dt { "创建者" }
                            dd {
                                button {
                                    class: "detail-link",
                                    title: "查看创建它的回合",
                                    onclick: move |_| {
                                        let creator = creator.clone();
                                        state.selected.set(Some(creator.clone()));
                                        state.selected_body.set(None);
                                        spawn(async move {
                                            match api::get_node(&creator).await {
                                                Ok(node) => state.selected_body.set(Some(node)),
                                                Err(e) => state.set_error(format!("获取节点失败: {e}")),
                                            }
                                        });
                                    },
                                    "#{short_id(&creator)}"
                                }
                            }
                        }
                        if !meta.context_refs.is_empty() {
                            // context 节点的 context_refs 是它的溯源（「源自」）；
                            // 结构节点的是它引入的材料。
                            dt { if is_context { "源自" } else { "context_refs" } }
                            dd {
                                {meta.context_refs.iter().map(|r| short_id(r).to_string()).collect::<Vec<_>>().join(", ")}
                            }
                        }
                        if let Some(outcome) = meta.outcome {
                            dt { "outcome" }
                            dd { span { class: "badge outcome-{outcome.label()}", "{outcome.label()}" } }
                        }
                        if let Some(ctx) = meta.context_tokens {
                            dt { "上下文" }
                            dd { "{fmt_tokens(ctx)} tokens" }
                        }
                    }
                    if !meta.preview.is_empty() {
                        div { class: "detail-preview", "{meta.preview}" }
                    }
                }
                if let Some(turn) = &inflight {
                    InflightDetail { turn: turn.clone() }
                } else {
                    match &body {
                        Some(node) => rsx! { NodeBody { node: node.clone() } },
                        None => rsx! { div { class: "detail-loading", "加载节点内容…" } },
                    }
                }
            }
            // 指针落在选中节点上时，面板底部就是输入框：回车创建子节点
            // （新 Input + 启动回合），与聊天视图同一套发送语义。
            div { class: "detail-compose",
                if is_turn && landed && !busy {
                    textarea {
                        class: "input-box",
                        placeholder: "从这里继续，Enter 发送，Shift+Enter 换行",
                        value: "{panel_draft}",
                        oninput: move |e| panel_draft.set(e.value()),
                        onkeydown: move |e| {
                            if e.key() == Key::Enter && !e.modifiers().contains(Modifiers::SHIFT) {
                                e.prevent_default();
                                send();
                            }
                        },
                    }
                } else {
                    p { class: "detail-compose-hint",
                        if !is_turn {
                            "指针只能落在回合节点上，此节点仅查看。"
                        } else if busy {
                            "当前会话有进行中的回合，结束后才能从这里继续。"
                        } else {
                            "指针未落在此节点。"
                        }
                    }
                }
            }
        }
    }
}

/// 在飞 turn 的实时内部：reasoning / 已生成文本 / 工具执行，按 WS 到达
/// 顺序渲染（与提交后的 steps 顺序一致），全部来自本地累积器。
#[component]
fn InflightDetail(turn: Inflight) -> Element {
    rsx! {
        div { class: "detail-body",
            div { class: "inflight-hint", "生成中…" }
            for item in &turn.items {
                match item {
                    InflightItem::Reasoning(r) => rsx! {
                        details { class: "reasoning", open: true,
                            summary { "思考过程" }
                            pre { class: "mono", "{r}" }
                        }
                    },
                    InflightItem::Text(t) => rsx! {
                        div { class: "bubble-text markdown", dangerous_inner_html: markdown_html(t) }
                    },
                    InflightItem::Tool(tool) => rsx! {
                        div { class: "tool-exec tool-running", key: "{tool.call_id}",
                            span { class: "tool-name", "{tool.name}" }
                            span { class: "tool-args", {compact_args(&tool.args)} }
                            match (&tool.output_preview, tool.duration_ms) {
                                (Some(preview), Some(ms)) => rsx! {
                                    span { class: "tool-duration", "{ms}ms" }
                                    div { class: "tool-preview mono", "{preview}" }
                                },
                                _ => rsx! {
                                    span { class: "tool-duration", "运行中…" }
                                },
                            }
                        }
                    },
                }
            }
        }
    }
}

#[component]
fn NodeBody(node: Node) -> Element {
    match &node.kind {
        NodeKind::Input { text, .. } => rsx! {
            div { class: "detail-body",
                div { class: "bubble-text", "{text}" }
            }
        },
        NodeKind::Turn {
            steps,
            outcome,
            model,
            usage,
            ..
        } => rsx! {
            div { class: "detail-body",
                div { class: "bubble-footer",
                    span { class: "bubble-model", "{model}" }
                    span { class: "badge outcome-{outcome.label()}", "{outcome.label()}" }
                    span { class: "bubble-usage",
                        "↑{usage.input_tokens} ↓{usage.output_tokens} tokens"
                    }
                }
                for step in steps {
                    match step {
                        Step::LlmCall { response_text, reasoning, usage, .. } => rsx! {
                            if let Some(r) = reasoning.as_ref().filter(|r| !r.is_empty()) {
                                details { class: "reasoning",
                                    summary { "思考过程" }
                                    pre { class: "mono", "{r}" }
                                }
                            }
                            if !response_text.is_empty() {
                                div { class: "bubble-text", "{response_text}" }
                            }
                            div { class: "step-usage", "本次调用 ↑{usage.input_tokens} ↓{usage.output_tokens}" }
                        },
                        Step::ToolExec { name, args, output, duration_ms, .. } => rsx! {
                            details { class: "tool-exec",
                                summary {
                                    span { class: "tool-name", "{name}" }
                                    span { class: "tool-args", {compact_args(args)} }
                                    span { class: "tool-duration", "{duration_ms}ms" }
                                }
                                div { class: "tool-section-label", "参数" }
                                pre { class: "mono", "{serde_json::to_string_pretty(args).unwrap_or_default()}" }
                                div { class: "tool-section-label", "输出" }
                                pre { class: "mono", "{output}" }
                            }
                        },
                    }
                }
            }
        },
        NodeKind::Context { body, model, .. } => rsx! {
            div { class: "detail-body",
                div { class: "bubble-footer",
                    span { class: "bubble-model", "{model}" }
                }
                pre { class: "mono", "{body}" }
            }
        },
    }
}
