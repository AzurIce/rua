//! Top bar: view switch, new-session draft entry, cursor switcher, current
//! tip, connection state. Graph management lives in the left sidebar.

use dioxus::prelude::*;

use crate::state::{AppState, ConnState, View, enter_draft, resync};
use crate::types::short_id;

#[component]
pub fn TopBar() -> Element {
    let mut state = use_context::<AppState>();
    let view = *state.view.read();
    let conn = *state.conn.read();
    let current_id = state.current_cursor.read().clone();
    let tip = state.current().and_then(|c| c.node);

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
            // ---- 图管理已移至左侧边栏（sidebar.rs）----
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
