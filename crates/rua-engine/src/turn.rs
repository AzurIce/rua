//! `run_turn`: the agent loop. Streams LLM calls, executes bash tool calls,
//! records every step, and returns a turn `Node` for the server to commit.
//!
//! Cancellation and provider failures still produce a node (outcome
//! `Cancelled` / `Failed`, steps so far preserved); only parameter-level
//! problems (empty history) surface as `Err`.

use futures::StreamExt;
use rig_core::completion::CompletionModel;
use rig_core::streaming::StreamedAssistantContent;
use rua_graph::events::TurnEvent;
use rua_graph::id::{CursorId, NodeId};
use rua_graph::message::{CoreMessage, CoreToolCall};
use rua_graph::node::{Input, Node, Outcome, Step, Turn, TurnLine, Usage};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::client::{Engine, RetryPolicy, error_chain_text, is_retryable_llm_error};
use crate::error::{Error, Result};
use crate::message::history_to_rig;
use crate::prompt::{EffectiveTools, build_system_prompt};
use crate::tools::BashTool;

/// Maximum number of tool-call rounds inside one turn.
pub const MAX_TOOL_ROUNDS: usize = 32;

/// Preview length (chars) for `ToolExecFinished` events.
const OUTPUT_PREVIEW_CHARS: usize = 500;

pub struct TurnParams {
    pub cursor_id: CursorId,
    /// Pre-allocated landing node id（`CursorMut::open_turn` 铸好并注册了
    /// 数据面空条目）。
    pub node_id: NodeId<Turn>,
    /// 相继边：本轮回应的 Input。
    pub parent: NodeId<Input>,
    pub actor: String,
    pub model: String,
    /// Assembled history (`crate::assemble` output); must be non-empty.
    pub history: Vec<CoreMessage>,
    /// 调用方附加段：拼在 engine 按有效工具集组装出的系统提示词之后
    /// （空行分隔）。不再是完整提示词。
    pub system_prompt: Option<String>,
    /// spawn 递归深度（0 = 用户发起）。达到 MAX_SPAWN_DEPTH 后不再
    /// 注册 spawn_turn/inspect 工具。
    pub depth: usize,
    /// 本次发送的工具列表覆盖（None = 全部可用工具）。改工具列表会改变
    /// 请求前缀 → 前缀缓存失效（开发测试时这正是目的）。
    pub tools: Option<Vec<String>>,
    /// 增量落盘回调：每个 step 完成时同步收到对应的 `TurnLine`（首个 LLM
    /// 调用前另收一条 `Init` 锚点）。server 侧注入的是数据面条目的
    /// `Entry<Turn>::append`（文件 + 内存同一临界区）。engine 的未来是
    /// `!Send`、跑在专用线程，sink 只需可调用。返回 Err = 正文无法
    /// 持久化：engine 以 `Outcome::Failed` 终止本轮，**落盘失败的 step
    /// 不入账**（数据面是唯一账本；meta 由落盘成功的 steps 推导）。
    pub sink: Option<Box<dyn FnMut(TurnLine) -> std::result::Result<(), String> + Send>>,
}

/// What one streamed LLM call produced.
struct CallOutcome {
    text: String,
    reasoning: String,
    tool_calls: Vec<CoreToolCall>,
    usage: Usage,
    cancelled: bool,
    error: Option<String>,
    duration_ms: u64,
}

impl CallOutcome {
    fn empty() -> Self {
        Self {
            text: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            usage: Usage::default(),
            cancelled: false,
            error: None,
            duration_ms: 0,
        }
    }

    /// 该次调用没产出任何可见内容（text/reasoning/tool_calls 全空）——
    /// 只有这种失败可以安全重试；流建立后重试会把已发出的内容重复一遍。
    fn is_blank(&self) -> bool {
        self.text.is_empty() && self.reasoning.is_empty() && self.tool_calls.is_empty()
    }
}

impl Engine {
    pub async fn run_turn(
        &self,
        params: TurnParams,
        events: UnboundedSender<TurnEvent>,
        cancel: CancellationToken,
    ) -> Result<Node<Turn>> {
        let TurnParams {
            cursor_id,
            node_id,
            parent,
            actor,
            model,
            mut history,
            system_prompt,
            depth,
            tools,
            mut sink,
        } = params;
        if history.is_empty() {
            return Err(Error::EmptyHistory);
        }

        // 有效工具集只算一次：schema 注册、提示词组装、执行分发三处共用。
        let effective =
            EffectiveTools::compute(tools.as_deref(), self.script.get().is_some(), depth);
        // 最终提示词 = engine 按工具集组装 + 调用方附加段（空行拼接在后）。
        let mut prompt = build_system_prompt(&effective);
        if let Some(extra) = system_prompt.as_deref() {
            prompt.push_str("\n\n");
            prompt.push_str(extra);
        }

        let send = |event: TurnEvent| {
            let _ = events.send(event);
        };
        send(TurnEvent::Started {
            cursor_id,
            node_id: node_id.raw(),
        });

        // Init 锚点：首次 LLM 调用的完整请求快照（系统提示 + 初始历史），
        // 一轮至多一份，是轮内 request 重建的起点。落盘失败 = 正文无法
        // 持久化，以此终止本轮（Failed、零 step）。
        let mut persist_ok = true;
        if let Some(sink) = &mut sink {
            if let Err(e) = sink(TurnLine::Init {
                request: request_snapshot(&history, &prompt),
            }) {
                tracing::error!(turn = %node_id.raw(), error = %e, "init anchor persist failed");
                persist_ok = false;
            }
        }

        let mut steps: Vec<Step> = Vec::new();
        let mut usage = Usage::default();
        let mut tool_rounds = 0usize;

        let outcome = 'agent: loop {
            if !persist_ok {
                break Outcome::Failed;
            }
            let call = self
                .stream_call_with_retry(
                    &model,
                    &history,
                    &prompt,
                    cursor_id,
                    node_id,
                    &effective,
                    &send,
                    &cancel,
                )
                .await;

            usage.add_assign(&call.usage);
            let step = Step::LlmCall {
                response_text: call.text.clone(),
                tool_calls: call.tool_calls.clone(),
                reasoning: (!call.reasoning.is_empty()).then_some(call.reasoning.clone()),
                usage: call.usage,
                provider_data: None,
            };
            // 先落盘再入账：落盘失败的 step 不进 steps（数据面是唯一账本）。
            if let Some(sink) = &mut sink {
                if let Err(e) = sink(TurnLine::from(step.clone())) {
                    tracing::error!(turn = %node_id.raw(), error = %e, "turn body persist failed");
                    break Outcome::Failed;
                }
            }
            steps.push(step);
            // 累计用量随每次调用完成推给订阅方（provider 只在流末尾给数，
            // 这是 in-flight 用量能刷新的最细粒度）。
            send(TurnEvent::LlmCallFinished {
                cursor_id,
                node_id: node_id.raw(),
                step: steps.len() - 1,
                usage,
            });
            history.push(CoreMessage::Assistant {
                content: call.text.clone(),
                tool_calls: call.tool_calls.clone(),
            });

            if call.cancelled {
                break Outcome::Cancelled;
            }
            if call.error.is_some() {
                break Outcome::Failed;
            }
            if call.tool_calls.is_empty() {
                break Outcome::Completed;
            }
            tool_rounds += 1;
            if tool_rounds > MAX_TOOL_ROUNDS {
                break Outcome::Failed;
            }

            for call_tool in &call.tool_calls {
                send(TurnEvent::ToolExecStarted {
                    cursor_id,
                    node_id: node_id.raw(),
                    call_id: call_tool.id.clone(),
                    name: call_tool.name.clone(),
                    args: call_tool.args.clone(),
                });
                let start = std::time::Instant::now();
                // 软拒绝：模型发出未启用工具的调用 → ToolResult 返回错误，
                // 事件/Step 照记，turn 继续。
                let output = if !effective.allowed(&call_tool.name) {
                    format!("error: tool not enabled: {}", call_tool.name)
                } else {
                    match call_tool.name.as_str() {
                        "bash" => self.bash.execute(&call_tool.args, &cancel).await,
                        "script" => match self.script.get() {
                            Some(host) => {
                                let code = call_tool
                                    .args
                                    .get("code")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default();
                                host.run(
                                    code,
                                    node_id,
                                    depth,
                                    effective.names(),
                                    cancel.clone(),
                                )
                            }
                            None => "error: script is not available".to_string(),
                        },
                        other => format!("error: unknown tool: {other}"),
                    }
                };
                let duration_ms = start.elapsed().as_millis() as u64;
                tracing::debug!(
                    turn = %node_id.raw(),
                    tool = %call_tool.name,
                    duration_ms,
                    output_bytes = output.len(),
                    "tool exec finished"
                );
                send(TurnEvent::ToolExecFinished {
                    cursor_id,
                    node_id: node_id.raw(),
                    call_id: call_tool.id.clone(),
                    output_preview: output.chars().take(OUTPUT_PREVIEW_CHARS).collect(),
                    duration_ms,
                });
                let step = Step::ToolExec {
                    call_id: call_tool.id.clone(),
                    name: call_tool.name.clone(),
                    args: call_tool.args.clone(),
                    output: output.clone(),
                    duration_ms,
                };
                // 先落盘再入账：落盘失败的 step 不进 steps（数据面是唯一账本）。
                if let Some(sink) = &mut sink {
                    if let Err(e) = sink(TurnLine::from(step.clone())) {
                        tracing::error!(turn = %node_id.raw(), error = %e, "turn body persist failed");
                        break 'agent Outcome::Failed;
                    }
                }
                steps.push(step);
                history.push(CoreMessage::ToolResult {
                    call_id: call_tool.id.clone(),
                    name: call_tool.name.clone(),
                    output,
                });
            }
        };

        Ok(Turn::node(
            node_id,
            parent,
            outcome,
            actor,
            model,
            usage,
            effective.names(),
            &steps,
        ))
    }

    /// [`Engine::stream_call`] 的重试包装：错误分类 + 指数退避（策略来自
    /// 该 provider 的 config）。只有「还没吐出任何内容」的可重试失败才
    /// 重试；重试等待期间可被取消（返回 cancelled 形态，走正常闭账）。
    async fn stream_call_with_retry(
        &self,
        model_ref: &str,
        history: &[CoreMessage],
        prompt: &str,
        cursor_id: CursorId,
        node_id: NodeId<Turn>,
        tools: &EffectiveTools,
        send: &dyn Fn(TurnEvent),
        cancel: &CancellationToken,
    ) -> CallOutcome {
        let retry = self
            .model_for(model_ref)
            .map(|(_, _, retry)| retry)
            .unwrap_or(RetryPolicy {
                max_retries: 0,
                base_delay_ms: 0,
            });
        let mut attempt = 0u32;
        loop {
            let call = self
                .stream_call(model_ref, history, prompt, cursor_id, node_id, tools, send, cancel)
                .await;
            // 重试资格：非取消、零内容、错误在可重试类别里。
            let eligible = !call.cancelled
                && call.is_blank()
                && call
                    .error
                    .as_deref()
                    .is_some_and(|e| is_retryable_llm_error(e));
            match (call.error.as_deref(), eligible) {
                (Some(error), true) if attempt < retry.max_retries => {
                    attempt += 1;
                    let delay = retry.delay_for(attempt);
                    tracing::warn!(
                        turn = %node_id.raw(),
                        model = model_ref,
                        attempt,
                        retry_after_ms = delay.as_millis() as u64,
                        error,
                        "llm call failed before any content; retrying"
                    );
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            return CallOutcome { cancelled: true, ..call };
                        }
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
                _ => return call,
            }
        }
    }

    /// One streaming LLM call over the current history. `model_ref` is the
    /// per-turn model override (`"provider/model"` or bare = default
    /// provider); it is resolved per call so per-send overrides actually
    /// take effect (and failures surface as a Failed turn, not a panic).
    /// `prompt` is the final system prompt; `tools` is the effective tool
    /// set (registered schemas come straight off its booleans).
    async fn stream_call(
        &self,
        model_ref: &str,
        history: &[CoreMessage],
        prompt: &str,
        cursor_id: CursorId,
        node_id: NodeId<Turn>,
        tools: &EffectiveTools,
        send: &dyn Fn(TurnEvent),
        cancel: &CancellationToken,
    ) -> CallOutcome {
        let start = std::time::Instant::now();
        let (model, additional_params, _) = match self.model_for(model_ref) {
            Ok(x) => x,
            Err(e) => {
                return CallOutcome {
                    error: Some(error_chain_text(&e)),
                    ..CallOutcome::empty()
                };
            }
        };
        let mut rig_history = history_to_rig(history);
        // The builder's `prompt` is appended as the last message; feed it the
        // tail and put everything before it into `.messages()`.
        let prompt_msg = rig_history.pop().expect("history checked non-empty");

        // 工具 schema 直接读有效工具集的布尔。
        let mut tool_defs = Vec::new();
        if tools.bash {
            tool_defs.push(BashTool::definition());
        }
        if tools.script {
            tool_defs.push(crate::script::script_definition());
        }
        let mut builder = model
            .completion_request(prompt_msg)
            .messages(rig_history)
            .tools(tool_defs);
        builder = builder.preamble(prompt.to_string());
        if let Some(additional_params) = &additional_params {
            builder = builder.additional_params(additional_params.clone());
        }

        let mut stream = match builder.stream().await {
            Ok(stream) => stream,
            Err(e) => {
                return CallOutcome {
                    error: Some(error_chain_text(&e)),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..CallOutcome::empty()
                };
            }
        };

        let mut outcome = CallOutcome::empty();

        loop {
            let item = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    stream.cancel();
                    outcome.cancelled = true;
                    break;
                }
                item = stream.next() => match item {
                    None => break,
                    Some(Ok(content)) => content,
                    Some(Err(e)) => {
                        outcome.error = Some(error_chain_text(&e));
                        break;
                    }
                },
            };
            match item {
                StreamedAssistantContent::Text(text) => {
                    outcome.text.push_str(&text.text);
                    send(TurnEvent::TextDelta {
                        cursor_id,
                        node_id: node_id.raw(),
                        delta: text.text,
                    });
                }
                StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                    outcome.reasoning.push_str(&reasoning);
                    send(TurnEvent::ReasoningDelta {
                        cursor_id,
                        node_id: node_id.raw(),
                        delta: reasoning,
                    });
                }
                // Complete reasoning block: supersedes its deltas. Only adopt
                // it when no deltas were seen (otherwise already rendered).
                StreamedAssistantContent::Reasoning { reasoning, .. } => {
                    if outcome.reasoning.is_empty() {
                        let text = reasoning.display_text();
                        if !text.is_empty() {
                            outcome.reasoning = text.clone();
                            send(TurnEvent::ReasoningDelta {
                                cursor_id,
                                node_id: node_id.raw(),
                                delta: text,
                            });
                        }
                    }
                }
                StreamedAssistantContent::ToolCall { tool_call, .. } => {
                    outcome.tool_calls.push(CoreToolCall {
                        id: tool_call.id.as_str().to_string(),
                        name: tool_call.function.name.clone(),
                        args: tool_call.function.arguments.clone(),
                    });
                }
                // Deltas are superseded by the complete ToolCall; Final usage
                // is read off the stream after the loop.
                StreamedAssistantContent::ToolCallDelta { .. }
                | StreamedAssistantContent::Final(_)
                | StreamedAssistantContent::Unknown(_) => {}
            }
        }

        let rig_usage = stream.usage();
        outcome.usage = Usage {
            input_tokens: rig_usage.input_tokens,
            output_tokens: rig_usage.output_tokens,
            reasoning_tokens: rig_usage.reasoning_tokens,
            cached_input_tokens: rig_usage.cached_input_tokens,
        };
        outcome.duration_ms = start.elapsed().as_millis() as u64;
        tracing::debug!(
            turn = %node_id.raw(),
            model = model_ref,
            duration_ms = outcome.duration_ms,
            input = outcome.usage.input_tokens,
            output = outcome.usage.output_tokens,
            cached = outcome.usage.cached_input_tokens,
            reasoning = outcome.usage.reasoning_tokens,
            tool_calls = outcome.tool_calls.len(),
            error = outcome.error.as_deref().unwrap_or(""),
            cancelled = outcome.cancelled,
            "llm call finished"
        );
        outcome
    }
}

/// The `CoreMessage` snapshot emitted as the turn's `TurnLine::Init` anchor:
/// the initial history as sent, with the final system prompt rendered as a
/// leading System message.
fn request_snapshot(history: &[CoreMessage], prompt: &str) -> Vec<CoreMessage> {
    let mut snapshot = Vec::with_capacity(history.len() + 1);
    snapshot.push(CoreMessage::System {
        content: prompt.to_string(),
    });
    snapshot.extend(history.iter().cloned());
    snapshot
}
