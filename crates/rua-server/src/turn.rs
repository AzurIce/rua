//! In-flight turn lifecycle: bridge engine `TurnEvent`s onto the WS bus,
//! then close the ticket (commit + finish + cursor advance) and release
//! the cursor.

use rua_graph::OpenTurn;
use rua_graph::node::Outcome;
use rua_graph::TurnEvent;
use rua_engine::TurnParams;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::events::ServerEvent;
use crate::state::SharedState;

/// Spawn the forwarder (engine events -> broadcast bus) and the turn task
/// (run engine -> commit_turn / abort_turn). `turn` 是 `open_turn` 发的
/// ticket：sink 的增量落盘走它的正文条目，收尾整体消费它。
pub fn spawn_turn(
    state: &SharedState,
    params: TurnParams,
    cancel: CancellationToken,
    turn: OpenTurn,
) {
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
        rt.block_on(run_and_commit(state, params, tx, cancel, turn));
    });
}

async fn run_and_commit(
    state: SharedState,
    mut params: TurnParams,
    tx: mpsc::UnboundedSender<TurnEvent>,
    cancel: CancellationToken,
    turn: OpenTurn,
) {
    let cursor_id = turn.handle.cursor_id;
    let node_id = turn.handle.node_id;
    // 统一注入增量落盘 sink：turn 的每个 step（+ 首条 Init 锚点）经数据面
    // 条目即时落盘（文件追加 + 内存更新同一临界区），崩溃不丢轮内进度。
    // 写入失败把 Err 交回 engine：以落盘失败为原因终止本轮（Failed），
    // 失败的那一行不进任何一侧账本。
    let entry = turn.entry();
    params.sink = Some(Box::new(move |line: rua_graph::TurnLine| {
        entry.append(line).map_err(|e| e.to_string())
    }));
    let result = state.engine.run_turn(params, tx, cancel).await;

    let mut graph = state.graph.lock().await;
    match result {
        Ok(node) => {
            let outcome = node.kind.outcome;
            // commit_turn：header 落账 + turn_finished + 游标推进一次到位；
            // 内部 commit 失败会自行以 Failed 闭账。
            match graph.commit_turn(turn, node) {
                Ok(committed) => {
                    let meta = graph
                        .meta(committed.raw())
                        .expect("just committed")
                        .header_value();
                    drop(graph);
                    state.broadcast(ServerEvent::NodeCommitted { meta });
                    state.broadcast(ServerEvent::CursorMoved {
                        cursor_id,
                        node: Some(committed.raw()),
                    });
                    state.broadcast(ServerEvent::TurnCommitted {
                        cursor_id,
                        node_id: committed.raw(),
                        outcome,
                    });
                }
                Err(e) => {
                    eprintln!("rua: failed to commit turn node {node_id}: {e}");
                    drop(graph);
                    state.broadcast(ServerEvent::TurnCommitted {
                        cursor_id,
                        node_id: node_id.raw(),
                        outcome: Outcome::Failed,
                    });
                }
            }
        }
        // Parameter-level engine error: no node to commit, but the cursor
        // must still be released and listeners told the turn ended.
        Err(e) => {
            eprintln!("rua: turn {node_id} aborted: {e}");
            let _ = graph.abort_turn(turn, Outcome::Failed);
            drop(graph);
            state.broadcast(ServerEvent::TurnCommitted {
                cursor_id,
                node_id: node_id.raw(),
                outcome: Outcome::Failed,
            });
        }
    }
    state.cancels.lock().await.remove(&cursor_id);
}
