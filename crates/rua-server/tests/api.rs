//! Integration test: real axum server on a random port + reqwest client,
//! with a mock engine (no API key needed). Covers cursor lifecycle, the
//! 409 busy contract, cancel, chain, move (incl. 400 on context nodes),
//! summarize, the graph index, and the WS event stream.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use rua_core::id::{CursorId, NodeId};
use rua_core::node::{Node, NodeKind, Outcome, Usage};
use rua_core::graph::Graph;
use rua_core::TurnEvent;
use rua_engine::TurnParams;
use rua_server::engine::AgentEngine;
use rua_server::state::AppState;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Mock engine: emits Started + a TextDelta, then parks until released or
/// cancelled, and commits a turn node with the matching outcome.
struct MockEngine {
    release: Arc<Notify>,
    /// true = park until released/cancelled (existing 409 tests);
    /// false = commit immediately (spawn tests).
    park: bool,
}

impl AgentEngine for MockEngine {
    fn run_turn<'a>(
        &'a self,
        params: TurnParams,
        events: UnboundedSender<TurnEvent>,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = rua_engine::Result<Node>> + 'a>> {
        Box::pin(async move {
            let _ = events.send(TurnEvent::Started {
                cursor_id: params.cursor_id,
                node_id: params.node_id,
            });
            let _ = events.send(TurnEvent::TextDelta {
                cursor_id: params.cursor_id,
                node_id: params.node_id,
                delta: "working".into(),
            });
            if self.park {
                tokio::select! {
                    _ = self.release.notified() => {}
                    _ = cancel.cancelled() => {}
                }
            }
            let outcome = if cancel.is_cancelled() {
                Outcome::Cancelled
            } else {
                Outcome::Completed
            };
            Ok(Node {
                id: params.node_id,
                parent: params.parent,
                context_refs: params.context_refs,
                created_by: None,
                created_at: Node::now_millis(),
                kind: NodeKind::Turn {
                    steps: vec![],
                    outcome,
                    actor: params.actor,
                    model: params.model,
                    usage: Usage::default(),
                },
            })
        })
    }

    fn summarize<'a>(
        &'a self,
        _material: &'a str,
    ) -> Pin<Box<dyn Future<Output = rua_engine::Result<String>> + Send + 'a>> {
        Box::pin(async { Ok("distilled summary".to_string()) })
    }
}

struct TestServer {
    base: String,
    release: Arc<Notify>,
    client: reqwest::Client,
    _dir: tempfile::TempDir,
}

async fn spawn_server() -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    // 与 main.rs 同布局：活跃图在 graphs/default。
    let graphs_root = dir.path().join("graphs");
    let graph = Graph::open(graphs_root.join("default")).unwrap();
    let release = Arc::new(Notify::new());
    let engine = Arc::new(MockEngine {
        release: release.clone(),
        park: true,
    });
    let state = Arc::new(AppState::new(graph, engine, "mock-model".into(), graphs_root, "default".into()));
    let app = rua_server::build_router(state, None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    TestServer {
        base: format!("http://{addr}"),
        release,
        client: reqwest::Client::new(),
        _dir: dir,
    }
}

impl TestServer {
    async fn create_cursor(&self) -> String {
        let resp = self
            .client
            .post(format!("{}/api/cursors", self.base))
            .json(&json!({"actor": "human", "capabilities": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let cursor: Value = resp.json().await.unwrap();
        assert_eq!(cursor["actor"], "human");
        cursor["id"].as_str().unwrap().to_string()
    }

    async fn graph(&self) -> Value {
        let resp = self
            .client
            .get(format!("{}/api/graph", self.base))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        resp.json().await.unwrap()
    }

    /// Poll until the graph has `n` nodes (turn commit is async).
    async fn wait_for_nodes(&self, n: usize) -> Value {
        for _ in 0..100 {
            let graph = self.graph().await;
            if graph["nodes"].as_array().unwrap().len() == n {
                return graph;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("graph never reached {n} nodes");
    }
}

#[tokio::test]
async fn cursor_input_chain_move_graph() {
    let server = spawn_server().await;
    let cursor_id = server.create_cursor().await;

    // Chain of a fresh cursor is empty.
    let resp = server
        .client
        .get(format!("{}/api/cursors/{cursor_id}/chain", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap(), json!([]));

    // Submit input: input node committed immediately, turn node pre-allocated.
    let resp = server
        .client
        .post(format!("{}/api/cursors/{cursor_id}/input", server.base))
        .json(&json!({"text": "hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let input_id = body["input_node"]["id"].as_str().unwrap().to_string();
    let turn_id = body["turn_node_id"].as_str().unwrap().to_string();
    assert_eq!(body["input_node"]["kind"], "input");

    // Cursor busy while the mock turn is parked: 409.
    let resp = server
        .client
        .post(format!("{}/api/cursors/{cursor_id}/input", server.base))
        .json(&json!({"text": "again"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    assert!(resp.json::<Value>().await.unwrap()["error"].is_string());

    // A running turn owns its cursor: move/detach while busy -> 409.
    let resp = server
        .client
        .post(format!("{}/api/cursors/{cursor_id}/move", server.base))
        .json(&json!({"node_id": input_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let resp = server
        .client
        .post(format!("{}/api/cursors/{cursor_id}/detach", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    // Cancel the in-flight turn: 204, then the cursor is idle -> 409.
    let resp = server
        .client
        .post(format!("{}/api/cursors/{cursor_id}/cancel", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // The cancelled turn still commits (input + turn = 2 nodes).
    let graph = server.wait_for_nodes(2).await;
    assert_eq!(graph["cursors"].as_array().unwrap().len(), 1);
    assert_eq!(graph["cursors"][0]["node"], json!(turn_id));
    assert_eq!(graph["interrupted"], json!([]));

    let resp = server
        .client
        .post(format!("{}/api/cursors/{cursor_id}/cancel", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    // Chain is root -> tip: input then the cancelled turn.
    let resp = server
        .client
        .get(format!("{}/api/cursors/{cursor_id}/chain", server.base))
        .send()
        .await
        .unwrap();
    let chain: Value = resp.json().await.unwrap();
    let chain = chain.as_array().unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0]["id"], json!(input_id));
    assert_eq!(chain[0]["kind"]["type"], "input");
    assert_eq!(chain[0]["kind"]["text"], "hello");
    assert_eq!(chain[1]["id"], json!(turn_id));
    assert_eq!(chain[1]["kind"]["outcome"], "cancelled");

    // Node body endpoint; bad id -> 400, unknown id -> 404.
    let resp = server
        .client
        .get(format!("{}/api/nodes/{input_id}", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = server
        .client
        .get(format!("{}/api/nodes/not-a-ulid", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = server
        .client
        .get(format!("{}/api/nodes/{}", server.base, NodeId::new()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Input nodes are not attach targets: 400.
    let resp = server
        .client
        .post(format!("{}/api/cursors/{cursor_id}/move", server.base))
        .json(&json!({"node_id": input_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Move back to the turn node (fork/rewind): ok.
    let resp = server
        .client
        .post(format!("{}/api/cursors/{cursor_id}/move", server.base))
        .json(&json!({"node_id": turn_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["node"], json!(turn_id));

    // Detach: cursor leaves the graph; next input would start a new root.
    let resp = server
        .client
        .post(format!("{}/api/cursors/{cursor_id}/detach", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["node"], json!(null));
    let resp = server
        .client
        .get(format!("{}/api/cursors/{cursor_id}/chain", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.json::<Value>().await.unwrap(), json!([]));
    // Re-attach to the turn for the rest of the test.
    let resp = server
        .client
        .post(format!("{}/api/cursors/{cursor_id}/move", server.base))
        .json(&json!({"node_id": turn_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Summarize the input into a context node.
    let resp = server
        .client
        .post(format!("{}/api/summarize", server.base))
        .json(&json!({"sources": [input_id]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let meta: Value = resp.json().await.unwrap();
    assert_eq!(meta["kind"], "context");
    assert_eq!(meta["context_refs"], json!([input_id]));
    let ctx_id = meta["id"].as_str().unwrap().to_string();

    // A cursor may not land on a context node: 400.
    let resp = server
        .client
        .post(format!("{}/api/cursors/{cursor_id}/move", server.base))
        .json(&json!({"node_id": ctx_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Unknown cursor -> 404.
    let resp = server
        .client
        .post(format!(
            "{}/api/cursors/{}/input",
            server.base,
            CursorId::new()
        ))
        .json(&json!({"text": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Final graph index: input + turn + context.
    let graph = server.graph().await;
    assert_eq!(graph["nodes"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn ws_streams_turn_events() {
    let server = spawn_server().await;
    let ws_url = format!("{}/api/ws", server.base.replacen("http", "ws", 1));
    let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();

    let cursor_id = server.create_cursor().await;
    let resp = server
        .client
        .post(format!("{}/api/cursors/{cursor_id}/input", server.base))
        .json(&json!({"text": "hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // Let the mock turn complete.
    server.release.notify_one();

    // Collect events until the turn commits.
    let mut events = Vec::new();
    let collect = async {
        while let Some(msg) = ws.next().await {
            let msg = msg.unwrap();
            if !msg.is_text() {
                continue;
            }
            let event: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            let name = event["event"].as_str().unwrap().to_string();
            if name == "text_delta" {
                assert_eq!(event["delta"], "working");
            }
            let done = name == "turn_committed";
            events.push(name);
            if done {
                break;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), collect)
        .await
        .expect("timed out waiting for turn_committed");

    for expected in [
        "cursor_created",
        "node_committed",
        "cursor_moved",
        "turn_started",
        "text_delta",
        "turn_committed",
    ] {
        assert!(
            events.iter().any(|e| e == expected),
            "missing {expected} in {events:?}"
        );
    }

    // The completed turn committed its node.
    server.wait_for_nodes(2).await;
}

#[tokio::test]
async fn root_input_creates_session_lazily() {
    let server = spawn_server().await;

    // Root input: creates cursor + input + starts a turn atomically.
    let resp = server
        .client
        .post(format!("{}/api/inputs", server.base))
        .json(&json!({"text": "hello root"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let cursor_id = body["cursor"]["id"].as_str().unwrap().to_string();
    let input_id = body["input_node"]["id"].as_str().unwrap().to_string();
    assert_eq!(body["cursor"]["actor"], "human");
    assert!(body["input_node"]["parent"].is_null());
    let graph = server.graph().await;
    assert_eq!(graph["in_flight"].as_array().unwrap().len(), 1);

    // Let the mock turn finish; cursor lands on the committed turn.
    server.release.notify_waiters();
    let graph = server.wait_for_nodes(2).await;
    let turn_id = graph["nodes"].as_array().unwrap()[1]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(graph["cursors"][0]["id"], json!(cursor_id));
    assert_eq!(graph["cursors"][0]["node"], json!(turn_id));

    // Attach a new lazy session to the turn node: ok.
    let resp = server
        .client
        .post(format!("{}/api/inputs", server.base))
        .json(&json!({"text": "fork here", "parent": turn_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["input_node"]["parent"], json!(turn_id));
    server.release.notify_waiters();

    // Parent must be a turn node: input -> 400, unknown -> 404.
    let resp = server
        .client
        .post(format!("{}/api/inputs", server.base))
        .json(&json!({"text": "x", "parent": input_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = server
        .client
        .post(format!("{}/api/inputs", server.base))
        .json(&json!({"text": "x", "parent": NodeId::new().to_string()}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Summarize the turn into a context node; it is not a valid parent either.
    let resp = server
        .client
        .post(format!("{}/api/summarize", server.base))
        .json(&json!({"sources": [turn_id]}))
        .send()
        .await
        .unwrap();
    let ctx_id = resp.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = server
        .client
        .post(format!("{}/api/inputs", server.base))
        .json(&json!({"text": "x", "parent": ctx_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn server_spawner_creates_session_and_inspect_reads_it() {
    use rua_engine::TurnSpawner;
    use rua_server::spawn::ServerSpawner;

    let dir = tempfile::tempdir().unwrap();
    let graphs_root = dir.path().join("graphs");
    let graph = Graph::open(graphs_root.join("default")).unwrap();
    let engine = Arc::new(MockEngine {
        release: Arc::new(Notify::new()),
        park: false,
    });
    let state = Arc::new(AppState::new(graph, engine, "mock-model".into(), graphs_root, "default".into()));
    let spawner = ServerSpawner::new(state.clone());

    // 从零 spawn：立即返回，turn 在后台跑（mock engine 直接完成）。
    let creator = NodeId::new();
    let spawned = spawner
        .spawn_turn(None, "child task".to_string(), "agent:test".to_string(), creator, 1)
        .await
        .unwrap();
    let input_id: NodeId = spawned.input_node_id.parse().unwrap();
    let turn_id: NodeId = spawned.turn_node_id.parse().unwrap();

    // 溯源盖章：spawn 出的根 input 的 created_by 指向发起它的 turn。
    {
        let graph = state.graph.lock().await;
        assert_eq!(graph.meta(input_id).unwrap().created_by, Some(creator));
    }

    // Input 节点已同步 commit，inspect 立即可读。
    let input = spawner.inspect(input_id, None).await.unwrap();
    match input {
        rua_engine::InspectOutcome::Committed { text, outcome, .. } => {
            assert_eq!(text, "child task");
            assert_eq!(outcome, None);
        }
        other => panic!("expected committed input, got {other:?}"),
    }

    // Turn 节点：inspect 阻塞到 commit（mock engine 立即完成）。
    let turn = spawner
        .inspect(turn_id, Some(Duration::from_secs(10)))
        .await
        .unwrap();
    match turn {
        rua_engine::InspectOutcome::Committed { outcome, usage, .. } => {
            assert_eq!(outcome.as_deref(), Some("completed"));
            assert!(usage.is_some());
        }
        other => panic!("expected committed turn, got {other:?}"),
    }

    // 未知节点 + 短等待 → running。
    let running = spawner
        .inspect(NodeId::new(), Some(Duration::from_millis(200)))
        .await
        .unwrap();
    assert!(matches!(running, rua_engine::InspectOutcome::Running));

    // pointer 必须落在已 commit 的 turn 节点上：input 节点非法。
    let err = spawner
        .spawn_turn(Some(input_id), "x".to_string(), "agent:test".to_string(), NodeId::new(), 1)
        .await
        .unwrap_err();
    assert!(err.contains("turn node"), "got: {err}");

    // 以刚完成的 turn 为 parent spawn（fork），能成。
    let forked = spawner
        .spawn_turn(Some(turn_id), "grandchild".to_string(), "agent:test".to_string(), NodeId::new(), 2)
        .await
        .unwrap();
    let forked_turn: NodeId = forked.turn_node_id.parse().unwrap();
    let result = spawner
        .inspect(forked_turn, Some(Duration::from_secs(10)))
        .await
        .unwrap();
    assert!(matches!(result, rua_engine::InspectOutcome::Committed { .. }));
}

#[tokio::test]
async fn graph_management_lifecycle() {
    let server = spawn_server().await;

    async fn list_graphs(server: &TestServer) -> Value {
        let resp = server
            .client
            .get(format!("{}/api/graphs", server.base))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        resp.json::<Value>().await.unwrap()
    }

    // 初始：只有 default 且为当前图。
    let g = list_graphs(&server).await;
    assert_eq!(g["graphs"], json!(["default"]));
    assert_eq!(g["current"], "default");

    // 新建 = 创建空图并切换过去。
    let resp = server
        .client
        .post(format!("{}/api/graphs", server.base))
        .json(&json!({"name": "工作"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let g = list_graphs(&server).await;
    assert_eq!(g["graphs"], json!(["default", "工作"]));
    assert_eq!(g["current"], "工作");

    // 非法名字：400。
    let resp = server
        .client
        .post(format!("{}/api/graphs", server.base))
        .json(&json!({"name": "../evil"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // 在当前图里挂一个在飞 turn：管理操作全部 409。
    let cursor_id = server.create_cursor().await;
    server
        .client
        .post(format!("{}/api/cursors/{cursor_id}/input", server.base))
        .json(&json!({"text": "park me"}))
        .send()
        .await
        .unwrap();
    for (method, url, body) in [
        ("POST", format!("{}/api/graphs/default/activate", server.base), None),
        (
            "POST",
            format!("{}/api/graphs/工作/rename", server.base),
            Some(json!({"name": "w2"})),
        ),
        ("DELETE", format!("{}/api/graphs/工作", server.base), None),
    ] {
        let req = match method {
            "POST" => server.client.post(url),
            _ => server.client.delete(url),
        };
        let req = match body {
            Some(b) => req.json(&b),
            None => req,
        };
        assert_eq!(req.send().await.unwrap().status(), 409, "{method} busy guard");
    }

    // 放行 turn，等 commit。
    server.release.notify_one();
    server.wait_for_nodes(2).await;

    // rename 当前图。
    let resp = server
        .client
        .post(format!("{}/api/graphs/工作/rename", server.base))
        .json(&json!({"name": "w2"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let g = list_graphs(&server).await;
    assert_eq!(g["graphs"], json!(["default", "w2"]));
    assert_eq!(g["current"], "w2");
    // 重命名后数据还在（图里有 2 个节点）。
    assert_eq!(server.graph().await["nodes"].as_array().unwrap().len(), 2);

    // 切回 default（空图）。
    let resp = server
        .client
        .post(format!("{}/api/graphs/default/activate", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert_eq!(server.graph().await["nodes"].as_array().unwrap().len(), 0);

    // 删除非当前图 w2：进回收站而不是真删。
    let resp = server
        .client
        .delete(format!("{}/api/graphs/w2", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let g = list_graphs(&server).await;
    assert_eq!(g["graphs"], json!(["default"]));
    let trash = server._dir.path().join("graphs/.trash");
    let trashed: Vec<_> = std::fs::read_dir(&trash).unwrap().map(|e| e.unwrap()).collect();
    assert_eq!(trashed.len(), 1);
    assert!(trashed[0].file_name().to_string_lossy().starts_with("w2-"));
    // 回收站里的数据完整可读。
    assert!(trashed[0].path().join("journal.jsonl").exists());

    // 删除最后一个图（当前的 default）：自动重建空 default 并切过去。
    let resp = server
        .client
        .delete(format!("{}/api/graphs/default", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let g = list_graphs(&server).await;
    assert_eq!(g["graphs"], json!(["default"]));
    assert_eq!(g["current"], "default");
    assert_eq!(server.graph().await["nodes"].as_array().unwrap().len(), 0);
}
