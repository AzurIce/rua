//! Chat view: the current cursor's chain as a conversation.

use dioxus::prelude::*;

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

    rsx! {
        div { class: "chat-view",
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
    }
}

#[component]
fn Bubble(node: Node) -> Element {
    match &node.kind {
        NodeKind::Input { text, actor } => {
            rsx! {
                div { class: "bubble bubble-input",
                    div { class: "bubble-header",
                        span { class: "bubble-actor", "{actor}" }
                        span { class: "bubble-id", "#{short_id(&node.id)}" }
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
        } => {
            let outcome = *outcome;
            rsx! {
                div { class: "bubble bubble-turn outcome-{outcome.label()}",
                    div { class: "bubble-header",
                        span { class: "bubble-actor", "{actor}" }
                        span { class: "bubble-id", "#{short_id(&node.id)}" }
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

#[component]
fn UsageView(usage: Usage) -> Element {
    let mut parts = vec![
        format!("↑{}", usage.input_tokens),
        format!("↓{}", usage.output_tokens),
    ];
    if usage.reasoning_tokens > 0 {
        parts.push(format!("思考{}", usage.reasoning_tokens));
    }
    if usage.cached_input_tokens > 0 {
        parts.push(format!("缓存{}", usage.cached_input_tokens));
    }
    rsx! {
        span { class: "bubble-usage", "{parts.join(\" · \")} tokens" }
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
