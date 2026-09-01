//! `run_turn`: the agent loop. Streams LLM calls, executes bash tool calls,
//! records every step, and returns a turn `Node` for the server to commit.
//!
//! Cancellation and provider failures still produce a node (outcome
//! `Cancelled` / `Failed`, steps so far preserved); only parameter-level
//! problems (empty history) surface as `Err`.

use futures::StreamExt;
use rig_core::completion::CompletionModel;
use rig_core::streaming::StreamedAssistantContent;
use rua_core::events::TurnEvent;
use rua_core::id::{CursorId, NodeId};
use rua_core::message::{CoreMessage, CoreToolCall};
use rua_core::node::{Node, NodeKind, Outcome, Step, Usage};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::client::Engine;
use crate::error::{Error, Result};
use crate::message::history_to_rig;
use crate::tools::BashTool;

/// Maximum number of tool-call rounds inside one turn.
pub const MAX_TOOL_ROUNDS: usize = 32;

/// Preview length (chars) for `ToolExecFinished` events.
const OUTPUT_PREVIEW_CHARS: usize = 500;

pub struct TurnParams {
    pub cursor_id: CursorId,
    /// Pre-allocated landing node id.
    pub node_id: NodeId,
    /// The cursor's current tip.
    pub parent: Option<NodeId>,
    pub context_refs: Vec<NodeId>,
    pub actor: String,
    pub model: String,
    /// Assembled history (`rua_core::assemble` output); must be non-empty.
    pub history: Vec<CoreMessage>,
    pub system_prompt: Option<String>,
    /// spawn_turn 递归深度（0 = 用户发起）。达到 MAX_SPAWN_DEPTH 后不再
    /// 注册 spawn_turn/inspect 工具。
    pub depth: usize,
    /// 本次发送的工具列表覆盖（None = 全部可用工具）。改工具列表会改变
    /// 请求前缀 → 前缀缓存失效（开发测试时这正是目的）。
    pub tools: Option<Vec<String>>,
}

/// What one streamed LLM call produced.
struct CallOutcome {
    text: String,
    reasoning: String,
    tool_calls: Vec<CoreToolCall>,
    usage: Usage,
    cancelled: bool,
    error: Option<String>,
}

impl Engine {
    pub async fn run_turn(
        &self,
        params: TurnParams,
        events: UnboundedSender<TurnEvent>,
        cancel: CancellationToken,
    ) -> Result<Node> {
        let TurnParams {
            cursor_id,
            node_id,
            parent,
            context_refs,
            actor,
            model,
            mut history,
            system_prompt,
            depth,
            tools,
        } = params;
        if history.is_empty() {
            return Err(Error::EmptyHistory);
        }

        let send = |event: TurnEvent| {
            let _ = events.send(event);
        };
        send(TurnEvent::Started { cursor_id, node_id });

        let mut steps: Vec<Step> = Vec::new();
        let mut usage = Usage::default();
        let mut tool_rounds = 0usize;

        let outcome = loop {
            let snapshot = request_snapshot(&history, system_prompt.as_deref());
            let call = self
                .stream_call(
                    &model,
                    &history,
                    system_prompt.as_deref(),
                    cursor_id,
                    node_id,
                    depth,
                    tools.as_deref(),
                    &send,
                    &cancel,
                )
                .await;

            usage.add_assign(&call.usage);
            steps.push(Step::LlmCall {
                request: snapshot,
                response_text: call.text.clone(),
                tool_calls: call.tool_calls.clone(),
                reasoning: (!call.reasoning.is_empty()).then_some(call.reasoning.clone()),
                usage: call.usage,
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
                    node_id,
                    call_id: call_tool.id.clone(),
                    name: call_tool.name.clone(),
                    args: call_tool.args.clone(),
                });
                let start = std::time::Instant::now();
                let output = match call_tool.name.as_str() {
                    "bash" => self.bash.execute(&call_tool.args, &cancel).await,
                    "spawn_turn" => match self.spawner.get() {
                        Some(spawner) => {
                            crate::spawn::execute_spawn(spawner, &call_tool.args, node_id, depth)
                                .await
                        }
                        None => "error: spawn_turn is not available".to_string(),
                    },
                    "inspect" => match self.spawner.get() {
                        Some(spawner) => {
                            crate::spawn::execute_inspect(spawner, &call_tool.args, &cancel).await
                        }
                        None => "error: inspect is not available".to_string(),
                    },
                    other => format!("error: unknown tool: {other}"),
                };
                let duration_ms = start.elapsed().as_millis() as u64;
                send(TurnEvent::ToolExecFinished {
                    cursor_id,
                    node_id,
                    call_id: call_tool.id.clone(),
                    output_preview: output.chars().take(OUTPUT_PREVIEW_CHARS).collect(),
                    duration_ms,
                });
                steps.push(Step::ToolExec {
                    call_id: call_tool.id.clone(),
                    name: call_tool.name.clone(),
                    args: call_tool.args.clone(),
                    output: output.clone(),
                    duration_ms,
                });
                history.push(CoreMessage::ToolResult {
                    call_id: call_tool.id.clone(),
                    name: call_tool.name.clone(),
                    output,
                });
            }
        };

        Ok(Node {
            id: node_id,
            parent,
            context_refs,
            created_by: None,
            created_at: Node::now_millis(),
            kind: NodeKind::Turn {
                steps,
                outcome,
                actor,
                model,
                usage,
            },
        })
    }

    /// One streaming LLM call over the current history. `model_ref` is the
    /// per-turn model override (`"provider/model"` or bare = default
    /// provider); it is resolved per call so per-send overrides actually
    /// take effect (and failures surface as a Failed turn, not a panic).
    async fn stream_call(
        &self,
        model_ref: &str,
        history: &[CoreMessage],
        system_prompt: Option<&str>,
        cursor_id: CursorId,
        node_id: NodeId,
        depth: usize,
        tools_override: Option<&[String]>,
        send: &dyn Fn(TurnEvent),
        cancel: &CancellationToken,
    ) -> CallOutcome {
        let (model, additional_params) = match self.model_for(model_ref) {
            Ok(x) => x,
            Err(e) => {
                return CallOutcome {
                    text: String::new(),
                    reasoning: String::new(),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    cancelled: false,
                    error: Some(e.to_string()),
                };
            }
        };
        let mut rig_history = history_to_rig(history);
        // The builder's `prompt` is appended as the last message; feed it the
        // tail and put everything before it into `.messages()`.
        let prompt = rig_history.pop().expect("history checked non-empty");

        // 可用工具全集 → 应用本次发送的工具覆盖（Some = 只保留列表里的）。
        let allowed = |name: &str| match tools_override {
            None => true,
            Some(list) => list.iter().any(|t| t == name),
        };
        let mut tool_defs = Vec::new();
        if allowed("bash") {
            tool_defs.push(BashTool::definition());
        }
        // 图生长工具：有 spawner、未达递归上限、且未被工具覆盖关掉才注册。
        if self.spawner.get().is_some() && depth < crate::spawn::MAX_SPAWN_DEPTH {
            if allowed("spawn_turn") {
                tool_defs.push(crate::spawn::spawn_turn_definition(depth));
            }
            if allowed("inspect") {
                tool_defs.push(crate::spawn::inspect_definition());
            }
        }
        let mut builder = model
            .completion_request(prompt)
            .messages(rig_history)
            .tools(tool_defs);
        if let Some(system_prompt) = system_prompt {
            builder = builder.preamble(system_prompt.to_string());
        }
        if let Some(additional_params) = &additional_params {
            builder = builder.additional_params(additional_params.clone());
        }

        let mut stream = match builder.stream().await {
            Ok(stream) => stream,
            Err(e) => {
                return CallOutcome {
                    text: String::new(),
                    reasoning: String::new(),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    cancelled: false,
                    error: Some(e.to_string()),
                };
            }
        };

        let mut outcome = CallOutcome {
            text: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            usage: Usage::default(),
            cancelled: false,
            error: None,
        };

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
                        outcome.error = Some(e.to_string());
                        break;
                    }
                },
            };
            match item {
                StreamedAssistantContent::Text(text) => {
                    outcome.text.push_str(&text.text);
                    send(TurnEvent::TextDelta {
                        cursor_id,
                        node_id,
                        delta: text.text,
                    });
                }
                StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                    outcome.reasoning.push_str(&reasoning);
                    send(TurnEvent::ReasoningDelta {
                        cursor_id,
                        node_id,
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
                                node_id,
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
        outcome
    }
}

/// The `CoreMessage` snapshot recorded on a `Step::LlmCall`: the history as
/// sent, with the system prompt (if any) rendered as a leading System message.
fn request_snapshot(history: &[CoreMessage], system_prompt: Option<&str>) -> Vec<CoreMessage> {
    let mut snapshot = Vec::with_capacity(history.len() + 1);
    if let Some(system_prompt) = system_prompt {
        snapshot.push(CoreMessage::System {
            content: system_prompt.to_string(),
        });
    }
    snapshot.extend(history.iter().cloned());
    snapshot
}
