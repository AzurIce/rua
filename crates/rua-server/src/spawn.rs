//! Server-side turn spawning: `graph.spawn`（script 绑定面）在此对接活图与
//! 运行时。`spawn_turn` deliberately walks the exact atomic path of
//! `POST /api/inputs` (create cursor + commit input + start turn).
//!
//! 等待/读取一侧在 [`crate::script`]（wait 绑定轮询图索引）。

use rua_graph::Ulid;
use rua_graph::id::NodeId;
use rua_graph::node::{Input, Turn};
use rua_engine::TurnParams;
use tokio_util::sync::CancellationToken;

use crate::events::ServerEvent;
use crate::state::SharedState;

/// Result of a successful `graph.spawn` call.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SpawnedTurn {
    pub cursor_id: String,
    pub input_node_id: String,
    pub turn_node_id: String,
}

pub struct ServerSpawner {
    state: SharedState,
}

impl ServerSpawner {
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }

    pub fn spawn_turn(
        &self,
        parent: Option<Ulid>,
        text: String,
        actor: String,
        created_by: NodeId<Turn>,
        depth: usize,
        tools: Vec<String>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SpawnedTurn, String>> + Send>,
    > {
        let state = self.state.clone();
        Box::pin(async move {
            let mut graph = state.graph.lock().await;
            // pointer 必须落在已提交的 turn 节点上（受检类型恢复）。
            let parent = parent
                .map(|p| graph.expect_turn(p).map_err(|e| e.to_string()))
                .transpose()?;

            let cursor = graph.create_cursor(actor.clone(), vec![]);
            state.broadcast(ServerEvent::CursorCreated {
                cursor: cursor.clone(),
            });
            let input = Input::node(
                NodeId::new(),
                parent,
                text,
                actor.clone(),
                tools.clone(),
                // 溯源：这个 input 由 created_by（发起 spawn 的 turn）产生。
                Some(created_by),
            );
            let input_id = input.id;
            graph.commit(input).map_err(|e| e.to_string())?;
            let input_meta = graph
                .meta(input_id.raw())
                .expect("just committed")
                .header_value();
            state.broadcast(ServerEvent::NodeCommitted { meta: input_meta });
            let turn = {
                let mut cur = graph.cursor_mut(cursor.id).map_err(|e| e.to_string())?;
                cur.move_to(input_id.raw()).map_err(|e| e.to_string())?;
                state.broadcast(ServerEvent::CursorMoved {
                    cursor_id: cursor.id,
                    node: Some(input_id.raw()),
                });
                cur.open_turn().map_err(|e| e.to_string())?
            };
            let turn_node_id = turn.handle.node_id;
            let (chain, materials) = graph.load_chain(input_id.raw()).map_err(|e| e.to_string())?;
            let history =
                rua_engine::assemble(&chain, &materials, graph.data()).map_err(|e| e.to_string())?;
            let cursor_id = cursor.id;
            drop(graph);

            let cancel = CancellationToken::new();
            state.cancels.lock().await.insert(cursor_id, cancel.clone());
            crate::turn::spawn_turn(
                &state,
                TurnParams {
                    cursor_id,
                    node_id: turn_node_id,
                    parent: input_id,
                    actor,
                    model: state.default_model.clone(),
                    history,
                    system_prompt: None,
                    depth,
                    // 继承父 turn 的有效工具集（engine 已校验/展开的显式列表）。
                    tools: Some(tools),
                    // sink 由 spawn_turn 统一注入（见 turn.rs）。
                    sink: None,
                },
                cancel,
                turn,
            );

            Ok(SpawnedTurn {
                cursor_id: cursor_id.to_string(),
                input_node_id: input_id.to_string(),
                turn_node_id: turn_node_id.to_string(),
            })
        })
    }
}
