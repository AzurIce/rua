//! Chat view: the current cursor's chain as a conversation.

use dioxus::prelude::*;

use crate::api;
use crate::state::{AppState, Inflight, InflightItem, cancel_current_turn, move_current_cursor, send_current_input};
use crate::types::*;

/// Assistant 文本按 markdown 渲染成 HTML，由调用方用 `dangerous_inner_html`
/// 注入。内容来自本地自用的 LLM（rua-server 单用户、只绑 127.0.0.1），
/// 不做 HTML 消毒。
pub(crate) fn markdown_html(text: &str) -> String {    let options = pulldown_cmark::Options::ENABLE_TABLES
        | pulldown_cmark::Options::ENABLE_STRIKETHROUGH
        | pulldown_cmark::Options::ENABLE_TASKLISTS;
    let parser = pulldown_cmark::Parser::new_ext(text, options);
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, parser);
    out
}

#[component]
pub fn ChatView() -> Element {
    let state = use_context::<AppState>();
    let chain = state.chain.read();
    let busy = state.busy();
    let draft_mode = state.current_cursor.read().is_none();
    let pending_attach = state.pending_attach.read().clone();
    let panel_open = *state.context_panel_open.read();

    rsx! {
        div { class: "chat-view",
            div { class: "chat-main",
                div { class: "chat-scroll",
                    if chain.is_empty() {
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
                    for node in chain.iter() {
                        // Context 节点是材料，不出现在对话流里。
                        if !matches!(node.kind, NodeKind::Context { .. }) {
                            Bubble { key: "{node.id}", node: node.clone() }
                        }
                    }
                    if let Some(turn) = state.current_inflight() {
                        InflightBubble { turn }
                    }
                }
                InputArea { busy }
            }
            if panel_open {
                ContextPanel {}
            }
        }
    }
}

#[component]
fn Bubble(node: Node) -> Element {
    match &node.kind {
        NodeKind::Input { text, actor, tools } => {
            rsx! {
                div { class: "bubble bubble-input",
                    div { class: "bubble-header",
                        span { class: "bubble-actor", "{actor}" }
                        span { class: "bubble-id", "#{short_id(&node.id)}" }
                        span { class: "badge", "{crate::state::tools_label(tools)}" }
                    }
                    div { class: "bubble-text", "{text}" }
                }
            }
        }
        NodeKind::Turn {
            steps,
            outcome,
            actor,
            model,
            usage,
            ..
        } => {
            let outcome = *outcome;
            // LlmCall 次数：有调用记录的回合才能看上下文快照。
            let llm_calls = steps
                .iter()
                .filter(|s| matches!(s, Step::LlmCall { .. }))
                .count();
            rsx! {
                div { class: "bubble bubble-turn outcome-{outcome.label()}",
                    div { class: "bubble-header",
                        span { class: "bubble-actor", "{actor}" }
                        span { class: "bubble-id", "#{short_id(&node.id)}" }
                        if llm_calls > 0 {
                            SnapshotButton { node_id: node.id.clone(), last_call: llm_calls - 1 }
                        }
                        ForkButton { node_id: node.id.clone() }
                    }
                    for step in steps {
                        StepView { step: step.clone() }
                    }
                    div { class: "bubble-footer",
                        span { class: "bubble-model", "{model}" }
                        span { class: "badge outcome-{outcome.label()}", {outcome_label(outcome)} }
                        UsageView { usage: *usage }
                    }
                }
            }
        }
        NodeKind::Context { .. } => unreachable!(),
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
                    span { class: "tool-args", {compact_args(&args)} }
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

/// Turn 气泡 header 的「上下文」按钮：打开侧栏并把快照 tab 指到本回合
/// （默认落在最后一次 LlmCall，侧栏里可切换）。
#[component]
fn SnapshotButton(node_id: String, last_call: usize) -> Element {
    let mut state = use_context::<AppState>();
    rsx! {
        button {
            class: "ctx-jump-btn",
            title: "在上下文侧栏查看本轮发送给模型的请求快照",
            onclick: move |_| {
                state.snapshot_target.set(Some((node_id.clone(), last_call)));
                state.context_panel_open.set(true);
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
                // rua-core 的 DEFAULT_PROVIDER = "default"（UI 不依赖 rua-core）。
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
                ContextPanelToggle {}
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

// ---- 上下文侧栏 ----

/// InputArea 工具栏的「上下文」开关按钮（带开关态样式）。
#[component]
fn ContextPanelToggle() -> Element {
    let mut state = use_context::<AppState>();
    let open = *state.context_panel_open.read();
    rsx! {
        button {
            class: if open { "ctx-toggle-btn active" } else { "ctx-toggle-btn" },
            title: "打开/关闭上下文侧栏（预览下一轮请求 / 查看已发送快照）",
            onclick: move |_| {
                let cur = *state.context_panel_open.read();
                state.context_panel_open.set(!cur);
            },
            "上下文"
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextTab {
    /// 下一轮请求的实时装配预览（默认）。
    Preview,
    /// 已提交回合的 LlmCall 请求快照（逐字记录）。
    Snapshot,
}

/// 聊天视图右侧的上下文侧栏：预览 / 快照两个 tab 共享消息列表组件。
#[component]
fn ContextPanel() -> Element {
    let mut state = use_context::<AppState>();
    let mut tab = use_signal(|| ContextTab::Preview);
    // Turn 气泡的「上下文」按钮写入 snapshot_target 时跳到快照 tab。
    use_effect(move || {
        if state.snapshot_target.read().is_some() {
            tab.set(ContextTab::Snapshot);
        }
    });
    let cur = *tab.read();
    rsx! {
        aside { class: "context-panel",
            div { class: "ctx-tabs",
                button {
                    class: if cur == ContextTab::Preview { "ctx-tab active" } else { "ctx-tab" },
                    onclick: move |_| tab.set(ContextTab::Preview),
                    "预览"
                }
                button {
                    class: if cur == ContextTab::Snapshot { "ctx-tab active" } else { "ctx-tab" },
                    onclick: move |_| tab.set(ContextTab::Snapshot),
                    "快照"
                }
                button {
                    class: "detail-close",
                    title: "关闭侧栏",
                    onclick: move |_| state.context_panel_open.set(false),
                    "×"
                }
            }
            div { class: "ctx-scroll",
                match cur {
                    ContextTab::Preview => rsx! { PreviewTab {} },
                    ContextTab::Snapshot => rsx! { SnapshotTab {} },
                }
            }
        }
    }
}

/// 预览 tab：当前 tip + 工具勾选实时装配出的下一轮请求。换游标、链增长
/// （新 commit）、改工具勾选都会触发重新拉取。
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
                match api::get_context_preview(&cid, tools.as_deref()).await {
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

/// 快照 tab：某个已提交回合一次 LlmCall 的逐字请求记录（首条 System 即
/// 当时实际生效的系统提示词）。默认显示链上最新 Turn 的最后一次调用；
/// Turn 气泡的「上下文」按钮把目标切到对应回合。
#[component]
fn SnapshotTab() -> Element {
    let mut state = use_context::<AppState>();
    let chain = state.chain.read();
    // 链上有 LlmCall 记录的 Turn（旧数据/纯工具回合可能没有）。
    let turns: Vec<&Node> = chain
        .iter()
        .filter(|n| {
            matches!(&n.kind, NodeKind::Turn { steps, .. }
                if steps.iter().any(|s| matches!(s, Step::LlmCall { .. })))
        })
        .collect();
    if turns.is_empty() {
        return rsx! {
            div { class: "ctx-empty",
                p { "链上还没有带调用记录的回合。" }
                p { "发送一条消息，回合提交后即可查看请求快照。" }
            }
        };
    }

    // 目标解析：snapshot_target 仍指向链上的回合时用它（调用序号夹取到
    // 有效范围），否则回落到最新 Turn 的最后一次调用。
    let target = state.snapshot_target.read().clone();
    let latest = *turns.last().expect("non-empty");
    let calls_of = |turn: &Node| -> Vec<(Usage, Vec<CoreMessageView>)> {
        let NodeKind::Turn { steps, .. } = &turn.kind else {
            unreachable!()
        };
        steps
            .iter()
            .filter_map(|s| match s {
                Step::LlmCall { usage, request, .. } => Some((*usage, request.clone())),
                _ => None,
            })
            .collect()
    };
    let (turn, sel) = match &target {
        Some((id, idx)) if turns.iter().any(|t| &t.id == id) => {
            let turn = turns.iter().find(|t| &t.id == id).expect("checked");
            let n = calls_of(turn).len();
            (*turn, (*idx).min(n - 1))
        }
        _ => {
            let n = calls_of(latest).len();
            (latest, n - 1)
        }
    };
    let turn_id = turn.id.clone();
    let calls = calls_of(turn);
    let (_, request) = calls[sel].clone();

    rsx! {
        div { class: "ctx-section",
            span { class: "ctx-label", "回合 #{short_id(&turn_id)}" }
        }
        div { class: "ctx-call-switch",
            for (i, (usage, _)) in calls.iter().enumerate() {
                {
                    let tid = turn_id.clone();
                    rsx! {
                        div { class: "ctx-call-item", key: "{i}",
                            button {
                                class: if i == sel { "ctx-call-btn active" } else { "ctx-call-btn" },
                                title: "查看本次调用的请求快照",
                                onclick: move |_| {
                                    state.snapshot_target.set(Some((tid.clone(), i)));
                                },
                                "调用 {i + 1}"
                            }
                            span { class: "ctx-call-usage", "{usage_label(usage)}" }
                        }
                    }
                }
            }
        }
        ContextMessageList { messages: request }
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
}
