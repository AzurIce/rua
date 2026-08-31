//! In-flight turn lifecycle: bridge engine `TurnEvent`s onto the WS bus,
//! then commit the returned turn node and release the cursor.

use rua_core::node::{NodeKind, Outcome};
use rua_core::TurnEvent;
use rua_engine::TurnParams;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::events::ServerEvent;
use crate::state::SharedState;

/// Spawn the forwarder (engine events -> broadcast bus) and the turn task
/// (run engine -> commit node -> finish turn -> advance cursor).
pub fn spawn_turn(state: &SharedState, params: TurnParams, cancel: CancellationToken) {
    let (tx, mut rx) = mpsc::unbounded_channel::<TurnEvent>();
    let bus = state.events.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            // Lagging subscribers skip; no subscribers is fine.
            let _ = bus.send(ServerEvent::from(event).to_json());
        }
    });

    // The engine future is `!Send` (it holds `&dyn Fn` across awaits), so the
    // turn cannot be `tokio::spawn`ed on the multi-thread runtime. Drive it
    // on a dedicated thread with a current-thread runtime instead.
    let state = state.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("turn runtime");
        rt.block_on(run_and_commit(state, params, tx, cancel));
    });
}

async fn run_and_commit(
    state: SharedState,
    params: TurnParams,
    tx: mpsc::UnboundedSender<TurnEvent>,
    cancel: CancellationToken,
) {
    let cursor_id = params.cursor_id;
    let node_id = params.node_id;
    let result = state.engine.run_turn(params, tx, cancel).await;

    let mut graph = state.graph.lock().await;
    match result {
        Ok(node) => {
            let outcome = match &node.kind {
                NodeKind::Turn { outcome, .. } => *outcome,
                _ => Outcome::Failed,
            };
            match graph.commit_node(node) {
                Ok(()) => {
                    let meta = graph.meta(node_id).expect("just committed").clone();
                    let _ = graph.finish_turn(cursor_id, outcome);
                    let _ = graph.move_cursor(cursor_id, node_id);
                    drop(graph);
                    state.broadcast(ServerEvent::NodeCommitted { meta });
                    state.broadcast(ServerEvent::CursorMoved {
                        cursor_id,
                        node: Some(node_id),
                    });
                    state.broadcast(ServerEvent::TurnCommitted {
                        cursor_id,
                        node_id,
                        outcome,
                    });
                }
                Err(e) => {
                    eprintln!("rua: failed to commit turn node {node_id}: {e}");
                    let _ = graph.finish_turn(cursor_id, Outcome::Failed);
                    drop(graph);
                    state.broadcast(ServerEvent::TurnCommitted {
                        cursor_id,
                        node_id,
                        outcome: Outcome::Failed,
                    });
                }
            }
        }
        // Parameter-level engine error: no node to commit, but the cursor
        // must still be released and listeners told the turn ended.
        Err(e) => {
            eprintln!("rua: turn {node_id} aborted: {e}");
            let _ = graph.finish_turn(cursor_id, Outcome::Failed);
            drop(graph);
            state.broadcast(ServerEvent::TurnCommitted {
                cursor_id,
                node_id,
                outcome: Outcome::Failed,
            });
        }
    }
    state.cancels.lock().await.remove(&cursor_id);
}
