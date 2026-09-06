//! 左缘侧栏：图列表（会话列表式）。每行可点击切换到该图，显示运行中
//! 状态（绿点 + 数量；只有当前图可能有在飞回合），hover 浮出重命名 /
//! 复制 / 删除操作。图管理从顶栏整体移到这里。

use dioxus::prelude::*;

use crate::state::{AppState, run_graph_op};

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
pub fn Sidebar() -> Element {
    let mut state = use_context::<AppState>();
    let graphs = state.graphs.read().clone();
    let current = state.current_graph.read().clone();
    // 只有活跃图加载在 daemon 里，运行中回合只可能属于它。
    let running = state.inflights.read().len();

    if !*state.sidebar_open.read() {
        return rsx! {
            div { class: "sidebar collapsed",
                button {
                    class: "sidebar-toggle",
                    title: "展开图列表",
                    onclick: move |_| state.sidebar_open.set(true),
                    "»"
                }
            }
        };
    }

    rsx! {
        aside { class: "sidebar",
            div { class: "sidebar-header",
                span { class: "sidebar-title", "图" }
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
                    class: "sidebar-toggle",
                    title: "收起侧栏",
                    onclick: move |_| state.sidebar_open.set(false),
                    "‹"
                }
            }
            div { class: "graph-list",
                for name in &graphs {
                    GraphItem {
                        key: "{name}",
                        name: name.clone(),
                        current: *name == current,
                        running: if *name == current { running } else { 0 },
                    }
                }
                if graphs.is_empty() {
                    div { class: "graph-list-empty", "（还没有图）" }
                }
            }
        }
    }
}

/// 图列表的一行。操作按钮必须 stop_propagation，避免冒泡成整行点击。
/// `name` 预先按闭包拆份克隆：rsx 的 move 闭包各自要拿走一份所有权。
#[component]
fn GraphItem(name: String, current: bool, running: usize) -> Element {
    let state = use_context::<AppState>();

    let row_title = if running > 0 {
        format!("当前图 · {running} 个回合进行中")
    } else if current {
        "当前图".to_string()
    } else {
        format!("切换到「{name}」")
    };

    // 按闭包拆份克隆：rsx 的 move 闭包各自要拿走一份所有权。
    let row_name = name.clone();
    let rename_name = name.clone();
    let dup_name = name.clone();
    let del_name = name.clone();

    rsx! {
        div {
            class: if current { "graph-item current" } else { "graph-item" },
            title: "{row_title}",
            onclick: move |_| {
                if current {
                    return;
                }
                let target = row_name.clone();
                spawn(async move {
                    run_graph_op(state, crate::api::activate_graph(&target)).await;
                });
            },
            if running > 0 {
                span { class: "graph-dot" }
                span { class: "graph-run-count", "{running}" }
            }
            span { class: "graph-name", "{name}" }
            span { class: "graph-item-ops",
                button {
                    class: "graph-op-btn",
                    title: "重命名",
                    onclick: move |e| {
                        e.stop_propagation();
                        let from = rename_name.clone();
                        if let Some(to) = prompt("重命名为：", &from)
                            && to != from
                        {
                            spawn(async move {
                                run_graph_op(state, crate::api::rename_graph(&from, &to)).await;
                            });
                        }
                    },
                    "✎"
                }
                button {
                    class: "graph-op-btn",
                    title: "复制为新图（深拷贝，不切换）",
                    onclick: move |e| {
                        e.stop_propagation();
                        let from = dup_name.clone();
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
                    title: "删除（移入 .rua/graphs/.trash/，可手工恢复）",
                    onclick: move |e| {
                        e.stop_propagation();
                        if confirm(&format!(
                            "删除图「{del_name}」？\n数据会移入回收站（.rua/graphs/.trash/），不会真删。"
                        )) {
                            let target = del_name.clone();
                            spawn(async move {
                                run_graph_op(state, crate::api::delete_graph(&target)).await;
                            });
                        }
                    },
                    "🗑"
                }
            }
        }
    }
}
