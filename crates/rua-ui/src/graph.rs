//! Graph view: the whole DAG on a dioxus-flow canvas, with a detail side panel.
//!
//! Layout: conversation chains run horizontally (one node per column, same
//! row); spawn subtrees and forks drop down from the parent's column as new
//! chains. The caller (us) owns data and layout while `FlowCanvas` owns
//! pan/zoom, node dragging, and edge rendering.

use std::collections::{HashMap, HashSet};

use dioxus::prelude::*;
use dioxus_flow::{
    EdgeEmphasis, EdgeRoute, FlowCanvas, FlowEdge, FlowNode, NodeId, NodeMove, Point, RenderNode,
    Size, Viewport, fit_viewport, focus_viewport,
};
use wasm_bindgen::JsCast;

use crate::api;
use crate::chat::{compact_args, markdown_html, usage_label};
use crate::state::{AppState, Inflight, InflightItem, detach_current_cursor, move_current_cursor, send_current_input};
use crate::types::*;

const CARD_W: f64 = 220.0;
const CARD_H: f64 = 96.0;
/// spawn 折叠条做进卡片内部（底栏）时，卡片需要的额外高度。
const STRIP_H: f64 = 20.0;
/// 列距：一个深度层级（缩进一格）。
const COL_W: f64 = 300.0;
/// 行距：一个节点一行（目录树的一行）。
const ROW_H: f64 = 128.0;
const FIT_PADDING: f64 = 60.0;
const FIT_MIN_ZOOM: f64 = 0.2;
const FIT_MAX_ZOOM: f64 = 1.5;
const FOCUS_ZOOM: f64 = 1.0;

/// 水平链布局（纯函数）：会话主链横着走，spawn / fork 垂直成子树。
/// - **链**：沿 parent（会话后继）的「第一个孩子」（created_at 最早）一路
///   向右——输入、回合、下一个输入……一节点一列，全部同一行，继续输入
///   就是主链向右延续；
/// - **子树**：一个节点的其余孩子另起新行——spawn 子会话（created_by，
///   虚线溯源边）和 fork（同一回合的多个后继输入）都从父节点那一列垂直
///   向下，各自成为一条新的水平链；
/// - **行分配**：链队列 BFS——父链整行放完后，它引出的子链依次占据下面
///   的行。父必在子之上，一链独占一行，绝不重叠；
/// - **Context 材料节点**：排在结构树最下方，每个独占一行，列基准取最深
///   「引入者」（context_refs 含它的结构节点）的列 + 1，没有引入者退回
///   最深 source。
///
/// 只算自动布局；用户拖拽的手动覆盖在组件层应用。
fn is_context(m: &NodeMeta) -> bool {
    m.kind == NodeKindTag::Context
}

fn layout(metas: &HashMap<String, NodeMeta>) -> HashMap<String, Point> {
    // parent 孩子（会话后继）与 created_by 孩子（spawn 溯源）分开收集；
    // 两者都没有（或父引用悬空）的是会话根。parent 优先：一个节点理论上
    // 可以同时有两者，此时它参与主链，created_by 只画溯源边。
    let mut cont_children: HashMap<String, Vec<String>> = HashMap::new();
    let mut spawn_children: HashMap<String, Vec<String>> = HashMap::new();
    let mut roots: Vec<&NodeMeta> = Vec::new();
    for meta in metas.values().filter(|m| !is_context(m)) {
        if let Some(p) = meta.parent.as_deref().filter(|p| metas.contains_key(*p)) {
            cont_children
                .entry(p.to_string())
                .or_default()
                .push(meta.id.clone());
        } else if let Some(c) = meta.created_by.as_deref().filter(|c| metas.contains_key(*c)) {
            spawn_children
                .entry(c.to_string())
                .or_default()
                .push(meta.id.clone());
        } else {
            roots.push(meta);
        }
    }
    roots.sort_by_key(|m| m.created_at);
    let by_created = |id: &String| metas.get(id).map(|m| m.created_at).unwrap_or(0);
    for ids in cont_children.values_mut().chain(spawn_children.values_mut()) {
        ids.sort_by_key(by_created);
    }

    // 链队列：(链首节点, 起始列)。BFS——每链独占一行，行号按出队顺序
    // 递增；父链引出的子链入队尾，保证父行必在子行之上。
    let mut pos: HashMap<String, Point> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut next_row = 0usize;
    let mut queue: std::collections::VecDeque<(String, usize)> = roots
        .iter()
        .map(|m| (m.id.clone(), 0usize))
        .collect();
    while let Some((start, start_col)) = queue.pop_front() {
        let row = next_row;
        next_row += 1;
        let mut col = start_col;
        let mut cur = Some(start);
        while let Some(id) = cur {
            if !seen.insert(id.clone()) {
                break;
            }
            pos.insert(
                id.clone(),
                Point::new(col as f64 * COL_W, row as f64 * ROW_H),
            );
            // spawn 子会话：从创建者那一列垂直向下，另起一条链。
            if let Some(kids) = spawn_children.get(&id) {
                for kid in kids {
                    queue.push_back((kid.clone(), col));
                }
            }
            // 会话后继：第一个孩子延续本行向右，其余 fork 从该列垂直向下
            // 另起一条链。
            cur = cont_children.get(&id).and_then(|ks| ks.first().cloned());
            if let Some(kids) = cont_children.get(&id) {
                for kid in kids.iter().skip(1) {
                    queue.push_back((kid.clone(), col));
                }
            }
            col += 1;
        }
    }

    // Context 材料：结构树最下方，每个独占一行。列基准优先取最深
    // 「引入者」（context_refs 含它的结构节点）的列 + 1，没有引入者再退回
    // 最深 source（context 节点自己的 context_refs）。
    let mut introducer_col: HashMap<String, usize> = HashMap::new();
    for meta in metas.values().filter(|m| !is_context(m)) {
        let Some(p) = pos.get(&meta.id) else {
            continue;
        };
        let col = (p.x / COL_W).round() as usize;
        for r in &meta.context_refs {
            if metas.get(r).is_some_and(is_context) {
                let slot = introducer_col.entry(r.clone()).or_insert(0);
                *slot = (*slot).max(col);
            }
        }
    }
    let band_start = if pos.is_empty() {
        0.0
    } else {
        pos.values().map(|p| p.y).fold(0.0, f64::max) + ROW_H
    };
    let mut ctxs: Vec<&NodeMeta> = metas.values().filter(|m| is_context(m)).collect();
    ctxs.sort_by_key(|m| m.created_at);
    for (i, meta) in ctxs.iter().enumerate() {
        let col = match introducer_col.get(&meta.id) {
            Some(col) => *col + 1,
            None => {
                let c = meta
                    .context_refs
                    .iter()
                    .filter_map(|r| pos.get(r).map(|p| (p.x / COL_W).round() as usize))
                    .max();
                c.map(|c| c + 1).unwrap_or(0)
            }
        };
        pos.insert(
            meta.id.clone(),
            Point::new(col as f64 * COL_W, band_start + i as f64 * ROW_H),
        );
    }
    pos
}

/// 折叠隐藏集：collapsed 里每个创建者直接 spawn 的根 + 其整棵子树。
/// 递归必须沿 **parent（会话后继）与 created_by（嵌套 spawn）两种边**
/// 都走——只走 parent 会漏掉被隐藏回合再 spawn 出的孙代子树：它们的
/// created_by 悬空后在布局里掉成新的根节点，折叠后树就乱了。注意种子
/// 只含 spawn 的根，创建者自己的会话后继（parent 孩子）不能隐藏。
fn collect_hidden(
    metas: &HashMap<String, NodeMeta>,
    collapsed: &HashSet<String>,
) -> HashSet<String> {
    let mut descendants: HashMap<String, Vec<String>> = HashMap::new();
    let mut spawned: HashMap<String, Vec<String>> = HashMap::new();
    for meta in metas.values() {
        if let Some(p) = meta.parent.as_deref() {
            descendants
                .entry(p.to_string())
                .or_default()
                .push(meta.id.clone());
        }
        if let Some(cb) = meta.created_by.as_deref() {
            spawned
                .entry(cb.to_string())
                .or_default()
                .push(meta.id.clone());
        }
    }
    let mut hidden: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = collapsed
        .iter()
        .flat_map(|c| spawned.get(c).cloned().unwrap_or_default())
        .collect();
    while let Some(id) = stack.pop() {
        if !hidden.insert(id.clone()) {
            continue;
        }
        if let Some(kids) = descendants.get(&id) {
            stack.extend(kids.iter().cloned());
        }
        if let Some(kids) = spawned.get(&id) {
            stack.extend(kids.iter().cloned());
        }
    }
    hidden
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
            // 占位 meta 没有 actor 来源：沿会话链继承父节点的发起方，
            // 否则人发起的在飞回合会被误标成 agent 发起的样式。
            let actor = turn
                .parent
                .as_deref()
                .and_then(|p| metas.get(p))
                .map(|m| m.actor.clone())
                .unwrap_or_else(|| "agent".to_string());
            metas.insert(
                turn.node_id.clone(),
                NodeMeta {
                    id: turn.node_id.clone(),
                    parent: turn.parent.clone(),
                    context_refs: vec![],
                    kind: NodeKindTag::Turn,
                    outcome: None,
                    actor,
                    created_at: u64::MAX,
                    usage: None,
                    context_tokens: None,
                    created_by: None,
                    model: None,
                    tools: vec![],
                    preview,
                },
            );
        }
    }
    let inflight_ids: HashSet<String> = inflights.keys().cloned().collect();

    // ---- spawn 组折叠：collapsed_spawns 里的创建者，其 spawn 子树
    // （created_by = 创建者的根 + 整棵后继子树）整体不渲染。 ----
    let collapsed = state.collapsed_spawns.read().clone();
    // 每个创建者 turn 直接 spawn 出的根节点列表（按 created_at 排序）。
    let mut spawn_children: HashMap<String, Vec<String>> = HashMap::new();
    for meta in metas.values() {
        if let Some(cb) = meta.created_by.as_deref() {
            spawn_children.entry(cb.to_string()).or_default().push(meta.id.clone());
        }
    }
    for list in spawn_children.values_mut() {
        list.sort_by_key(|id| metas.get(id).map(|m| m.created_at).unwrap_or(0));
    }
    if !collapsed.is_empty() {
        let hidden = collect_hidden(&metas, &collapsed);
        metas.retain(|id, _| !hidden.contains(id));
    }
    // 折叠徽章计数用的映射在过滤前算好（子树隐藏了，创建者还在）。
    let spawn_counts: HashMap<String, usize> =
        spawn_children.iter().map(|(k, v)| (k.clone(), v.len())).collect();

    let tip = state.current().and_then(|c| c.node);
    let pending_attach = state.pending_attach.read().clone();
    let selected = state.selected.read().clone();
    let selection = state.selection.read().clone();
    let overrides = state.graph_positions.read();

    // ---- nodes: auto layout + manual drag overrides + state classes ----
    let auto = layout(&metas);
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
            // 有 spawn 子会话的回合卡片多一条内置底栏（折叠条），更高。
            let has_strip = spawn_counts.get(&meta.id).copied().unwrap_or(0) > 0;
            let size = Size::new(CARD_W, CARD_H + if has_strip { STRIP_H } else { 0.0 });
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
            // 发起方着色：人发起的节点（用户输入 / 人发起的回合）accent
            // 左侧条，agent spawn 的回合紫色左侧条——一眼区分「我说的」
            // 和 agent 繁殖出的节点。
            if meta.created_by.is_none()
                && (meta.kind == NodeKindTag::Input
                    || (meta.kind == NodeKindTag::Turn && meta.actor == "human"))
            {
                class.push_str("user-authored ");
            }
            if meta.kind == NodeKindTag::Turn && meta.actor != "human" {
                class.push_str("agent-authored ");
            }
            // 框选选中集（复制/粘贴用）。
            if selection.contains(&meta.id) {
                class.push_str("box-selected ");
            }
            FlowNode::new(meta.id.clone(), position, size).with_class(class)
        })
        .collect();

    // ---- edges: 同一行的（输入→回合、回合→主链后继输入）走水平直线；
    // 跨行的（fork 子链、spawn 溯源）走垂直直线——spawn 虚线区分溯源，
    // fork 实线。引用边只画「引入」：结构节点（input/turn）→ 它引入的
    // context 节点（纵向虚线，材料带在结构树下方）。context 节点的溯源不
    // 画边，留在详情面板的「源自」列表里。 ----
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
            // 同行（主链延续）水平直线；跨行（fork 子链）垂直直线。
            let same_row = auto
                .get(parent)
                .zip(auto.get(&meta.id))
                .is_some_and(|(a, b)| a.y == b.y);
            let route = if same_row {
                EdgeRoute::Horizontal
            } else {
                EdgeRoute::Vertical
            };
            edges.push(FlowEdge {
                id: format!("p-{}", meta.id),
                source: NodeId::from(parent),
                target: NodeId::from(meta.id.as_str()),
                route,
                emphasis: edge_emphasis(parent, &meta.id, &hover_chain),
                ..Default::default()
            });
        }
        // spawn 边（provenance）：创建者 turn → 它 spawn 出的根节点（在
        // 创建者下一行同一列），垂直直线 + 虚线区分「创建/归属」。
        if let Some(creator) = meta.created_by.as_deref().filter(|c| metas.contains_key(*c)) {
            edges.push(FlowEdge {
                id: format!("s-{}", meta.id),
                source: NodeId::from(creator),
                target: NodeId::from(meta.id.as_str()),
                route: EdgeRoute::Vertical,
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
                route: EdgeRoute::Vertical,
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
                        // 点空白 = detach：取消选中与框选选择集，并把指针置空。
                        // - 空闲会话：服务端 detach，下一次输入从新的根开始；
                        // - 草稿态：清掉待定落点（本来就什么都没指）；
                        // - 会话忙：不动指针（服务端也会 409）。
                        state.selected.set(None);
                        state.selected_body.set(None);
                        state.selection.set(Default::default());
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
                    on_box_select: move |sel: dioxus_flow::BoxSelect| {
                        let ids: HashSet<String> = sel.nodes.into_iter().map(|i| i.0).collect();
                        if sel.additive {
                            state.selection.write().extend(ids);
                        } else {
                            state.selection.set(ids);
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
                    {
                        let selection_count = selection.len();
                        let clipboard = state.clipboard.read().clone();
                        let clipboard_count =
                            clipboard.as_ref().map(|(_, ids)| ids.len()).unwrap_or(0);
                        rsx! {
                            button {
                                class: "graph-tool-btn",
                                disabled: selection_count == 0,
                                title: "复制框选的子树（可跨图粘贴）",
                                onclick: move |_| {
                                    let graph = state.current_graph.read().clone();
                                    let mut ids: Vec<String> =
                                        state.selection.read().iter().cloned().collect();
                                    ids.sort();
                                    state.clipboard.set(Some((graph, ids)));
                                },
                                "复制 {selection_count}"
                            }
                            button {
                                class: "graph-tool-btn",
                                disabled: clipboard_count == 0,
                                title: "把剪贴板里的子树克隆进当前图（新 id，含后继与引用的材料）",
                                onclick: move |_| {
                                    let Some((from_graph, ids)) = state.clipboard.read().clone()
                                    else {
                                        return;
                                    };
                                    spawn(async move {
                                        match api::clone_subgraph(&from_graph, &ids).await {
                                            Ok(n) => {
                                                // 克隆进来的新节点直接成为选择集，方便连续操作
                                                state.selection.set(Default::default());
                                                if n == 0 {
                                                    state.set_error("没有可克隆的节点".to_string());
                                                }
                                            }
                                            Err(e) => state.set_error(format!("粘贴失败: {e}")),
                                        }
                                    });
                                },
                                "粘贴 {clipboard_count}"
                            }
                        }
                    }
                }
            }
            if selected.is_some() {
                DetailPanel {}
            }
        }
    }
}

/// 单节点卡片：顶行 kind 徽标 + 短 id，右侧 outcome 彩点（在飞时换「进行
/// 中」）；中间 preview 两行截断；底行 actor · ctx · usage（单行不折行，
/// actor 超长省略）。spawn 了子会话的回合卡片底部内置一条通长折叠底栏
/// （SpawnStrip，卡片加高 STRIP_H，圆角由卡片 overflow 裁出），model 标签
/// 收进栏内；没有子会话时 model 标签单独挂在卡片下方。
fn node_card(
    meta: &NodeMeta,
    _is_tip: bool,
    _is_selected: bool,
    is_inflight: bool,
    spawn_count: usize,
    spawn_collapsed: bool,
) -> Element {
    let kind = kind_label(meta.kind);
    // 「发送过的 model」标签只显示模型名（缓存信息收归卡片底行，职责分离）。
    let model_info = meta.model.as_ref().map(|model| crate::chat::short_model(model));
    let strip_class = if spawn_count > 0 { "fcard-strip" } else { "" };
    rsx! {
        div { class: "fcard fcard-{kind} {strip_class}",
            div { class: "fcard-top",
                span { class: "fcard-kind kind-{kind}", {kind_name(meta.kind)} }
                span { class: "fcard-id", "#{short_id(&meta.id)}" }
                if is_inflight {
                    span { class: "fcard-running", "进行中" }
                }
                if let Some(outcome) = meta.outcome {
                    span { class: "fcard-outcome", title: "{outcome.label()}",
                        span { class: "outcome-dot outcome-{outcome.label()}" }
                    }
                }
            }
            div { class: "fcard-preview", "{meta.preview}" }
            div { class: "fcard-bottom",
                span { class: "fcard-actor", title: "{meta.actor}", "{meta.actor}" }
                if let Some(ctx) = meta.context_tokens {
                    span { class: "fcard-ctx", title: "上下文量（该回合最后一次调用的 input tokens）",
                        "ctx {fmt_tokens(ctx)}"
                    }
                }
                if let Some(usage) = &meta.usage {
                    span { class: "fcard-usage",
                        "↑{usage.input_tokens} ↓{usage.output_tokens}"
                        if usage.input_tokens > 0 && usage.cached_input_tokens > 0 {
                            " · 缓存{usage.cached_input_tokens * 100 / usage.input_tokens}%"
                        }
                    }
                }
            }
            if spawn_count > 0 {
                SpawnStrip {
                    creator: meta.id.clone(),
                    count: spawn_count,
                    collapsed: spawn_collapsed,
                    model_info: model_info.clone(),
                }
            }
        }
        if spawn_count == 0
            && let Some(info) = model_info
        {
            div { class: "fcard-model-tag", title: meta.model.clone().unwrap_or_default(),
                "{info}"
            }
        }
    }
}

/// spawn 折叠条：回合卡片内部的通长底栏（flex 子元素 + 左右负边距顶到卡片
/// 内边距外，顶部分隔线；圆角靠卡片 overflow:hidden 裁出）。左侧折叠图标
/// + 子会话计数，右侧顺带显示 model 标签。点击折叠/展开该回合 spawn 出的
/// 整棵子树。必须拦住 mousedown/mouseup，否则节点的拖拽/选中逻辑会抢事件。
#[component]
fn SpawnStrip(creator: String, count: usize, collapsed: bool, model_info: Option<String>) -> Element {
    let mut state = use_context::<AppState>();
    rsx! {
        button {
            class: if collapsed { "spawn-strip collapsed" } else { "spawn-strip" },
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
            span { class: "spawn-strip-chevron", if collapsed { "▸" } else { "▾" } }
            span { "{count} 子会话" }
            if let Some(info) = model_info {
                span { class: "spawn-strip-model", "{info}" }
            }
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
                        if let Some(model) = &meta.model {
                            dt { "model" }
                            dd { "{model}" }
                        }
                        if matches!(meta.kind, NodeKindTag::Input | NodeKindTag::Turn) {
                            // Input = 请求的工具列表；Turn = 该轮有效集
                            // （depth 到顶等隐式变化在这可见）。
                            dt { "工具" }
                            dd { "{crate::state::tools_label(&meta.tools)}" }
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
                    div { class: "input-toolbar",
                        crate::chat::ModelPicker {}
                        crate::chat::ToolToggles {}
                    }
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
                    span { class: "bubble-usage", "{usage_label(usage)}" }
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
                                div { class: "bubble-text markdown", dangerous_inner_html: markdown_html(response_text) }
                            }
                            div { class: "step-usage",
                                "本次调用 {usage_label(usage)}"
                            }
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

#[cfg(test)]
mod layout_tests {
    use super::*;

    /// 用真实 smoke 图的 journal 复现布局，检查卡片 AABB 无重叠。
    /// 环境没有该文件时跳过（本地调试性质）。
    #[test]
    fn real_smoke_graph_has_no_card_overlap() {
        let journal = std::path::Path::new("/tmp/rua-smoke/.rua/graphs/测试-梳理 repo/journal.jsonl");
        let Ok(text) = std::fs::read_to_string(journal) else {
            eprintln!("smoke graph not found, skipping");
            return;
        };
        let mut metas: HashMap<String, NodeMeta> = HashMap::new();
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v["event"] == "node_committed" {
                let meta: NodeMeta = serde_json::from_value(v["meta"].clone()).unwrap();
                metas.insert(meta.id.clone(), meta);
            }
        }
        assert!(!metas.is_empty());
        let pos = layout(&metas);
        // 每个节点都必须有位置（缺失会掉到 (0,0) 全部糊在一起）
        for id in metas.keys() {
            assert!(pos.contains_key(id), "node {id} missing from layout");
        }
        let mut cards: Vec<(&str, f64, f64)> = pos
            .iter()
            .map(|(id, p)| (id.as_str(), p.x, p.y))
            .collect();
        cards.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let mut overlaps = 0;
        for (i, a) in cards.iter().enumerate() {
            for b in &cards[i + 1..] {
                if b.1 - a.1 >= COL_W {
                    break;
                }
                let x_overlap = a.1 < b.1 + CARD_W && b.1 < a.1 + CARD_W;
                let y_overlap = a.2 < b.2 + CARD_H && b.2 < a.2 + CARD_H;
                if x_overlap && y_overlap {
                    overlaps += 1;
                    if overlaps <= 5 {
                        eprintln!(
                            "overlap: {} ({},{}) vs {} ({},{})",
                            a.0, a.1, a.2, b.0, b.1, b.2
                        );
                    }
                }
            }
        }
        assert_eq!(overlaps, 0, "{overlaps} card overlaps");
    }

    /// 打印 smoke 图布局的 ASCII 鸟瞰（调试用，--nocapture 看）。
    #[test]
    fn print_ascii_birdview() {
        let journal = std::path::Path::new("/tmp/rua-smoke/.rua/graphs/测试-梳理 repo/journal.jsonl");
        let Ok(text) = std::fs::read_to_string(journal) else {
            return;
        };
        let mut metas: HashMap<String, NodeMeta> = HashMap::new();
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v["event"] == "node_committed" {
                let meta: NodeMeta = serde_json::from_value(v["meta"].clone()).unwrap();
                metas.insert(meta.id.clone(), meta);
            }
        }
        let pos = layout(&metas);
        let max_x = pos.values().map(|p| p.x).fold(0.0, f64::max);
        let max_y = pos.values().map(|p| p.y).fold(0.0, f64::max);
        let cols = (max_x / COL_W) as usize + 1;
        let rows = (max_y / ROW_H) as usize + 1;
        eprintln!("extent: {cols} cols x {rows} rows ({} nodes)", pos.len());
        let mut grid = vec![vec!['.'; cols]; rows];
        for (id, p) in &pos {
            let c = (p.x / COL_W) as usize;
            let r = (p.y / ROW_H) as usize;
            grid[r][c] = match metas[id].kind {
                NodeKindTag::Input => 'i',
                NodeKindTag::Turn => 'T',
                NodeKindTag::Context => 'M',
            };
        }
        for row in &grid {
            eprintln!("{}", row.iter().map(|c| format!("{c} ")).collect::<String>());
        }
    }

    fn load_smoke_metas() -> Option<HashMap<String, NodeMeta>> {
        let journal = std::path::Path::new("/tmp/rua-smoke/.rua/graphs/测试-梳理 repo/journal.jsonl");
        let text = std::fs::read_to_string(journal).ok()?;
        let mut metas = HashMap::new();
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v["event"] == "node_committed" {
                let meta: NodeMeta = serde_json::from_value(v["meta"].clone()).unwrap();
                metas.insert(meta.id.clone(), meta);
            }
        }
        Some(metas)
    }

    /// 折叠语义：嵌套 spawn（子会话里再 spawn 的孙代子树）必须一起隐藏，
    /// 否则 created_by 悬空、孙代在布局里掉成新根（折叠后树乱掉）；创建者
    /// 自己的会话后继不能跟着隐藏。
    #[test]
    fn collapse_hides_nested_spawn_subtrees() {
        use NodeKindTag::{Input, Turn};
        fn meta(
            id: &str,
            parent: Option<&str>,
            created_by: Option<&str>,
            kind: NodeKindTag,
        ) -> NodeMeta {
            NodeMeta {
                id: id.to_string(),
                parent: parent.map(str::to_string),
                context_refs: vec![],
                kind,
                outcome: None,
                actor: "test".to_string(),
                created_at: 0,
                usage: None,
                context_tokens: None,
                created_by: created_by.map(str::to_string),
                model: None,
                tools: vec![],
                preview: String::new(),
            }
        }
        let metas: HashMap<String, NodeMeta> = [
            meta("u1", None, None, Input),
            meta("t1", Some("u1"), None, Turn),
            // t1 spawn 的子会话
            meta("a", None, Some("t1"), Input),
            meta("at", Some("a"), None, Turn),
            // at 在子会话里再 spawn 的孙代子树
            meta("b", None, Some("at"), Input),
            meta("bt", Some("b"), None, Turn),
            // t1 的会话后继（必须保持可见）
            meta("u2", Some("t1"), None, Input),
            meta("u2t", Some("u2"), None, Turn),
        ]
        .into_iter()
        .map(|m| (m.id.clone(), m))
        .collect();
        let hidden = collect_hidden(&metas, &HashSet::from(["t1".to_string()]));
        let want: HashSet<String> = ["a", "at", "b", "bt"].iter().map(|s| s.to_string()).collect();
        assert_eq!(hidden, want);
    }

    /// 真实 smoke 图：每个创建者都单独折叠一次，可见集里不得有
    /// created_by 悬空的 spawn 根（那会在布局里掉成新根把树搞乱）。
    #[test]
    fn collapse_leaves_no_orphan_spawn_roots() {
        let Some(metas) = load_smoke_metas() else {
            eprintln!("smoke graph not found, skipping");
            return;
        };
        let creators: HashSet<String> =
            metas.values().filter_map(|m| m.created_by.clone()).collect();
        assert!(!creators.is_empty());
        for creator in creators {
            let hidden = collect_hidden(&metas, &HashSet::from([creator.clone()]));
            for m in metas.values() {
                if hidden.contains(&m.id) {
                    continue;
                }
                if let Some(cb) = &m.created_by {
                    assert!(
                        !hidden.contains(cb),
                        "collapsing {creator} leaves orphan spawn root {} (creator {cb} hidden)",
                        m.id
                    );
                }
            }
        }
    }
}
