//! rua-ui: Dioxus web frontend for the rua session-graph daemon.
//!
//! The UI is a view onto the graph plus the current cursor (a session):
//! a chat view of the cursor's chain, and a graph view of the whole DAG.
//! A new session starts as a local draft; the cursor is only created on the
//! server when the first message is sent.

mod api;
mod chat;
mod graph;
mod state;
mod topbar;
mod types;
mod ws;

use dioxus::prelude::*;

use crate::chat::ChatView;
use crate::graph::GraphView;
use crate::state::{AppState, ConnState, View, bootstrap};
use crate::topbar::TopBar;

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    let mut state = AppState {
        view: use_signal(|| View::Chat),
        graphs: use_signal(Vec::new),
        current_graph: use_signal(String::new),
        cursors: use_signal(Vec::new),
        current_cursor: use_signal(|| None),
        pending_attach: use_signal(|| None),
        metas: use_signal(Default::default),
        chain: use_signal(Vec::new),
        inflights: use_signal(Default::default),
        conn: use_signal(|| ConnState::Disconnected),
        error: use_signal(|| None),
        selected: use_signal(|| None),
        selected_body: use_signal(|| None),
        graph_positions: use_signal(Default::default),
        collapsed_spawns: use_signal(Default::default),
        draft: use_signal(String::new),
        booted: use_signal(|| false),
    };
    use_context_provider(|| state);

    use_hook(move || {
        spawn(async move {
            bootstrap(state).await;
        });
        ws::spawn_ws_loop(state);
    });

    let booted = *state.booted.read();
    let view = *state.view.read();
    let error = state.error.read().clone();

    rsx! {
        document::Stylesheet { href: asset!("/assets/style.css") }
        div { class: "app",
            TopBar {}
            if let Some(err) = error {
                div { class: "error-banner",
                    span { class: "error-text", "{err}" }
                    button {
                        class: "error-close",
                        onclick: move |_| state.error.set(None),
                        "×"
                    }
                }
            }
            if booted {
                match view {
                    View::Chat => rsx! { ChatView {} },
                    View::Graph => rsx! { GraphView {} },
                }
            } else {
                div { class: "loading", "正在连接 rua-server…" }
            }
        }
    }
}
