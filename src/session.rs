use futures::StreamExt;
use tokio::sync::mpsc;

use crate::app::event::UiEvent;
use crate::deepseek::{DeepSeekClient, Message, StreamEvent};
use crate::tools::ToolRegistry;

/// Manages the lifecycle of a chat turn, including the agent loop
/// (request → stream → optional tool calls → re-request).
#[derive(Clone)]
pub struct Session {
    client: DeepSeekClient,
    tx: mpsc::UnboundedSender<UiEvent>,
    tools: ToolRegistry,
}

impl Session {
    pub fn new(
        client: DeepSeekClient,
        tx: mpsc::UnboundedSender<UiEvent>,
        tools: ToolRegistry,
    ) -> Self {
        Self { client, tx, tools }
    }

    /// Start a chat turn. If the LLM requests tool calls, this will
    /// execute them and continue the conversation automatically.
    pub fn send_message(
        &self,
        messages: Vec<Message>,
    ) -> tokio::task::JoinHandle<()> {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let tools = self.tools.clone();

        tokio::spawn(async move {
            let mut messages = messages;

            let tool_schema_vec = tools.to_api_schema();
            let tool_schema = if tool_schema_vec.is_empty() {
                None
            } else {
                Some(tool_schema_vec.as_slice())
            };

            loop {
                let result = run_turn(
                    &client, &tx, &tools, &mut messages, tool_schema,
                ).await;

                match result {
                    Ok(TurnResult::Done) => break,
                    Ok(TurnResult::ToolCalls) => continue,
                    Err(e) => {
                        let _ = tx.send(UiEvent::StreamError(e));
                        break;
                    }
                }
            }
        })
    }
}

enum TurnResult {
    Done,
    ToolCalls,
}

async fn run_turn(
    client: &DeepSeekClient,
    tx: &mpsc::UnboundedSender<UiEvent>,
    tools: &ToolRegistry,
    messages: &mut Vec<Message>,
    tool_schema: Option<&[serde_json::Value]>,
) -> Result<TurnResult, String> {
    let mut stream = client
        .chat_stream(messages.clone(), tool_schema)
        .await
        .map_err(|e| e.to_string())?;

    let mut content = String::new();
    let mut reasoning_content = String::new();
    let mut pending_tool_calls: Vec<PendingToolCall> = vec![];

    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta(delta) => {
                content.push_str(&delta);
                let _ = tx.send(UiEvent::StreamDelta(delta));
            }
            StreamEvent::ReasoningDelta(delta) => {
                reasoning_content.push_str(&delta);
                let _ = tx.send(UiEvent::ReasoningDelta(delta));
            }
            StreamEvent::ToolCall { id, name, arguments } => {
                pending_tool_calls.push(PendingToolCall { id, name, arguments });
            }
            StreamEvent::Done => {
                if pending_tool_calls.is_empty() {
                    // Normal completion — no tools requested
                    let _ = tx.send(UiEvent::StreamDone);
                    return Ok(TurnResult::Done);
                }

                // Tools were requested. Finish the current stream UI,
                // then execute tools and continue the outer loop.
                let _ = tx.send(UiEvent::StreamDone);

                // Add assistant message (may have content + tool_calls + reasoning)
                let assistant_content = if content.is_empty() {
                    None
                } else {
                    Some(content.clone())
                };
                let assistant_reasoning = if reasoning_content.is_empty() {
                    None
                } else {
                    Some(reasoning_content.clone())
                };

                let api_tool_calls: Vec<crate::deepseek::ToolCall> = pending_tool_calls
                    .iter()
                    .map(|pt| crate::deepseek::ToolCall {
                        id: pt.id.clone(),
                        call_type: "function".to_string(),
                        function: crate::deepseek::ToolFunction {
                            name: pt.name.clone(),
                            arguments: pt.arguments.clone(),
                        },
                    })
                    .collect();

                messages.push(Message::assistant_with_tools(
                    assistant_content,
                    api_tool_calls,
                    assistant_reasoning,
                ));

                // Execute each tool
                for call in &pending_tool_calls {
                    // Notify UI
                    let _ = tx.send(UiEvent::ToolCall {
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    });

                    // Parse arguments
                    let params = serde_json::from_str::<serde_json::Value>(&call.arguments
                    )
                    .unwrap_or_else(|_| serde_json::json!({}));

                    // Execute
                    let result = match tools.execute(&call.name, params).await {
                        Ok(output) => output,
                        Err(e) => format!("Error: {}", e),
                    };

                    // Notify UI of result
                    let _ = tx.send(UiEvent::ToolResult {
                        name: call.name.clone(),
                        output: result.clone(),
                    });

                    // Add tool message to history
                    messages.push(Message::tool(
                        &call.id,
                        &result,
                    ));
                }

                // Continue outer loop (re-send with tool results)
                return Ok(TurnResult::ToolCalls);
            }
            StreamEvent::Error(e) => {
                let _ = tx.send(UiEvent::StreamError(e.clone()));
                return Err(e);
            }
        }
    }

    Ok(TurnResult::Done)
}

#[derive(Debug, Clone)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}
