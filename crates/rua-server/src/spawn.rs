//! Server-side `TurnSpawner`: the agent's spawn_turn / inspect tools run
//! against the live graph and runtime. `spawn_turn` deliberately walks the
//! exact atomic path of `POST /api/inputs` (create cursor + commit input +
//! start turn).

use std::time::Duration;

use rua_core::id::NodeId;
use rua_core::node::{Node, NodeKind, NodeKindTag, Step};
use rua_engine::{InspectOutcome, SpawnedTurn, TurnParams, TurnSpawner};
use tokio_util::sync::CancellationToken;

use crate::events::ServerEvent;
use crate::state::SharedState;

/// inspect 的轮询间隔（本地 daemon，简单轮询比事件订阅省事）。
const POLL_INTERVAL: Duration = Duration::from_millis(250);

pub struct ServerSpawner {
    state: SharedState,
}

impl ServerSpawner {
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }
}

impl TurnSpawner for ServerSpawner {
    fn spawn_turn(
        &self,
        parent: Option<NodeId>,
        text: String,
        actor: String,
        created_by: NodeId,
        depth: usize,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SpawnedTurn, String>> + Send>,
    > {
        let state = self.state.clone();
        Box::pin(async move {
            let mut graph = state.graph.lock().await;
            if let Some(parent) = parent {
                match graph.meta(parent) {
                    None => return Err(format!("node not found: {parent}")),
                    Some(meta) if meta.kind != NodeKindTag::Turn => {
                        return Err("pointer must be a committed turn node".to_string());
                    }
                    _ => {}
                }
            }

            let cursor = graph.create_cursor(actor.clone(), vec![]);
            state.broadcast(ServerEvent::CursorCreated {
                cursor: cursor.clone(),
            });
            let input = Node {
                id: NodeId::new(),
                parent,
                context_refs: vec![],
                // 溯源：这个 input 由 created_by（发起 spawn 的 turn）产生。
                created_by: Some(created_by),
                created_at: Node::now_millis(),
                kind: NodeKind::Input {
                    text,
                    actor: actor.clone(),
                },
            };
            let input_id = input.id;
            graph.commit_node(input).map_err(|e| e.to_string())?;
            let input_meta = graph.meta(input_id).expect("just committed").clone();
            state.broadcast(ServerEvent::NodeCommitted { meta: input_meta });
            graph.move_cursor(cursor.id, input_id).map_err(|e| e.to_string())?;
            state.broadcast(ServerEvent::CursorMoved {
                cursor_id: cursor.id,
                node: Some(input_id),
            });
            let handle = graph.begin_turn(cursor.id).map_err(|e| e.to_string())?;
            let history = graph.assemble_chain(input_id).map_err(|e| e.to_string())?;
            let cursor_id = cursor.id;
            drop(graph);

            let cancel = CancellationToken::new();
            state.cancels.lock().await.insert(cursor_id, cancel.clone());
            crate::turn::spawn_turn(
                &state,
                TurnParams {
                    cursor_id,
                    node_id: handle.node_id,
                    parent: Some(input_id),
                    context_refs: vec![],
                    actor,
                    model: state.model.clone(),
                    history,
                    system_prompt: Some(state.system_prompt.clone()),
                    depth,
                },
                cancel,
            );

            Ok(SpawnedTurn {
                cursor_id: cursor_id.to_string(),
                input_node_id: input_id.to_string(),
                turn_node_id: handle.node_id.to_string(),
            })
        })
    }

    fn inspect(
        &self,
        node: NodeId,
        wait: Option<Duration>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<InspectOutcome, String>> + Send>,
    > {
        let state = self.state.clone();
        Box::pin(async move {
            let deadline = wait.map(|w| tokio::time::Instant::now() + w);
            loop {
                {
                    let mut graph = state.graph.lock().await;
                    if let Ok(node) = graph.node(node) {
                        return Ok(project(node));
                    }
                }
                if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
                    return Ok(InspectOutcome::Running);
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        })
    }
}

/// Project a committed node to the inspect payload. Turn = outcome + final
/// response text + usage; Input/Context are returned as their text verbatim
/// (the agent may inspect any pointer it holds).
fn project(node: &Node) -> InspectOutcome {
    match &node.kind {
        NodeKind::Turn {
            steps,
            outcome,
            usage,
            ..
        } => {
            let text = steps
                .iter()
                .rev()
                .find_map(|s| match s {
                    Step::LlmCall {
                        response_text, ..
                    } if !response_text.is_empty() => Some(response_text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            InspectOutcome::Committed {
                outcome: Some(format!("{outcome:?}").to_lowercase()),
                text,
                usage: Some(*usage),
            }
        }
        NodeKind::Input { text, .. } => InspectOutcome::Committed {
            outcome: None,
            text: text.clone(),
            usage: None,
        },
        NodeKind::Context { body, .. } => InspectOutcome::Committed {
            outcome: None,
            text: body.clone(),
            usage: None,
        },
    }
}
