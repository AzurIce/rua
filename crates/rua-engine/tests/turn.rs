//! Integration tests for `run_turn` against a mock OpenAI-compatible
//! (DeepSeek-shaped) SSE endpoint.

use std::sync::{Arc, Mutex};

use rua_engine::config::ProviderConfig;
use rua_graph::events::TurnEvent;
use rua_graph::id::{CursorId, NodeId};
use rua_graph::message::CoreMessage;
use rua_graph::node::{Outcome, Step, Turn, TurnLine, Usage};
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
    engine_with_retry(server, 3, 1000)
}

/// 可配重试策略的 engine（超时字段保持默认值；wiremock 本地无网络延迟）。
fn engine_with_retry(server: &MockServer, max_retries: u32, retry_base_ms: u64) -> Engine {
    let config = ProviderConfig {
        kind: "deepseek".to_string(),
        api_key: "test-key".to_string(),
        base_url: server.uri(),
        models: Vec::new(),
        additional_params: serde_json::Map::new(),
        connect_timeout_secs: 10,
        read_timeout_secs: 120,
        llm_max_retries: max_retries,
        llm_retry_base_ms: retry_base_ms,
    };
    Engine::new(
        &[("deepseek".to_string(), config)],
        std::env::current_dir().unwrap(),
    )
    .unwrap()
}

fn params(history: Vec<CoreMessage>) -> TurnParams {
    TurnParams {
        cursor_id: CursorId::new(),
        node_id: NodeId::new(),
        // engine 不校验 parent 的存在性（那是 Graph::commit 的职责），
        // 测试里铸一个占位即可。
        parent: NodeId::new(),
        actor: "human".to_string(),
        model: "deepseek/deepseek-v4-pro".to_string(),
        history,
        system_prompt: Some("You are helpful.".to_string()),
        depth: 0,
        tools: None,
        sink: None,
    }
}

/// 收集型 sink：把 run_turn 期间发出的所有 TurnLine 收进共享 vec。
fn collecting_sink() -> (
    Arc<Mutex<Vec<TurnLine>>>,
    Box<dyn FnMut(TurnLine) -> Result<(), String> + Send>,
) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let into = lines.clone();
    (
        lines,
        Box::new(move |line: TurnLine| {
            into.lock().unwrap().push(line);
            Ok(())
        }),
    )
}

/// sink 行流 → steps（`Init` 锚点不是 step）。engine 返回的节点是
/// header-only（正文在数据面），测试经 sink 行流观察 steps。
fn steps_of(lines: &[TurnLine]) -> Vec<Step> {
    lines
        .iter()
        .cloned()
        .filter_map(TurnLine::into_step)
        .collect()
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
    p.model = "deepseek/other-model".to_string();
    let node = engine.run_turn(p, tx, CancellationToken::new()).await.unwrap();
    assert_eq!(node.kind.outcome, Outcome::Completed);
    assert_eq!(node.kind.model, "deepseek/other-model");
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

    let lines = lines.lock().unwrap();
    let steps = steps_of(&lines);
    assert_eq!(node.kind.outcome, Outcome::Completed);
    assert_eq!(node.kind.actor, "human");
    assert_eq!(node.kind.model, "deepseek/deepseek-v4-pro");
    // 无 spawner：有效工具集只有 bash，记录在 Turn 节点上。
    assert_eq!(node.kind.tools, vec!["bash".to_string()]);
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
    assert_eq!(node.kind.usage.input_tokens, 10);

    // Sink: Init 锚点（系统提示 + 初始 user 消息）+ 一条 LlmCall 行。
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
    // 每次调用完成推一条累计用量事件（provider 只在流末尾给数）。
    let finished: Vec<(usize, Usage)> = events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::LlmCallFinished { step, usage, .. } => Some((*step, *usage)),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        vec![(
            0,
            Usage {
                input_tokens: 10,
                output_tokens: 5,
                reasoning_tokens: 0,
                cached_input_tokens: 4
            }
        )]
    );
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

    let (steps, outcome) = (steps_of(&lines.lock().unwrap()), node.kind.outcome);
    assert_eq!(outcome, Outcome::Completed);
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
    // 两次调用的累计用量：step 下标落在各自 LlmCall 行上（llm + tool + llm），
    // 第二次是第一次的累计；且「调用完成」先于它触发的工具执行事件。
    let finished: Vec<(usize, Usage)> = events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::LlmCallFinished { step, usage, .. } => Some((*step, *usage)),
            _ => None,
        })
        .collect();
    let usage = |i: u64, o: u64| Usage {
        input_tokens: i,
        output_tokens: o,
        reasoning_tokens: 0,
        cached_input_tokens: i * 4 / 10,
    };
    assert_eq!(finished, vec![(0, usage(10, 5)), (2, usage(20, 10))]);
    let llm_done = events
        .iter()
        .position(|e| matches!(e, TurnEvent::LlmCallFinished { .. }))
        .unwrap();
    let tool_start = events
        .iter()
        .position(|e| matches!(e, TurnEvent::ToolExecStarted { .. }))
        .unwrap();
    assert!(llm_done < tool_start);
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
    let (lines, sink) = collecting_sink();
    let mut p = params(vec![CoreMessage::User {
        content: "hi".to_string(),
    }]);
    p.sink = Some(sink);
    let node = engine.run_turn(p, tx, cancel).await.unwrap();

    let (steps, outcome) = (steps_of(&lines.lock().unwrap()), node.kind.outcome);
    assert_eq!(outcome, Outcome::Cancelled);
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

    // 本测试考察「不可恢复的失败直接 Failed」语义，关掉重试（默认开启时
    // 500 属可重试类，会退避重试后同样 Failed，但拖慢测试）。
    let engine = engine_with_retry(&server, 0, 1000);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (lines, sink) = collecting_sink();
    let mut p = params(vec![CoreMessage::User {
        content: "hi".to_string(),
    }]);
    p.sink = Some(sink);
    let node = engine.run_turn(p, tx, CancellationToken::new()).await.unwrap();

    let (steps, outcome) = (steps_of(&lines.lock().unwrap()), node.kind.outcome);
    assert_eq!(outcome, Outcome::Failed);
    assert_eq!(steps.len(), 1);
}

#[tokio::test]
async fn retryable_failure_before_any_content_is_retried() {
    let server = MockServer::start().await;
    // 首次 500（可重试类），之后正常 SSE：整轮应重试并 Completed，且
    // 全程只产生一个 LlmCall step（重试发生在任何内容吐出之前）。
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[
            text_chunk("hello"),
            final_chunk("stop"),
        ])))
        .mount(&server)
        .await;

    let engine = engine_with_retry(&server, 3, 1);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (lines, sink) = collecting_sink();
    let mut p = params(vec![CoreMessage::User {
        content: "hi".to_string(),
    }]);
    p.sink = Some(sink);
    let node = engine.run_turn(p, tx, CancellationToken::new()).await.unwrap();

    assert_eq!(node.kind.outcome, Outcome::Completed);
    let steps = steps_of(&lines.lock().unwrap());
    assert_eq!(steps.len(), 1);
    assert!(matches!(
        &steps[0],
        Step::LlmCall { response_text, .. } if response_text == "hello"
    ));
    // 恰好 2 次请求：1 次失败 + 1 次重试成功。
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn non_retryable_failure_is_not_retried() {
    let server = MockServer::start().await;
    // 400 = 请求本身有问题，重试无意义：即使重试开启也只发 1 次。
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&server)
        .await;

    let engine = engine_for(&server);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut p = params(vec![CoreMessage::User {
        content: "hi".to_string(),
    }]);
    p.sink = Some(collecting_sink().1);
    let node = engine.run_turn(p, tx, CancellationToken::new()).await.unwrap();

    assert_eq!(node.kind.outcome, Outcome::Failed);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
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

// ---- script tool (PTC) ----

#[derive(Default)]
struct MockScriptHost {
    calls: std::sync::Mutex<Vec<(String, NodeId<Turn>, usize, Vec<String>)>>,
}

impl rua_engine::ScriptHost for MockScriptHost {
    fn run(
        &self,
        code: &str,
        me: NodeId<Turn>,
        depth: usize,
        tools: Vec<String>,
        _cancel: CancellationToken,
    ) -> String {
        self.calls
            .lock()
            .unwrap()
            .push((code.to_string(), me, depth, tools));
        format!("host output for: {code}")
    }
}

#[tokio::test]
async fn script_roundtrip_delegates_to_host() {
    let server = MockServer::start().await;
    // Request 2（带 host 输出的 tool result）：最终文本。
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("host output for"))
        .respond_with(sse_response(sse(&[
            text_chunk("all done"),
            final_chunk("stop"),
        ])))
        .mount(&server)
        .await;
    // Request 1（无标记）：script 调用。
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[
            tool_call_chunk(
                "call_script",
                "script",
                r#"{"code":"console.log(graph.spawn({content:'subtask'}).turn_node_id)"}"#,
            ),
            final_chunk("tool_calls"),
        ])))
        .mount(&server)
        .await;

    let engine = engine_for(&server);
    let host = std::sync::Arc::new(MockScriptHost::default());
    engine.set_script_host(host.clone());

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (lines, sink) = collecting_sink();
    let mut p = params(vec![CoreMessage::User {
        content: "delegate".to_string(),
    }]);
    p.sink = Some(sink);
    let node = engine.run_turn(p, tx, CancellationToken::new()).await.unwrap();

    let (steps, outcome, tools) = (
        steps_of(&lines.lock().unwrap()),
        node.kind.outcome,
        node.kind.tools.clone(),
    );
    assert_eq!(outcome, Outcome::Completed);
    assert_eq!(steps.len(), 3, "llm + script + llm");
    // Turn 节点记录该轮有效工具集（host 在位、depth 0 → 全量）。
    assert_eq!(tools, vec!["bash".to_string(), "script".to_string()]);

    // host 收到：完整 code、调用方 turn id、depth、调用方有效工具集。
    let calls = host.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].0.contains("graph.spawn"), "got: {}", calls[0].0);
    assert_eq!(calls[0].1, node.id);
    assert_eq!(calls[0].2, 0);
    assert_eq!(calls[0].3, vec!["bash".to_string(), "script".to_string()]);
    drop(calls);

    // host 输出回灌进历史（ToolResult 文本 = console 输出）。
    let Step::ToolExec { name, output, .. } = &steps[1] else {
        panic!("expected tool exec at steps[1]");
    };
    assert_eq!(name, "script");
    assert!(output.contains("host output for"), "got: {output}");

    // script 的 schema 走了 wire（host 在位、depth 0 < MAX_SPAWN_DEPTH）。
    let requests = server.received_requests().await.unwrap();
    let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let tools = first["tools"].to_string();
    assert!(tools.contains("script"), "got: {tools}");
}

#[tokio::test]
async fn script_gated_by_host_and_depth() {
    // No host injected -> script not advertised.
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
    assert!(!tools.contains("\"script\""), "got: {tools}");

    // Host injected but depth exhausted -> still not advertised.
    let server2 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[text_chunk("ok"), final_chunk("stop")])))
        .mount(&server2)
        .await;
    let engine2 = engine_for(&server2);
    engine2.set_script_host(std::sync::Arc::new(MockScriptHost::default()));
    let mut p = params(vec![CoreMessage::User {
        content: "hi".to_string(),
    }]);
    p.depth = rua_engine::script::MAX_SPAWN_DEPTH;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    engine2.run_turn(p, tx, CancellationToken::new()).await.unwrap();
    let requests = server2.received_requests().await.unwrap();
    let tools = serde_json::from_slice::<serde_json::Value>(&requests[0].body).unwrap()["tools"]
        .to_string();
    assert!(!tools.contains("\"script\""), "got: {tools}");
}

/// 发送时工具覆盖：只留 bash（即使 host 在位、深度未用尽）。
#[tokio::test]
async fn tools_override_filters_advertised_tools() {
    let server3 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[text_chunk("ok"), final_chunk("stop")])))
        .mount(&server3)
        .await;
    let engine3 = engine_for(&server3);
    engine3.set_script_host(std::sync::Arc::new(MockScriptHost::default()));
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
    assert!(!tools.contains("\"script\""), "got: {tools}");
}

/// 分发拦截：override 只留 bash，模型仍调 script → ToolResult 软拒绝
/// （Step/事件照记），host 未被调用。
#[tokio::test]
async fn disabled_tool_call_is_soft_rejected() {
    let server = MockServer::start().await;
    // Request 2（带软拒绝的 tool result）：最终文本。
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("tool not enabled"))
        .respond_with(sse_response(sse(&[
            text_chunk("sorry, no script"),
            final_chunk("stop"),
        ])))
        .mount(&server)
        .await;
    // Request 1：模型硬发 script。
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[
            tool_call_chunk("call_script", "script", r#"{"code":"1"}"#),
            final_chunk("tool_calls"),
        ])))
        .mount(&server)
        .await;

    let engine = engine_for(&server);
    let host = std::sync::Arc::new(MockScriptHost::default());
    engine.set_script_host(host.clone());
    let mut p = params(vec![CoreMessage::User {
        content: "delegate".to_string(),
    }]);
    p.tools = Some(vec!["bash".to_string()]);
    let (lines, sink) = collecting_sink();
    p.sink = Some(sink);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let node = engine.run_turn(p, tx, CancellationToken::new()).await.unwrap();

    let (steps, outcome) = (steps_of(&lines.lock().unwrap()), node.kind.outcome);
    assert_eq!(outcome, Outcome::Completed);
    assert_eq!(steps.len(), 3, "llm + rejected tool exec + llm");
    let Step::ToolExec { name, output, .. } = &steps[1] else {
        panic!("expected tool exec");
    };
    assert_eq!(name, "script");
    assert!(
        output.contains("error: tool not enabled: script"),
        "got: {output}"
    );
    // 软拒绝：从未到达 host。
    assert!(host.calls.lock().unwrap().is_empty());
}

/// 正文落盘失败（第一条 llm_call 行写不进去）：以 Failed 终止本轮，**失败
/// 行不入账**（数据面是唯一账本——server 侧 Entry::append 的文件+内存同
/// 临界区性质在这里表现为：sink 返回 Err 的 step 不进 steps），且不再发起
/// 后续 LLM 调用。
#[tokio::test]
async fn sink_failure_terminates_turn_as_failed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[
            text_chunk("Hello, "),
            text_chunk("world!"),
            final_chunk("stop"),
        ])))
        .mount(&server)
        .await;

    let engine = engine_for(&server);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    // 记录 sink 收到的全部尝试，llm_call 行返回失败。
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let into = attempts.clone();
    let sink: Box<dyn FnMut(TurnLine) -> std::result::Result<(), String> + Send> =
        Box::new(move |line: TurnLine| {
            let fails = matches!(line, TurnLine::LlmCall { .. });
            into.lock().unwrap().push(line);
            if fails { Err("disk full".to_string()) } else { Ok(()) }
        });
    let mut p = params(vec![CoreMessage::User {
        content: "hi".to_string(),
    }]);
    p.sink = Some(sink);
    let node = engine.run_turn(p, tx, CancellationToken::new()).await.unwrap();

    assert_eq!(node.kind.outcome, Outcome::Failed);
    // sink 见到的尝试：Init 锚点 + 失败的 llm_call（失败即终止）。
    assert_eq!(attempts.lock().unwrap().len(), 2);
    // 没有发起第二次 LLM 调用。
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// Init 锚点就写不进去：零 step 的 Failed 轮，不发起任何 LLM 调用。
#[tokio::test]
async fn init_sink_failure_yields_empty_failed_turn() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(sse(&[text_chunk("hi"), final_chunk("stop")])))
        .mount(&server)
        .await;

    let engine = engine_for(&server);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let sink: Box<dyn FnMut(TurnLine) -> std::result::Result<(), String> + Send> =
        Box::new(|_line: TurnLine| Err("read-only fs".to_string()));
    let mut p = params(vec![CoreMessage::User {
        content: "hi".to_string(),
    }]);
    p.sink = Some(sink);
    let node = engine.run_turn(p, tx, CancellationToken::new()).await.unwrap();

    assert_eq!(node.kind.outcome, Outcome::Failed);
    // Init 失败 = 正文无法持久化：一次 LLM 调用都不发起。
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}
