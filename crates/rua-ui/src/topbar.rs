//! Top bar: view switch, graph management, cursor switcher, current tip,
//! connection state.

use dioxus::prelude::*;

use crate::state::{AppState, ConnState, View, enter_draft, resync, run_graph_op};
use crate::types::short_id;

fn prompt(message: &str, default: &str) -> Option<String> {
    let window = web_sys::window()?;
    window
        .prompt_with_message_and_default(message, default)
        .ok()
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn confirm(message: &str) -> bool {
    web_sys::window()
        .and_then(|w| w.confirm_with_message(message).ok())
        .unwrap_or(false)
}

#[component]
pub fn TopBar() -> Element {
    let mut state = use_context::<AppState>();
    let view = *state.view.read();
    let conn = *state.conn.read();
    let current_id = state.current_cursor.read().clone();
    let tip = state.current().and_then(|c| c.node);
    let graphs = state.graphs.read().clone();
    let current_graph = state.current_graph.read().clone();

    rsx! {
        header { class: "topbar",
            span { class: "topbar-title", "rua" }
            nav { class: "view-switch",
                button {
                    class: if view == View::Chat { "view-btn active" } else { "view-btn" },
                    onclick: move |_| state.view.set(View::Chat),
                    "聊天"
                }
                button {
                    class: if view == View::Graph { "view-btn active" } else { "view-btn" },
                    onclick: move |_| state.view.set(View::Graph),
                    "图"
                }
            }
            // ---- 图管理（用户侧）：切换 / 新建 / 重命名 / 删除（回收站）----
            div { class: "graph-switch",
                select {
                    class: "graph-select",
                    title: "切换图",
                    value: current_graph.clone(),
                    onchange: move |e| {
                        let name = e.value();
                        if name.is_empty() || name == current_graph {
                            return;
                        }
                        spawn(async move {
                            run_graph_op(state, crate::api::activate_graph(&name)).await;
                        });
                    },
                    if graphs.is_empty() {
                        option { value: "", "…" }
                    }
                    for name in &graphs {
                        option {
                            key: "{name}",
                            value: "{name}",
                            selected: *name == current_graph,
                            "{name}"
                        }
                    }
                }
                button {
                    class: "graph-op-btn",
                    title: "新建空图并切换过去",
                    onclick: move |_| {
                        if let Some(name) = prompt("新图的名字：", "") {
                            spawn(async move {
                                run_graph_op(state, crate::api::create_graph(&name)).await;
                            });
                        }
                    },
                    "+"
                }
                button {
                    class: "graph-op-btn",
                    title: "重命名当前图",
                    onclick: move |_| {
                        let from = state.current_graph.read().clone();
                        if from.is_empty() {
                            return;
                        }
                        if let Some(to) = prompt("重命名为：", &from) {
                            if to != from {
                                spawn(async move {
                                    run_graph_op(state, crate::api::rename_graph(&from, &to)).await;
                                });
                            }
                        }
                    },
                    "✎"
                }
                button {
                    class: "graph-op-btn",
                    title: "复制当前图为一个新图（深拷贝，不切换）",
                    onclick: move |_| {
                        let from = state.current_graph.read().clone();
                        if from.is_empty() {
                            return;
                        }
                        if let Some(to) = prompt("复制为：", &format!("{from}-副本")) {
                            spawn(async move {
                                run_graph_op(state, crate::api::duplicate_graph(&from, &to)).await;
                            });
                        }
                    },
                    "⧉"
                }
                button {
                    class: "graph-op-btn danger",
                    title: "删除当前图（移入 .rua/graphs/.trash/，可手工恢复）",
                    onclick: move |_| {
                        let name = state.current_graph.read().clone();
                        if name.is_empty() {
                            return;
                        }
                        if confirm(&format!("删除图「{name}」？\n数据会移入回收站（.rua/graphs/.trash/），不会真删。")) {
                            spawn(async move {
                                run_graph_op(state, crate::api::delete_graph(&name)).await;
                            });
                        }
                    },
                    "🗑"
                }
            }
            button {
                class: "view-btn",
                title: "新对话：进入本地草稿态（不碰服务端），发送第一条消息时才真正创建会话",
                onclick: move |_| enter_draft(state, None),
                "+ 新对话"
            }
            select {
                class: "cursor-select",
                title: "切换游标（会话）",
                value: current_id.clone().unwrap_or_default(),
                onchange: move |e| {
                    let id = e.value();
                    // "" is the draft entry; re-selecting it is a no-op.
                    if id.is_empty() || Some(&id) == current_id.as_ref() {
                        return;
                    }
                    // Switching to another cursor discards the draft.
                    state.current_cursor.set(Some(id));
                    state.pending_attach.set(None);
                    state.selected.set(None);
                    spawn(async move {
                        resync(state).await;
                    });
                },
                if current_id.is_none() {
                    option { value: "", "（新会话草稿）" }
                }
                for cursor in state.cursors.read().iter() {
                    option {
                        key: "{cursor.id}",
                        value: "{cursor.id}",
                        "{cursor.actor} · #{short_id(&cursor.id)}"
                    }
                }
            }
            if let Some(tip) = tip {
                span { class: "tip-label", title: "{tip}", "tip #{short_id(&tip)}" }
            } else {
                span { class: "tip-label", "tip —" }
            }
            span { class: "conn conn-{conn_class(conn)}", {conn.label()} }
        }
    }
}

fn conn_class(conn: ConnState) -> &'static str {
    match conn {
        ConnState::Connected => "up",
        ConnState::Connecting => "mid",
        ConnState::Disconnected => "down",
    }
}
