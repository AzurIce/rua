//! Integration tests for `run_turn` against a mock OpenAI-compatible
//! (DeepSeek-shaped) SSE endpoint.

use std::sync::{Arc, Mutex};

use rua_engine::config::ProviderConfig;
use rua_graph::events::TurnEvent;
use rua_graph::id::{CursorId, NodeId};
use rua_graph::message::CoreMessage;
use rua_graph::node::{NodeKind, Outcome, Step, TurnLine};
use rua_engine::{Engine, TurnParams};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(frames: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for frame in frames {
        body.push_str("data: ");
        body.push_str(&frame.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn text_chunk(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "cmpl-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "deepseek-v4-pro",
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": text},
            "finish_reason": null
        }]
    })
}

fn reasoning_chunk(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "cmpl-1",
        "choices": [{
            "index": 0,
            "delta": {"reasoning_content": text},
            "finish_reason": null
        }]
    })
}

fn tool_call_chunk(id: &str, name: &str, arguments: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "cmpl-1",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments}
                }]
            },
            "finish_reason": null
        }]
    })
}

fn final_chunk(finish_reason: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "cmpl-1",
        "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "prompt_cache_hit_tokens": 4,
            "prompt_cache_miss_tokens": 6,
            "total_tokens": 15
        }
    })
}

fn sse_response(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn engine_for(server: &MockServer) -> Engine {
    let config = ProviderConfig {
        kind: "deepseek".to_string(),
        api_key: "test-key".to_string(),
        base_url: server.uri(),
        model: "deepseek-v4-pro".to_string(),
        additional_params: serde_json::Map::new(),
    };
    Engine::new(
        &[(rua_engine::config::DEFAULT_PROVIDER.to_string(), config)],
        std::env::current_dir().unwrap(),
    )
    .unwrap()
}

fn params(history: Vec<CoreMessage>) -> TurnParams {
    TurnParams {
        cursor_id: CursorId::new(),
        node_id: NodeId::new(),
        parent: None,
        context_refs: vec![],
        actor: "human".to_string(),
        model: "deepseek-v4-pro".to_string(),
        history,
        system_prompt: Some("You are helpful.".to_string()),
        depth: 0,
        tools: None,
        sink: None,
    }
}

/// 收集型 sink：把 run_turn 期间发出的所有 TurnLine 收进共享 vec。
fn collecting_sink() -> (Arc<Mutex<Vec<TurnLine>>>, Box<dyn FnMut(TurnLine) + Send>) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let into = lines.clone();
    (
        lines,
        Box::new(move |line: TurnLine| into.lock().unwrap().push(line)),
    )
}

/// 从 sink 行流重放每次 LLM 调用的 request：Init 锚点起步，LlmCall 输出
/// 当前累积后推入 Assistant 消息，ToolExec 推入 ToolResult（与 server 的
/// wire 视图折叠同一套语义）。
fn replay_requests(lines: &[TurnLine]) -> Vec<Vec<CoreMessage>> {
    let mut acc = Vec::new();
    let mut out = Vec::new();
    for line in lines {
        match line {
            TurnLine::Init { request } => acc = request.clone(),
            TurnLine::LlmCall {
                response_text,
                tool_calls,
                ..
            } => {
                out.push(acc.clone());
                acc.push(CoreMessage::Assistant {
                    content: response_text.clone(),
                    tool_calls: tool_calls.clone(),
                });
            }
            TurnLine::ToolExec {
                call_id,
                name,
                output,
                ..
            } => acc.push(CoreMessage::ToolResult {
                call_id: call_id.clone(),
                name: name.clone(),
                output: output.clone(),
            }),
        }
    }
    out
}

#[tokio::test]
async fn model_override_reaches_the_request() {
    // 覆盖的模型名必须真的出现在请求体里（曾经只记录进节点、请求仍走
    // 默认模型的 bug 的回归测试）。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("\"model\":\"other-model\""))
        .respond_with(sse_response(sse(&[
            text_chunk("ok"),
            final_chunk("stop"),
        ])))
        .mount(&server)
        .await;

    let engine = engine_for(&server);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut p = params(vec![CoreMessage::User {
        content: "hi".to_string(),
    }]);
    p.model = "other-model".to_string();
    let node = engine.run_turn(p, tx, CancellationToken::new()).await.unwrap();
    let NodeKind::Turn { outcome, model, .. } = &node.kind else {
        panic!("expected turn node");
    };
    assert_eq!(*outcome, Outcome::Completed);
    assert_eq!(model, "other-model");
}

#[tokio::test]
async fn plain_text_turn_completes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[
            reasoning_chunk("let me think"),
            text_chunk("Hello, "),
            text_chunk("world!"),
            final_chunk("stop"),
        ])))
        .mount(&server)
        .await;

    let engine = engine_for(&server);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (lines, sink) = collecting_sink();
    let mut p = params(vec![CoreMessage::User {
        content: "hi".to_string(),
    }]);
    p.sink = Some(sink);
    let node = engine.run_turn(p, tx, CancellationToken::new()).await.unwrap();

    let NodeKind::Turn {
        steps,
        outcome,
        actor,
        model,
        usage,
        tools,
    } = &node.kind
    else {
        panic!("expected turn node");
    };
    assert_eq!(*outcome, Outcome::Completed);
    assert_eq!(actor, "human");
    assert_eq!(model, "deepseek-v4-pro");
    // 无 spawner：有效工具集只有 bash，记录在 Turn 节点上。
    assert_eq!(tools, &vec!["bash".to_string()]);
    assert_eq!(steps.len(), 1);
    let Step::LlmCall {
        response_text,
        tool_calls,
        reasoning,
        usage: step_usage,
        ..
    } = &steps[0]
    else {
        panic!("expected llm call step");
    };
    assert_eq!(response_text, "Hello, world!");
    assert!(tool_calls.is_empty());
    assert_eq!(reasoning.as_deref(), Some("let me think"));
    assert_eq!(step_usage.input_tokens, 10);
    assert_eq!(step_usage.output_tokens, 5);
    assert_eq!(step_usage.cached_input_tokens, 4);
    assert_eq!(usage.input_tokens, 10);

    // Sink: Init 锚点（系统提示 + 初始 user 消息）+ 一条 LlmCall 行。
    let lines = lines.lock().unwrap();
    assert_eq!(lines.len(), 2);
    let TurnLine::Init { request } = &lines[0] else {
        panic!("expected init anchor");
    };
    assert_eq!(request.len(), 2);
    assert!(matches!(&request[0], CoreMessage::System { .. }));
    assert!(matches!(&lines[1], TurnLine::LlmCall { response_text, .. } if response_text == "Hello, world!"));
    drop(lines);

    // Events: Started, reasoning delta, two text deltas.
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert!(matches!(events[0], TurnEvent::Started { .. }));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TurnEvent::ReasoningDelta { delta, .. } if delta == "let me think"))
    );
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello, world!");
    // Engine never emits Committed (the server does).
    assert!(!events.iter().any(|e| matches!(e, TurnEvent::Committed { .. })));
}

#[tokio::test]
async fn tool_call_turn_executes_bash_and_feeds_back_result() {
    let server = MockServer::start().await;
    // Mounted first: matches the *second* request (the one carrying the
    // tool result). wiremock tries newer mocks first, so this only wins
    // once the request body contains a `tool` role message.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(sse_response(sse(&[
            text_chunk("done with tools"),
            final_chunk("stop"),
        ])))
        .mount(&server)
        .await;
    // Mounted second (newer): matches any POST, loses to the above on the
    // tool-result request.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[
            tool_call_chunk("call_abc", "bash", r#"{"command":"echo rua-tool-ok"}"#),
            final_chunk("tool_calls"),
        ])))
        .mount(&server)
        .await;

    let engine = engine_for(&server);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (lines, sink) = collecting_sink();
    let mut p = params(vec![CoreMessage::User {
        content: "run something".to_string(),
    }]);
    p.sink = Some(sink);
    let node = engine
        .run_turn(p, tx, CancellationToken::new())
        .await
        .unwrap();

    let NodeKind::Turn { steps, outcome, .. } = &node.kind else {
        panic!("expected turn node");
    };
    assert_eq!(*outcome, Outcome::Completed);
    assert_eq!(steps.len(), 3, "llm call + tool exec + llm call");

    let Step::LlmCall { tool_calls, .. } = &steps[0] else {
        panic!("expected llm call");
    };
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "call_abc");
    assert_eq!(tool_calls[0].name, "bash");

    let Step::ToolExec {
        call_id,
        name,
        output,
        ..
    } = &steps[1]
    else {
        panic!("expected tool exec");
    };
    assert_eq!(call_id, "call_abc");
    assert_eq!(name, "bash");
    assert_eq!(output.trim_end(), "rua-tool-ok");

    let Step::LlmCall { response_text, .. } = &steps[2] else {
        panic!("expected llm call");
    };
    assert_eq!(response_text, "done with tools");

    // 第二次调用的 request 从 sink 行重建（Init 锚点 + 折叠前序行）：
    // 必须携带 assistant 工具调用与 tool result。
    let requests_replayed = replay_requests(&lines.lock().unwrap());
    assert_eq!(requests_replayed.len(), 2);
    let second = &requests_replayed[1];
    assert!(
        second.iter().any(|m| matches!(
            m,
            CoreMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty()
        )),
        "rebuilt request should contain the assistant tool call"
    );
    assert!(
        second.iter().any(|m| matches!(
            m,
            CoreMessage::ToolResult { call_id, output, .. }
                if call_id == "call_abc" && output.contains("rua-tool-ok")
        )),
        "rebuilt request should contain the tool result"
    );

    // The tool result actually went back over the wire.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    let second_str = second.to_string();
    assert!(second_str.contains("\"role\":\"tool\""), "got: {second_str}");
    assert!(second_str.contains("rua-tool-ok"), "got: {second_str}");

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert!(events.iter().any(|e| matches!(
        e,
        TurnEvent::ToolExecStarted { call_id, name, .. } if call_id == "call_abc" && name == "bash"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        TurnEvent::ToolExecFinished { call_id, output_preview, .. }
            if call_id == "call_abc" && output_preview.contains("rua-tool-ok")
    )));
}

#[tokio::test]
async fn cancelled_turn_yields_cancelled_node_with_steps_preserved() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[
            text_chunk("partial"),
            final_chunk("stop"),
        ])))
        .mount(&server)
        .await;

    let engine = engine_for(&server);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let node = engine
        .run_turn(
            params(vec![CoreMessage::User {
                content: "hi".to_string(),
            }]),
            tx,
            cancel,
        )
        .await
        .unwrap();

    let NodeKind::Turn { steps, outcome, .. } = &node.kind else {
        panic!("expected turn node");
    };
    assert_eq!(*outcome, Outcome::Cancelled);
    // The in-flight LLM call is still recorded (empty response).
    assert_eq!(steps.len(), 1);
    assert!(matches!(&steps[0], Step::LlmCall { response_text, .. } if response_text.is_empty()));
}

#[tokio::test]
async fn provider_error_yields_failed_node() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let engine = engine_for(&server);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let node = engine
        .run_turn(
            params(vec![CoreMessage::User {
                content: "hi".to_string(),
            }]),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let NodeKind::Turn { steps, outcome, .. } = &node.kind else {
        panic!("expected turn node");
    };
    assert_eq!(*outcome, Outcome::Failed);
    assert_eq!(steps.len(), 1);
}

#[tokio::test]
async fn empty_history_is_a_parameter_error() {
    let server = MockServer::start().await;
    let engine = engine_for(&server);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let result = engine
        .run_turn(params(vec![]), tx, CancellationToken::new())
        .await;
    assert!(matches!(result, Err(rua_engine::Error::EmptyHistory)));
}

// ---- spawn_turn / inspect tools ----

#[derive(Default)]
struct MockSpawner {
    spawned: std::sync::Mutex<Vec<(Option<NodeId>, String, usize, Vec<String>)>>,
    inspected: std::sync::Mutex<Vec<NodeId>>,
}

impl rua_engine::TurnSpawner for MockSpawner {
    fn spawn_turn(
        &self,
        parent: Option<NodeId>,
        text: String,
        _actor: String,
        _created_by: NodeId,
        depth: usize,
        tools: Vec<String>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<rua_engine::SpawnedTurn, String>> + Send>,
    > {
        self.spawned
            .lock()
            .unwrap()
            .push((parent, text.clone(), depth, tools));
        Box::pin(async move {
            Ok(rua_engine::SpawnedTurn {
                cursor_id: "cur-child".to_string(),
                input_node_id: NodeId::new().to_string(),
                turn_node_id: NodeId::new().to_string(),
            })
        })
    }

    fn inspect(
        &self,
        node: NodeId,
        _wait: Option<std::time::Duration>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<rua_engine::InspectOutcome, String>> + Send>,
    > {
        self.inspected.lock().unwrap().push(node);
        Box::pin(async move {
            Ok(rua_engine::InspectOutcome::Committed {
                outcome: Some("completed".to_string()),
                text: "child result".to_string(),
                usage: None,
            })
        })
    }
}

#[tokio::test]
async fn spawn_turn_then_inspect_roundtrip() {
    let server = MockServer::start().await;
    // wiremock 按挂载顺序（先挂先匹配）尝试。History 会累积，后面的请求
    // 同时包含更早的工具结果，所以越晚出现的标记越要先挂。
    // Request 3（带 inspect 结果）：最终文本。
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("child result"))
        .respond_with(sse_response(sse(&[
            text_chunk("all done"),
            final_chunk("stop"),
        ])))
        .mount(&server)
        .await;
    // Request 2（带 spawn 结果）：inspect 调用。
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("turn_node_id"))
        .respond_with(sse_response(sse(&[
            tool_call_chunk("call_inspect", "inspect", r#"{"pointer":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}"#),
            final_chunk("tool_calls"),
        ])))
        .mount(&server)
        .await;
    // Request 1（无标记）：spawn_turn 调用。
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[
            tool_call_chunk(
                "call_spawn",
                "spawn_turn",
                r#"{"pointer":null,"content":"subtask"}"#,
            ),
            final_chunk("tool_calls"),
        ])))
        .mount(&server)
        .await;

    let engine = engine_for(&server);
    let spawner = std::sync::Arc::new(MockSpawner::default());
    engine.set_spawner(spawner.clone());

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let node = engine
        .run_turn(
            params(vec![CoreMessage::User {
                content: "delegate".to_string(),
            }]),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let NodeKind::Turn { steps, outcome, tools, .. } = &node.kind else {
        panic!("expected turn node");
    };
    assert_eq!(*outcome, Outcome::Completed);
    assert_eq!(steps.len(), 5, "llm + spawn + llm + inspect + llm");
    // Turn 节点记录该轮有效工具集（spawner 在位、depth 0 → 全量）。
    assert_eq!(
        tools,
        &vec![
            "bash".to_string(),
            "spawn_turn".to_string(),
            "inspect".to_string()
        ]
    );

    // spawn_turn reached the runtime with parent=None, content, depth+1,
    // and the inherited effective tool set (no `tools` arg passed).
    let spawned = spawner.spawned.lock().unwrap();
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0].0, None);
    assert_eq!(spawned[0].1, "subtask");
    assert_eq!(spawned[0].2, 1, "child runs at depth 1");
    assert_eq!(
        spawned[0].3,
        vec![
            "bash".to_string(),
            "spawn_turn".to_string(),
            "inspect".to_string()
        ],
        "child inherits the parent's full effective set by default"
    );

    // inspect reached the runtime with the pointer from the tool args.
    let inspected = spawner.inspected.lock().unwrap();
    assert_eq!(inspected.len(), 1);
    assert_eq!(
        inspected[0].to_string(),
        "01ARZ3NDEKTSV4RRFFQ69G5FAV"
    );

    // The inspect result fed back into history (and matched mock 3).
    let Step::ToolExec { name, output, .. } = &steps[3] else {
        panic!("expected tool exec at steps[3]");
    };
    assert_eq!(name, "inspect");
    assert!(output.contains("child result"), "got: {output}");

    // The spawn tool defs went over the wire (depth 0 < MAX_SPAWN_DEPTH).
    let requests = server.received_requests().await.unwrap();
    let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let tools = first["tools"].to_string();
    assert!(tools.contains("spawn_turn"), "got: {tools}");
    assert!(tools.contains("inspect"), "got: {tools}");
}

#[tokio::test]
async fn spawn_tools_gated_by_spawner_and_depth() {
    // No spawner injected -> spawn/inspect not advertised.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[text_chunk("ok"), final_chunk("stop")])))
        .mount(&server)
        .await;
    let engine = engine_for(&server);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    engine
        .run_turn(
            params(vec![CoreMessage::User {
                content: "hi".to_string(),
            }]),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    let tools = serde_json::from_slice::<serde_json::Value>(&requests[0].body).unwrap()["tools"]
        .to_string();
    assert!(tools.contains("bash"), "got: {tools}");
    assert!(!tools.contains("spawn_turn"), "got: {tools}");
    assert!(!tools.contains("\"inspect\""), "got: {tools}");

    // Spawner injected but depth exhausted -> still not advertised.
    let server2 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[text_chunk("ok"), final_chunk("stop")])))
        .mount(&server2)
        .await;
    let engine2 = engine_for(&server2);
    engine2.set_spawner(std::sync::Arc::new(MockSpawner::default()));
    let mut p = params(vec![CoreMessage::User {
        content: "hi".to_string(),
    }]);
    p.depth = rua_engine::spawn::MAX_SPAWN_DEPTH;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    engine2.run_turn(p, tx, CancellationToken::new()).await.unwrap();
    let requests = server2.received_requests().await.unwrap();
    let tools = serde_json::from_slice::<serde_json::Value>(&requests[0].body).unwrap()["tools"]
        .to_string();
    assert!(!tools.contains("spawn_turn"), "got: {tools}");
}

/// 发送时工具覆盖：只留 bash（即使 spawner 在位、深度未用尽）。
#[tokio::test]
async fn tools_override_filters_advertised_tools() {
    let server3 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[text_chunk("ok"), final_chunk("stop")])))
        .mount(&server3)
        .await;
    let engine3 = engine_for(&server3);
    engine3.set_spawner(std::sync::Arc::new(MockSpawner::default()));
    let mut p = params(vec![CoreMessage::User {
        content: "hi".to_string(),
    }]);
    p.tools = Some(vec!["bash".to_string()]);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    engine3.run_turn(p, tx, CancellationToken::new()).await.unwrap();
    let requests = server3.received_requests().await.unwrap();
    let tools = serde_json::from_slice::<serde_json::Value>(&requests[0].body).unwrap()["tools"]
        .to_string();
    assert!(tools.contains("bash"), "got: {tools}");
    assert!(!tools.contains("spawn_turn"), "got: {tools}");
    assert!(!tools.contains("\"inspect\""), "got: {tools}");
}

/// 分发拦截：override 只留 bash，模型仍调 spawn_turn → ToolResult 软拒绝
/// （Step/事件照记），spawner 未被调用。
#[tokio::test]
async fn disabled_tool_call_is_soft_rejected() {
    let server = MockServer::start().await;
    // Request 2（带软拒绝的 tool result）：最终文本。
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("tool not enabled"))
        .respond_with(sse_response(sse(&[
            text_chunk("sorry, no spawn"),
            final_chunk("stop"),
        ])))
        .mount(&server)
        .await;
    // Request 1：模型硬发 spawn_turn。
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[
            tool_call_chunk(
                "call_spawn",
                "spawn_turn",
                r#"{"pointer":null,"content":"subtask"}"#,
            ),
            final_chunk("tool_calls"),
        ])))
        .mount(&server)
        .await;

    let engine = engine_for(&server);
    let spawner = std::sync::Arc::new(MockSpawner::default());
    engine.set_spawner(spawner.clone());
    let mut p = params(vec![CoreMessage::User {
        content: "delegate".to_string(),
    }]);
    p.tools = Some(vec!["bash".to_string()]);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let node = engine.run_turn(p, tx, CancellationToken::new()).await.unwrap();

    let NodeKind::Turn { steps, outcome, .. } = &node.kind else {
        panic!("expected turn node");
    };
    assert_eq!(*outcome, Outcome::Completed);
    assert_eq!(steps.len(), 3, "llm + rejected tool exec + llm");
    let Step::ToolExec { name, output, .. } = &steps[1] else {
        panic!("expected tool exec");
    };
    assert_eq!(name, "spawn_turn");
    assert!(
        output.contains("error: tool not enabled: spawn_turn"),
        "got: {output}"
    );
    // 软拒绝：从未到达 runtime。
    assert!(spawner.spawned.lock().unwrap().is_empty());
}

/// spawn 显式子集：父轮全量，spawn_turn 传 tools=["bash"] → 子代收到
/// ["bash"]（校验通过的子集）。
#[tokio::test]
async fn spawn_explicit_tools_subset_passes_validation() {
    let server = MockServer::start().await;
    // Request 2（带 spawn 结果）：最终文本。
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("turn_node_id"))
        .respond_with(sse_response(sse(&[
            text_chunk("spawned"),
            final_chunk("stop"),
        ])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[
            tool_call_chunk(
                "call_spawn",
                "spawn_turn",
                r#"{"pointer":null,"content":"subtask","tools":["bash"]}"#,
            ),
            final_chunk("tool_calls"),
        ])))
        .mount(&server)
        .await;

    let engine = engine_for(&server);
    let spawner = std::sync::Arc::new(MockSpawner::default());
    engine.set_spawner(spawner.clone());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let node = engine
        .run_turn(
            params(vec![CoreMessage::User {
                content: "delegate".to_string(),
            }]),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let NodeKind::Turn { outcome, .. } = &node.kind else {
        panic!("expected turn node");
    };
    assert_eq!(*outcome, Outcome::Completed);
    let spawned = spawner.spawned.lock().unwrap();
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0].3, vec!["bash".to_string()]);
}

/// spawn 显式 tools 的严格校验：未知名或超出父有效集 → error: invalid
/// tools，spawner 未被调用。
#[tokio::test]
async fn spawn_invalid_tools_subset_is_rejected() {
    // (a) 未知名：父轮全量，传 ["nonexistent"]。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("invalid tools"))
        .respond_with(sse_response(sse(&[
            text_chunk("fixed"),
            final_chunk("stop"),
        ])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[
            tool_call_chunk(
                "call_spawn",
                "spawn_turn",
                r#"{"pointer":null,"content":"subtask","tools":["nonexistent"]}"#,
            ),
            final_chunk("tool_calls"),
        ])))
        .mount(&server)
        .await;
    let engine = engine_for(&server);
    let spawner = std::sync::Arc::new(MockSpawner::default());
    engine.set_spawner(spawner.clone());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let node = engine
        .run_turn(
            params(vec![CoreMessage::User {
                content: "delegate".to_string(),
            }]),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let NodeKind::Turn { steps, .. } = &node.kind else {
        panic!("expected turn node");
    };
    let Step::ToolExec { output, .. } = &steps[1] else {
        panic!("expected tool exec");
    };
    assert!(output.contains("error: invalid tools"), "got: {output}");
    assert!(output.contains("nonexistent"), "got: {output}");
    assert!(spawner.spawned.lock().unwrap().is_empty());

    // (b) 父有效集之外的项：父轮 override 不含 inspect，传 ["inspect"]。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("invalid tools"))
        .respond_with(sse_response(sse(&[
            text_chunk("fixed"),
            final_chunk("stop"),
        ])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[
            tool_call_chunk(
                "call_spawn",
                "spawn_turn",
                r#"{"pointer":null,"content":"subtask","tools":["inspect"]}"#,
            ),
            final_chunk("tool_calls"),
        ])))
        .mount(&server)
        .await;
    let engine = engine_for(&server);
    let spawner = std::sync::Arc::new(MockSpawner::default());
    engine.set_spawner(spawner.clone());
    let mut p = params(vec![CoreMessage::User {
        content: "delegate".to_string(),
    }]);
    p.tools = Some(vec!["bash".to_string(), "spawn_turn".to_string()]);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let node = engine.run_turn(p, tx, CancellationToken::new()).await.unwrap();
    let NodeKind::Turn { steps, .. } = &node.kind else {
        panic!("expected turn node");
    };
    let Step::ToolExec { output, .. } = &steps[1] else {
        panic!("expected tool exec");
    };
    assert!(output.contains("error: invalid tools"), "got: {output}");
    assert!(spawner.spawned.lock().unwrap().is_empty());
}
