//! Chat view: the current cursor's chain as a conversation.
//!
//! 链数据是轻量 meta（chain 端点不含 steps；Input 正文内联在 meta.text）。
//! 每个 Turn（连同它的输入气泡）一个容器 `turn-<node_id>`，steps 详情经
//! IntersectionObserver（±400px 预取）懒加载进 `state.turn_details`。
//! 滚动跟随：用户在底部（阈值 40px）时新内容贴底，上滚解除并显示
//! 「回到底部」浮钮。左侧 TurnNav 刻度导航，右侧 inspect 侧栏跟随
//! `focused_turn`（视口内最靠上的可见 Turn）。

use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use crate::state::{
    AppState, Inflight, InflightItem, cancel_current_turn, load_turn_detail, move_current_cursor,
    send_current_input,
};
use crate::types::*;

/// 贴底判定阈值（px）。
const BOTTOM_THRESHOLD: f64 = 40.0;
/// Turn 详情懒加载的预取余量（rootMargin）。
const PREFETCH_MARGIN: &str = "400px 0px";

/// Assistant 文本按 markdown 渲染成 HTML，由调用方用 `dangerous_inner_html`
/// 注入。内容来自本地自用的 LLM（rua-server 单用户、只绑 127.0.0.1），
/// 不做 HTML 消毒。
pub(crate) fn markdown_html(text: &str) -> String {
    let options = pulldown_cmark::Options::ENABLE_TABLES
        | pulldown_cmark::Options::ENABLE_STRIKETHROUGH
        | pulldown_cmark::Options::ENABLE_TASKLISTS;
    let parser = pulldown_cmark::Parser::new_ext(text, options);
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, parser);
    out
}

fn element_by_id(id: &str) -> Option<web_sys::Element> {
    web_sys::window()?.document()?.get_element_by_id(id)
}

/// 定位到某个 Turn 容器（instant，避免平滑滚动途中聚焦闪烁）。
fn scroll_to_turn(turn_id: &str) {
    if let Some(el) = element_by_id(&format!("turn-{turn_id}")) {
        let opts = web_sys::ScrollIntoViewOptions::new();
        opts.set_block(web_sys::ScrollLogicalPosition::Start);
        el.scroll_into_view_with_scroll_into_view_options(&opts);
    }
}

/// 定位并聚焦某个 Turn（导航条点击/滚轮跳轮、气泡「上下文」按钮共用）。
fn jump_to_turn(mut state: AppState, turn_id: &str) {
    state.focused_turn.set(Some(turn_id.to_string()));
    scroll_to_turn(turn_id);
}

/// 聚焦 Turn = 视口内最靠上的可见 Turn 容器（容器底边越过滚动口顶沿
/// 即算可见）；一个都够不到时回退到链上最后一个。
fn update_focus(mut state: AppState, turn_ids: &[String], scroller: &web_sys::HtmlElement) {
    let top = scroller.get_bounding_client_rect().top();
    let mut focused = None;
    for id in turn_ids {
        if let Some(el) = element_by_id(&format!("turn-{id}"))
            && el.get_bounding_client_rect().bottom() > top + 8.0
        {
            focused = Some(id.clone());
            break;
        }
    }
    if focused.is_none() {
        focused = turn_ids.last().cloned();
    }
    if state.focused_turn.peek().as_ref() != focused.as_ref() {
        state.focused_turn.set(focused);
    }
}

/// 链分组：Input + 紧随的 Turn 一对一个容器；Context 材料不进对话流；
/// 没有配对 Turn 的 Input（连续两个、或末尾新提交而 turn 尚在飞）单列。
struct TurnGroup {
    input: Option<NodeMeta>,
    turn: NodeMeta,
}

enum ChatItem {
    Turn(TurnGroup),
    StrayInput(NodeMeta),
}

fn group_chain(chain: &[NodeMeta]) -> Vec<ChatItem> {
    let mut items = Vec::new();
    let mut pending: Option<NodeMeta> = None;
    for meta in chain {
        match meta.kind {
            NodeKindTag::Input => {
                if let Some(prev) = pending.replace(meta.clone()) {
                    items.push(ChatItem::StrayInput(prev));
                }
            }
            NodeKindTag::Turn => {
                items.push(ChatItem::Turn(TurnGroup {
                    input: pending.take(),
                    turn: meta.clone(),
                }));
            }
            NodeKindTag::Context => {}
        }
    }
    if let Some(rest) = pending {
        items.push(ChatItem::StrayInput(rest));
    }
    items
}

#[component]
pub fn ChatView() -> Element {
    let mut state = use_context::<AppState>();
    let chain = state.chain.read();
    let busy = state.busy();
    let draft_mode = state.current_cursor.read().is_none();
    let pending_attach = state.pending_attach.read().clone();
    let panel_open = *state.context_panel_open.read();
    let follow = *state.follow_bottom.read();

    let mut scroll_el = use_signal(|| None::<web_sys::HtmlElement>);

    let items = group_chain(&chain);
    let empty = items.is_empty();
    let turn_ids: Vec<String> = items
        .iter()
        .filter_map(|item| match item {
            ChatItem::Turn(g) => Some(g.turn.id.clone()),
            _ => None,
        })
        .collect();

    // 切换会话 / 链增长 / 流式新内容：跟随模式下保持贴底（instant）；
    // 无论是否跟随都重算聚焦（内容不滚动时 onscroll 不会触发）。
    {
        let focus_ids = turn_ids.clone();
        use_effect(move || {
            let _cursor = state.current_cursor.read();
            let _chain_len = state.chain.read().len();
            let _inflight_len = state.current_inflight().map(|t| t.content_len());
            if let Some(el) = scroll_el.read().clone() {
                if *state.follow_bottom.read() {
                    el.set_scroll_top(el.scroll_height());
                }
                update_focus(state, &focus_ids, &el);
            }
        });
    }

    // 切换会话（含初次进入）：强制回到底部并恢复跟随。
    use_effect(move || {
        let _cursor = state.current_cursor.read();
        state.follow_bottom.set(true);
        if let Some(el) = scroll_el.read().clone() {
            el.set_scroll_top(el.scroll_height());
        }
    });

    let scroll_turn_ids = turn_ids.clone();
    rsx! {
        div { class: "chat-view",
            TurnNav {}
            div { class: "chat-main",
                div {
                    class: "chat-scroll",
                    onmounted: move |event| {
                        if let Some(el) = event.data().downcast::<web_sys::Element>() {
                            scroll_el.set(Some(el.clone().unchecked_into::<web_sys::HtmlElement>()));
                        }
                    },
                    onscroll: move |_| {
                        let Some(el) = scroll_el.read().clone() else { return };
                        let at_bottom = el.scroll_height()
                            - el.scroll_top()
                            - el.client_height()
                            <= BOTTOM_THRESHOLD as i32;
                        if *state.follow_bottom.peek() != at_bottom {
                            state.follow_bottom.set(at_bottom);
                        }
                        update_focus(state, &scroll_turn_ids, &el);
                    },
                    if empty {
                        div { class: "chat-empty",
                            if draft_mode {
                                p { "新会话草稿（尚未创建）。" }
                                p { "发送第一条消息即创建新会话，它将成为图的一个根节点。" }
                                if let Some(node_id) = &pending_attach {
                                    p { "将从节点 #{short_id(node_id)} 分叉。" }
                                }
                            } else {
                                p { "空会话。" }
                                p { "在下方输入第一条消息，它将成为图的一个根节点。" }
                            }
                        }
                    }
                    for item in items {
                        {
                            match item {
                                ChatItem::Turn(group) => rsx! {
                                    TurnContainer {
                                        key: "{group.turn.id}",
                                        input: group.input,
                                        turn: group.turn,
                                    }
                                },
                                ChatItem::StrayInput(meta) => rsx! {
                                    InputBubble { key: "{meta.id}", meta }
                                },
                            }
                        }
                    }
                    if let Some(turn) = state.current_inflight() {
                        InflightBubble { turn }
                    }
                }
                if !follow {
                    button {
                        class: "jump-bottom-btn",
                        title: "回到底部并恢复跟随流式输出",
                        onclick: move |_| {
                            state.follow_bottom.set(true);
                            if let Some(el) = scroll_el.read().clone() {
                                let opts = web_sys::ScrollToOptions::new();
                                opts.set_top(el.scroll_height() as f64);
                                opts.set_behavior(web_sys::ScrollBehavior::Smooth);
                                el.scroll_with_scroll_to_options(&opts);                            }
                        },
                        "回到底部 ↓"
                    }
                }
                InputArea { busy }
            }
            if panel_open {
                InspectPanel {}
            }
        }
    }
}

/// IntersectionObserver 持有器：组件卸载时先 disconnect 再释放回调闭包，
/// 避免 JS 回调打进已释放的 Rust 闭包。
struct TurnObserver {
    observer: web_sys::IntersectionObserver,
    _closure: wasm_bindgen::closure::Closure<
        dyn FnMut(Vec<web_sys::IntersectionObserverEntry>, web_sys::IntersectionObserver),
    >,
}

impl Drop for TurnObserver {
    fn drop(&mut self) {
        self.observer.disconnect();
    }
}

/// 一个 Turn 容器：输入气泡（meta.text 直接渲染）+ Turn 气泡。header /
/// footer 用 meta 渲染（actor、model、outcome、usage 都在），steps 区域
/// 懒加载——容器接近视口（±400px）且缓存未命中时才 get_node，未加载时
/// 渲染骨架以减少滚动跳动。
#[component]
fn TurnContainer(input: Option<NodeMeta>, turn: NodeMeta) -> Element {
    let state = use_context::<AppState>();
    let turn_id = turn.id.clone();
    let detail = state.turn_details.read().get(&turn_id).cloned();
    let mut near = use_signal(|| false);
    let mut observer = use_signal(|| None::<TurnObserver>);

    let effect_id = turn_id.clone();
    use_effect(move || {
        if *near.read() && !state.turn_details.read().contains_key(&effect_id) {
            load_turn_detail(state, effect_id.clone());
        }
    });

    // chain 上的 Turn 都已提交，outcome 必有；None 只是防御性兜底。
    let outcome = turn.outcome.unwrap_or(Outcome::Completed);
    rsx! {
        div {
            class: "turn-container",
            id: "turn-{turn_id}",
            onmounted: move |event| {
                let Some(el) = event.data().downcast::<web_sys::Element>().cloned() else {
                    return;
                };
                let target = el.clone();
                let cb = wasm_bindgen::closure::Closure::new(
                    move |entries: Vec<web_sys::IntersectionObserverEntry>,
                          obs: web_sys::IntersectionObserver| {
                        if entries.iter().any(|e| e.is_intersecting()) {
                            near.set(true);
                            obs.unobserve(&target);
                        }
                    },
                );
                let init = web_sys::IntersectionObserverInit::new();
                init.set_root_margin(PREFETCH_MARGIN);
                match web_sys::IntersectionObserver::new_with_options(
                    cb.as_ref().unchecked_ref(),
                    &init,
                ) {
                    Ok(obs) => {
                        obs.observe(&el);
                        observer.set(Some(TurnObserver {
                            observer: obs,
                            _closure: cb,
                        }));
                    }
                    // IntersectionObserver 不可用时退化为立即加载。
                    Err(_) => near.set(true),
                }
            },
            if let Some(input) = &input {
                InputBubble { meta: input.clone() }
            }
            div { class: "bubble bubble-turn outcome-{outcome.label()}",
                div { class: "bubble-header",
                    span { class: "bubble-actor", "{turn.actor}" }
                    span { class: "bubble-id", "#{short_id(&turn.id)}" }
                    FocusButton { node_id: turn.id.clone() }
                    ForkButton { node_id: turn.id.clone() }
                }
                match &detail {
                    Some(node) => rsx! {
                        if let NodeKind::Turn { steps, .. } = &node.kind {
                            for step in steps {
                                StepView { step: step.clone() }
                            }
                        }
                    },
                    None => rsx! {
                        div { class: "turn-skeleton",
                            div { class: "skeleton-bar w70" }
                            div { class: "skeleton-bar w90" }
                            div { class: "skeleton-bar w45" }
                        }
                    },
                }
                div { class: "bubble-footer",
                    if let Some(model) = &turn.model {
                        span { class: "bubble-model", "{model}" }
                    }
                    span { class: "badge outcome-{outcome.label()}", { outcome_label(outcome) } }
                    if let Some(usage) = &turn.usage {
                        UsageView { usage: *usage }
                    }
                }
            }
        }
    }
}

/// 输入气泡：直接用 meta.text 渲染，不需要详情请求。
#[component]
fn InputBubble(meta: NodeMeta) -> Element {
    let text = meta.text.clone().unwrap_or_default();
    rsx! {
        div { class: "bubble bubble-input",
            div { class: "bubble-header",
                span { class: "bubble-actor", "{meta.actor}" }
                span { class: "bubble-id", "#{short_id(&meta.id)}" }
                span { class: "badge", "{crate::state::tools_label(&meta.tools)}" }
            }
            div { class: "bubble-text", "{text}" }
        }
    }
}

fn outcome_label(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Completed => "完成",
        Outcome::Failed => "失败",
        Outcome::Cancelled => "已取消",
        Outcome::Interrupted => "已中断",
    }
}

#[component]
fn StepView(step: Step) -> Element {
    match step {
        Step::LlmCall {
            response_text,
            reasoning,
            ..
        } => rsx! {
            if let Some(reasoning) = reasoning.filter(|r| !r.is_empty()) {
                details { class: "reasoning",
                    summary { "思考过程" }
                    pre { class: "mono", "{reasoning}" }
                }
            }
            if !response_text.is_empty() {
                div { class: "bubble-text markdown", dangerous_inner_html: markdown_html(&response_text) }
            }
        },
        Step::ToolExec {
            name,
            args,
            output,
            duration_ms,
            ..
        } => rsx! {
            details { class: "tool-exec",
                summary {
                    span { class: "tool-name", "{name}" }
                    span { class: "tool-args", { compact_args(&args) } }
                    span { class: "tool-duration", "{duration_ms}ms" }
                }
                pre { class: "mono", "{output}" }
            }
        },
    }
}

/// Compact one-line args preview for tool exec lines.
pub(crate) fn compact_args(args: &serde_json::Value) -> String {
    let s = serde_json::to_string(args).unwrap_or_default();
    const MAX: usize = 80;
    if s.chars().count() > MAX {
        format!("{}…", s.chars().take(MAX).collect::<String>())
    } else {
        s
    }
}

/// 统一的 token 用量展示：`↑{in} ↓{out}[ · 缓存{cached}({pct}%)][ · 思考{r}]`，
/// 零值部分省略；pct = cached*100/input（input>0 且有缓存时）。聊天气泡
/// footer、图节点详情 turn 汇总行、LlmCall step 行三处共用。
pub(crate) fn usage_label(usage: &Usage) -> String {
    let mut s = format!("↑{} ↓{}", usage.input_tokens, usage.output_tokens);
    if usage.input_tokens > 0 && usage.cached_input_tokens > 0 {
        s.push_str(&format!(
            " · 缓存{}({}%)",
            usage.cached_input_tokens,
            usage.cached_input_tokens * 100 / usage.input_tokens
        ));
    }
    if usage.reasoning_tokens > 0 {
        s.push_str(&format!(" · 思考{}", usage.reasoning_tokens));
    }
    s
}

#[component]
fn UsageView(usage: Usage) -> Element {
    rsx! {
        span { class: "bubble-usage", "{usage_label(&usage)}" }
    }
}

#[component]
fn ForkButton(node_id: String) -> Element {
    let state = use_context::<AppState>();
    rsx! {
        button {
            class: "fork-btn",
            title: "把游标移到这个节点（fork 从这里继续）",
            onclick: move |_| {
                let state = state;
                let node_id = node_id.clone();
                spawn(async move {
                    move_current_cursor(state, &node_id).await;
                });
            },
            "fork 到这里"
        }
    }
}

/// Turn 气泡 header 的「上下文」按钮：定位并聚焦本轮（滚动跟随会让
/// inspect 侧栏自然显示该轮快照）。
#[component]
fn FocusButton(node_id: String) -> Element {
    let mut state = use_context::<AppState>();
    rsx! {
        button {
            class: "ctx-jump-btn",
            title: "定位到本轮，并在 inspect 侧栏查看该轮快照",
            onclick: move |_| {
                state.context_panel_open.set(true);
                jump_to_turn(state, &node_id);
            },
            "上下文"
        }
    }
}

#[component]
fn InflightBubble(turn: Inflight) -> Element {
    let state = use_context::<AppState>();
    rsx! {
        div { class: "bubble bubble-turn bubble-inflight",
            div { class: "bubble-header",
                span { class: "bubble-actor", "agent" }
                span { class: "bubble-id", "#{short_id(&turn.node_id)}" }
                span { class: "inflight-hint", "生成中…" }
                button {
                    class: "cancel-btn",
                    onclick: move |_| {
                        spawn(async move {
                            cancel_current_turn(state).await;
                        });
                    },
                    "取消"
                }
            }
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
                            span { class: "tool-args", { compact_args(&tool.args) } }
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

// ---- 左侧 Turn 导航条 ----

/// 刻度体量映射：usage 总量（in+out tokens）线性映射到宽度 10..28px 与
/// 透明度 0.35..0.85，4000 tokens 封顶；无 usage 记录给最小刻度。
fn tick_scale(usage: Option<Usage>) -> (u32, f64) {
    let total = usage
        .map(|u| u.input_tokens + u.output_tokens)
        .unwrap_or(0);
    let scale = (total as f64 / 4000.0).min(1.0);
    let width = 10 + (18.0 * scale).round() as u32;
    let opacity = 0.35 + 0.5 * scale;
    (width, opacity)
}

/// 常显细条，在聊天主区左缘：每个 Turn 一个刻度，长度/透明度映射该轮
/// 体量；hover 出对应 Input 的 preview，点击定位该轮，条上滚轮以聚焦
/// Turn 为基准按轮上/下跳。主内容区滚轮不受影响（导航条本身不可滚动
/// 冒泡不到聊天容器）。
#[component]
fn TurnNav() -> Element {
    let state = use_context::<AppState>();
    let chain = state.chain.read();
    let focused = state.focused_turn.read().clone();

    // (turn meta, 对应 Input 的 preview)——配对规则同 group_chain。
    let mut ticks: Vec<(NodeMeta, String)> = Vec::new();
    let mut pending: Option<&NodeMeta> = None;
    for m in chain.iter() {
        match m.kind {
            NodeKindTag::Input => pending = Some(m),
            NodeKindTag::Turn => {
                let tip = pending
                    .filter(|i| !i.preview.is_empty())
                    .map(|i| i.preview.clone())
                    .unwrap_or_else(|| m.preview.clone());
                ticks.push((m.clone(), tip));
                pending = None;
            }
            NodeKindTag::Context => {}
        }
    }
    let turn_ids: Vec<String> = ticks.iter().map(|(m, _)| m.id.clone()).collect();

    rsx! {
        div {
            class: "turn-nav",
            onwheel: move |e| {
                let dy = e.delta().strip_units().y;
                if dy == 0.0 {
                    return;
                }
                nav_jump(state, &turn_ids, dy > 0.0);
            },
            for (meta, tip) in ticks {
                {
                    let id = meta.id.clone();
                    let active = focused.as_deref() == Some(id.as_str());
                    let (w, o) = tick_scale(meta.usage);
                    rsx! {
                        button {
                            key: "{id}",
                            class: if active { "turn-tick active" } else { "turn-tick" },
                            style: "width: {w}px; opacity: {o};",
                            title: "点击定位到该轮",
                            onclick: move |_| {
                                jump_to_turn(state, &id);
                            },
                            span { class: "turn-nav-tip", "{tip}" }
                        }
                    }
                }
            }
        }
    }
}

/// 导航条滚轮：以聚焦 Turn 为基准按轮上/下跳（无聚焦时向下跳首轮、
/// 向上跳末轮）。
fn nav_jump(state: AppState, turn_ids: &[String], down: bool) {
    if turn_ids.is_empty() {
        return;
    }
    let cur = state
        .focused_turn
        .read()
        .clone()
        .and_then(|f| turn_ids.iter().position(|id| id == &f));
    let next = match cur {
        Some(i) if down => (i + 1).min(turn_ids.len() - 1),
        Some(i) => i.saturating_sub(1),
        None if down => 0,
        None => turn_ids.len() - 1,
    };
    jump_to_turn(state, &turn_ids[next]);
}

/// 模型选择下拉（发送时覆盖；「默认」= daemon 配置模型）。多 provider：
/// 条目按 provider 分组（optgroup），值是 model ref（默认 provider 组用
/// 裸模型名，具名 provider 用 "provider/model"）。
/// 注意：换模型/改工具列表都会改变请求前缀，前缀缓存会失效。
#[component]
pub(crate) fn ModelPicker() -> Element {
    let mut state = use_context::<AppState>();
    let models = state.models.read().clone();
    let selected = state.selected_model.read().clone();
    let default_model = state.default_model.read().clone();
    // 按 provider 分组，保持返回顺序（server 保证默认 provider 在前）。
    let mut groups: Vec<(String, Vec<ModelEntry>)> = Vec::new();
    for e in &models {
        if let Some(g) = groups.iter_mut().find(|(p, _)| p == &e.provider) {
            g.1.push(e.clone());
        } else {
            groups.push((e.provider.clone(), vec![e.clone()]));
        }
    }
    rsx! {
        select {
            class: "model-select",
            title: "本次发送使用的模型（默认 = daemon 配置）",
            value: selected.clone().unwrap_or_default(),
            onchange: move |e| {
                let v = e.value();
                state.selected_model.set(if v.is_empty() { None } else { Some(v) });
            },
            option { value: "", "模型: 默认 ({short_model(&default_model)})" }
            for (provider, entries) in groups {
                // rua-engine 的 DEFAULT_PROVIDER = "default"（UI 不依赖 rua-engine）。
                optgroup { label: if provider == "default" { "默认 provider".to_string() } else { provider.clone() },
                    for e in entries {
                        option {
                            key: "{e.id}",
                            value: "{e.id}",
                            selected: selected.as_deref() == Some(e.id.as_str()),
                            "{e.model}"
                        }
                    }
                }
            }
        }
    }
}

/// 模型名缩短：取最后一段路径/冒号前缀，最多 20 字符。
pub(crate) fn short_model(model: &str) -> String {
    let tail = model.rsplit('/').next().unwrap_or(model);
    if tail.chars().count() > 20 {
        format!("{}…", tail.chars().take(19).collect::<String>())
    } else {
        tail.to_string()
    }
}

/// 工具覆盖：上拉勾选列表 + 重置按钮（发送时覆盖；全开 = 不覆盖）。
/// 关工具会改请求前缀，前缀缓存失效——开发测试用。
#[component]
pub(crate) fn ToolToggles() -> Element {
    let mut state = use_context::<AppState>();
    let mut open = use_signal(|| false);
    let off = state.tools_off.read().clone();
    let total = AppState::ALL_TOOLS.len();
    let summary = if off.is_empty() {
        "工具: 全部".to_string()
    } else {
        format!("工具: {}/{}", total - off.len(), total)
    };
    rsx! {
        span { class: "tool-menu-wrap",
            // 打开时铺一个透明 backdrop，点外面即关闭。
            if *open.read() {
                div {
                    class: "tool-menu-backdrop",
                    onclick: move |_| open.set(false),
                }
                div { class: "tool-menu",
                    for tool in AppState::ALL_TOOLS {
                        {
                            let tool_static: &'static str = tool;
                            let enabled = !off.contains(tool_static);
                            rsx! {
                                label { key: "{tool_static}",
                                    input {
                                        r#type: "checkbox",
                                        checked: enabled,
                                        onchange: move |_| {
                                            let mut set = state.tools_off.write();
                                            if !set.remove(tool_static) {
                                                set.insert(tool_static.to_string());
                                            }
                                        },
                                    }
                                    "{tool_static}"
                                }
                            }
                        }
                    }
                }
            }
            button {
                class: "tool-menu-btn",
                title: "本次发送可用的工具（默认全开；关掉会使命中前缀缓存失效）",
                onclick: move |_| {
                    let cur = *open.read();
                    open.set(!cur);
                },
                "{summary} ▴"
            }
            if !off.is_empty() {
                button {
                    class: "tool-reset-btn",
                    title: "重置为全部工具（不覆盖）",
                    onclick: move |_| state.tools_off.write().clear(),
                    "重置"
                }
            }
        }
    }
}

#[component]
fn InputArea(busy: bool) -> Element {
    let mut state = use_context::<AppState>();
    let draft = state.draft.read().clone();

    let send = move || {
        let text = state.draft.read().trim().to_string();
        if !text.is_empty() && !state.busy() {
            spawn(async move {
                send_current_input(state, text).await;
            });
        }
    };

    rsx! {
        div { class: "input-area",
            div { class: "input-toolbar",
                ModelPicker {}
                ToolToggles {}
                InspectPanelToggle {}
            }
            textarea {
                class: "input-box",
                placeholder: if busy { "等待当前轮次结束…" } else { "输入消息，Enter 发送，Shift+Enter 换行" },
                value: "{draft}",
                oninput: move |e| state.draft.set(e.value()),
                onkeydown: move |e| {
                    if e.key() == Key::Enter && !e.modifiers().contains(Modifiers::SHIFT) {
                        e.prevent_default();
                        send();
                    }
                },
            }
            if busy {
                button {
                    class: "cancel-btn",
                    onclick: move |_| {
                        spawn(async move {
                            cancel_current_turn(state).await;
                        });
                    },
                    "取消"
                }
            } else {
                button {
                    class: "send-btn",
                    disabled: draft.trim().is_empty(),
                    onclick: move |_| send(),
                    "发送"
                }
            }
        }
    }
}

// ---- inspect 侧栏 ----

/// InputArea 工具栏的「上下文」开关按钮（带开关态样式）。
#[component]
fn InspectPanelToggle() -> Element {
    let mut state = use_context::<AppState>();
    let open = *state.context_panel_open.read();
    rsx! {
        button {
            class: if open { "ctx-toggle-btn active" } else { "ctx-toggle-btn" },
            title: "打开/关闭 inspect 侧栏（聚焦最新轮 = 预览下一轮请求；聚焦旧轮 = 该轮快照）",
            onclick: move |_| {
                let cur = *state.context_panel_open.read();
                state.context_panel_open.set(!cur);
            },
            "上下文"
        }
    }
}

/// 聊天视图右侧的 inspect 侧栏，与滚动联动：聚焦 Turn = 视口内最靠上
/// 的可见容器（滚动即聚焦，无需点击）。聚焦最新轮 / 进行中轮 / 未聚焦
/// 时显示下一轮请求的实时装配预览；聚焦旧轮显示该轮快照。
#[component]
fn InspectPanel() -> Element {
    let mut state = use_context::<AppState>();
    let chain = state.chain.read();
    let focused = state.focused_turn.read().clone();
    let last_turn_id = chain
        .iter()
        .rev()
        .find(|m| m.kind == NodeKindTag::Turn)
        .map(|m| m.id.clone());
    let inflight_id = state.current_inflight().map(|t| t.node_id);
    let preview = match &focused {
        None => true,
        Some(id) => Some(id) == last_turn_id.as_ref() || Some(id) == inflight_id.as_ref(),
    };

    rsx! {
        aside { class: "context-panel",
            div { class: "ctx-tabs",
                span { class: "ctx-panel-title",
                    if preview { "下一轮请求（预览）" } else { "回合快照" }
                }
                button {
                    class: "detail-close",
                    title: "关闭侧栏",
                    onclick: move |_| state.context_panel_open.set(false),
                    "×"
                }
            }
            div { class: "ctx-scroll",
                if preview {
                    PreviewTab {}
                } else if let Some(turn_id) = focused {
                    TurnSnapshot { turn_id }
                }
            }
        }
    }
}

/// 预览：当前 tip + 工具勾选实时装配出的下一轮请求。换游标、链增长
/// （新节点 commit）、改工具勾选都会触发重新拉取。
#[component]
fn PreviewTab() -> Element {
    let mut state = use_context::<AppState>();
    let mut preview = use_signal(|| None::<ContextPreviewResponse>);
    let mut failed = use_signal(|| false);
    use_effect(move || {
        let cursor = state.current_cursor.read().clone();
        // 链长度（新节点 commit 后重拉）与工具勾选都是刷新触发源。
        let _chain_len = state.chain.read().len();
        let tools = state.tools_override();
        preview.set(None);
        failed.set(false);
        if let Some(cid) = cursor {
            spawn(async move {
                match crate::api::get_context_preview(&cid, tools.as_deref()).await {
                    Ok(p) => preview.set(Some(p)),
                    Err(e) => {
                        state.set_error(format!("获取上下文预览失败: {e}"));
                        failed.set(true);
                    }
                }
            });
        }
    });

    // draft 会话（无 cursor）没有服务端装配对象，显示空态。
    if state.current_cursor.read().is_none() {
        return rsx! {
            div { class: "ctx-empty",
                p { "新会话草稿尚无上下文。" }
                p { "发送第一条消息后即可预览下一轮请求。" }
            }
        };
    }
    let data = preview.read().clone();
    let failed = *failed.read();
    match data {
        Some(p) => {
            let prompt_chars = p.system_prompt.chars().count();
            rsx! {
                div { class: "ctx-section",
                    span { class: "ctx-label", "工具" }
                    span { class: "badge", "{crate::state::tools_label(&p.tools)}" }
                }
                details { class: "ctx-msg ctx-system",
                    summary {
                        span { class: "ctx-role ctx-role-system", "system" }
                        span { class: "ctx-msg-note", "动态组装的系统提示词" }
                        span { class: "ctx-msg-chars", "{prompt_chars} 字符" }
                    }
                    pre { class: "mono", "{p.system_prompt}" }
                }
                ContextMessageList { messages: p.messages }
            }
        }
        None if failed => rsx! {
            div { class: "ctx-empty", p { "预览加载失败（详见顶部错误条）。" } }
        },
        None => rsx! {
            div { class: "detail-loading", "装配预览加载中…" }
        },
    }
}

/// 旧轮快照：当时的系统提示词（首个 LlmCall 的 request 首条 System，可
/// 展开/收起）、usage 汇总（含缓存 %）、steps 概要列表。详情与聊天主区
/// 共用 `turn_details` 缓存；聚焦的轮可能还没懒加载过，这里兜底拉取。
#[component]
fn TurnSnapshot(turn_id: String) -> Element {
    let state = use_context::<AppState>();
    let load_id = turn_id.clone();
    use_effect(move || {
        if !state.turn_details.read().contains_key(&load_id) {
            load_turn_detail(state, load_id.clone());
        }
    });
    let detail = state.turn_details.read().get(&turn_id).cloned();
    let Some(node) = detail else {
        return rsx! {
            div { class: "detail-loading", "加载回合快照…" }
        };
    };
    let NodeKind::Turn {
        steps,
        usage,
        model,
        ..
    } = &node.kind
    else {
        return rsx! {};
    };
    let system_prompt = steps.iter().find_map(|s| match s {
        Step::LlmCall { request, .. } => match request.first() {
            Some(CoreMessageView::System { content }) => Some(content.clone()),
            _ => None,
        },
        _ => None,
    });

    rsx! {
        div { class: "ctx-section",
            span { class: "ctx-label", "回合 #{short_id(&turn_id)}" }
            span { class: "badge", "{short_model(model)}" }
        }
        div { class: "ctx-section",
            span { class: "ctx-label", "用量" }
            span { class: "ctx-call-usage", "{usage_label(usage)}" }
        }
        if let Some(prompt) = &system_prompt {
            details { class: "ctx-msg ctx-system",
                summary {
                    span { class: "ctx-role ctx-role-system", "system" }
                    span { class: "ctx-msg-note", "当时生效的系统提示词" }
                    span { class: "ctx-msg-chars", "{prompt.chars().count()} 字符" }
                }
                pre { class: "mono", "{prompt}" }
            }
        }
        div { class: "ctx-section",
            span { class: "ctx-label", "steps（{steps.len()}）" }
        }
        div { class: "ctx-step-list",
            for (i, step) in steps.iter().enumerate() {
                match step {
                    Step::LlmCall { usage, .. } => rsx! {
                        div { class: "ctx-step", key: "{i}",
                            span { class: "ctx-step-name", "LLM 调用" }
                            span { class: "ctx-step-note", "{usage_label(usage)}" }
                        }
                    },
                    Step::ToolExec { name, duration_ms, .. } => rsx! {
                        div { class: "ctx-step", key: "{i}",
                            span { class: "ctx-step-name mono", "tool {name}" }
                            span { class: "ctx-step-note", "{duration_ms}ms" }
                        }
                    },
                }
            }
        }
    }
}

/// 预览 / 快照共用的消息列表：每条一个折叠条目（role 徽标 + 字符数，
/// 展开看全文），底部汇总条数与总字符数。
#[component]
fn ContextMessageList(messages: Vec<CoreMessageView>) -> Element {
    let total: usize = messages.iter().map(|m| message_text(m).chars().count()).sum();
    let count = messages.len();
    rsx! {
        div { class: "ctx-msg-list",
            for (i, m) in messages.iter().enumerate() {
                {
                    let text = message_text(m);
                    let chars = text.chars().count();
                    let role = role_label(m);
                    rsx! {
                        details { key: "{i}", class: "ctx-msg",
                            summary {
                                span { class: "ctx-role ctx-role-{role}", "{role}" }
                                match m {
                                    CoreMessageView::ToolResult { name, .. } => rsx! {
                                        span { class: "ctx-msg-note mono", "{name}" }
                                    },
                                    CoreMessageView::Context { sources, .. } => rsx! {
                                        span { class: "ctx-msg-note", "来源 {sources.len()} 节点" }
                                    },
                                    CoreMessageView::Assistant { tool_calls, .. } if !tool_calls.is_empty() => rsx! {
                                        span { class: "ctx-msg-note", "{tool_calls.len()} 个工具调用" }
                                    },
                                    _ => rsx! {},
                                }
                                span { class: "ctx-msg-chars", "{chars} 字符" }
                            }
                            pre { class: "mono", "{text}" }
                        }
                    }
                }
            }
            div { class: "ctx-summary", "{count} 条消息 · 共 {total} 字符" }
        }
    }
}

fn role_label(m: &CoreMessageView) -> &'static str {
    match m {
        CoreMessageView::System { .. } => "system",
        CoreMessageView::User { .. } => "user",
        CoreMessageView::Assistant { .. } => "assistant",
        CoreMessageView::ToolResult { .. } => "tool",
        CoreMessageView::Context { .. } => "context",
    }
}

/// 条目展开后的全文：tool_calls 以 pretty JSON 附在 assistant 文本后。
fn message_text(m: &CoreMessageView) -> String {
    match m {
        CoreMessageView::System { content } | CoreMessageView::User { content } => content.clone(),
        CoreMessageView::Assistant { content, tool_calls } => {
            if tool_calls.is_empty() {
                content.clone()
            } else {
                format!(
                    "{content}\n\n[tool_calls]\n{}",
                    serde_json::to_string_pretty(tool_calls).unwrap_or_default()
                )
            }
        }
        CoreMessageView::ToolResult { output, .. } => output.clone(),
        CoreMessageView::Context { body, .. } => body.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, reasoning: u64, cached: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            reasoning_tokens: reasoning,
            cached_input_tokens: cached,
        }
    }

    #[test]
    fn usage_label_omits_zero_parts() {
        // 无缓存、无思考：只有 ↑in ↓out。
        assert_eq!(usage_label(&usage(932, 120, 0, 0)), "↑932 ↓120");
        // input 为 0 时不显示缓存（除零保护）。
        assert_eq!(usage_label(&usage(0, 0, 0, 0)), "↑0 ↓0");
    }

    #[test]
    fn usage_label_cache_percentage() {
        assert_eq!(
            usage_label(&usage(200, 50, 0, 100)),
            "↑200 ↓50 · 缓存100(50%)"
        );
        // 有缓存但 input 为 0：省略缓存部分（pct 无意义）。
        assert_eq!(usage_label(&usage(0, 5, 0, 3)), "↑0 ↓5");
    }

    #[test]
    fn usage_label_reasoning_part() {
        assert_eq!(usage_label(&usage(100, 20, 30, 0)), "↑100 ↓20 · 思考30");
        // 缓存在前、思考在后。
        assert_eq!(
            usage_label(&usage(1000, 200, 40, 250)),
            "↑1000 ↓200 · 缓存250(25%) · 思考40"
        );
    }

    fn meta(id: &str, kind: NodeKindTag) -> NodeMeta {
        NodeMeta {
            id: id.to_string(),
            parent: None,
            context_refs: vec![],
            kind,
            outcome: None,
            actor: "t".to_string(),
            created_at: 0,
            usage: None,
            context_tokens: None,
            created_by: None,
            distilled_from: None,
            model: None,
            tools: vec![],
            text: None,
            preview: String::new(),
        }
    }

    #[test]
    fn group_chain_pairs_input_with_following_turn() {
        let chain = vec![
            meta("i1", NodeKindTag::Input),
            meta("t1", NodeKindTag::Turn),
            meta("c", NodeKindTag::Context),
            meta("i2", NodeKindTag::Input),
            meta("t2", NodeKindTag::Turn),
            // 末尾新提交的 Input（turn 尚在飞）：单列。
            meta("i3", NodeKindTag::Input),
        ];
        let items = group_chain(&chain);
        assert_eq!(items.len(), 3);
        match &items[0] {
            ChatItem::Turn(g) => {
                assert_eq!(g.input.as_ref().unwrap().id, "i1");
                assert_eq!(g.turn.id, "t1");
            }
            _ => panic!("expected turn group"),
        }
        // Context 材料不进对话流。
        match &items[1] {
            ChatItem::Turn(g) => assert_eq!(g.turn.id, "t2"),
            _ => panic!("expected turn group"),
        }
        match &items[2] {
            ChatItem::StrayInput(m) => assert_eq!(m.id, "i3"),
            _ => panic!("expected stray input"),
        }
    }

    #[test]
    fn tick_scale_caps_at_max() {
        assert_eq!(tick_scale(None), (10, 0.35));
        assert_eq!(tick_scale(Some(usage(4000, 4000, 0, 0))), (28, 0.85));
        assert_eq!(tick_scale(Some(usage(1000, 1000, 0, 0))), (19, 0.6));
    }
}
