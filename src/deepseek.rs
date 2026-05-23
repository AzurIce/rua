use std::collections::HashMap;
use std::pin::Pin;

use color_eyre::{eyre::Context, Result};
use futures::{Stream, StreamExt};
use reqwest::header::{self, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::config::DeepSeekConfig;
use crate::model::{ChatEntry, Role};

// ===================================================================
// Wire-format types
// ===================================================================

/// A single chat message (API wire format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// DeepSeek reasoning models require passing reasoning_content back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }
    }

    pub fn assistant(content: Option<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }
    }

    pub fn assistant_with_tools(
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
        reasoning_content: Option<String>,
    ) -> Self {
        Self {
            role: "assistant".to_string(),
            content,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
            reasoning_content,
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            reasoning_content: None,
        }
    }
}

impl From<ChatEntry> for Message {
    fn from(entry: ChatEntry) -> Self {
        Self {
            role: match entry.role {
                Role::User => "user".to_string(),
                Role::Assistant => "assistant".to_string(),
                Role::System => "system".to_string(),
                Role::Tool => "tool".to_string(),
            },
            content: Some(entry.text),
            tool_calls: None,
            tool_call_id: entry.tool_call_id,
            reasoning_content: entry.reasoning_content,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: String,
}

// ===================================================================
// Request / Response types
// ===================================================================

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct ChoiceDelta {
    pub content: Option<String>,
    pub role: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct ToolCallDelta {
    pub index: Option<u32>,
    pub id: Option<String>,
    #[serde(default)]
    pub function: FunctionDelta,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct FunctionDelta {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StreamChoice {
    pub delta: ChoiceDelta,
    pub index: Option<u32>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StreamChunk {
    pub id: Option<String>,
    pub object: Option<String>,
    pub created: Option<u64>,
    pub model: Option<String>,
    pub choices: Option<Vec<StreamChoice>>,
}

// ===================================================================
// Stream events
// ===================================================================

#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    /// Reasoning content from DeepSeek reasoning models (not displayed, but must be preserved).
    ReasoningDelta(String),
    ToolCall { id: String, name: String, arguments: String },
    Done,
    Error(String),
}

// ===================================================================
// Accumulator for partial tool calls
// ===================================================================

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

// ===================================================================
// Client
// ===================================================================

#[derive(Clone)]
pub struct DeepSeekClient {
    client: reqwest::Client,
    config: DeepSeekConfig,
}

impl DeepSeekClient {
    pub fn new(config: DeepSeekConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", config.api_key))
                .context("invalid api_key")?,
        );

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self { client, config })
    }

    /// Send a chat request and return a stream of events.
    pub async fn chat_stream(
        &self,
        messages: Vec<Message>,
        tools: Option<&[serde_json::Value]>,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>> {
        let body = ChatRequest {
            model: self.config.model.clone(),
            messages,
            stream: true,
            tools: tools.map(|t| t.to_vec()),
        };

        let url = format!("{}/chat/completions", self.config.base_url);
        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to POST to {}", url))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            color_eyre::eyre::bail!("DeepSeek API error {}: {}", status, text);
        }

        let mut byte_stream = response.bytes_stream();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();

        tokio::spawn(async move {
            let mut buffer = String::new();
            let mut tool_acc: HashMap<u32, ToolCallAccumulator> = HashMap::new();

            while let Some(result) = byte_stream.next().await {
                let chunk = match result {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Error(format!("stream error: {}", e)));
                        break;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].trim_end_matches('\r').to_string();
                    buffer = buffer[pos + 1..].to_string();

                    if !line.starts_with("data: ") {
                        continue;
                    }
                    let data = &line[6..];
                    if data == "[DONE]" {
                        let _ = tx.send(StreamEvent::Done);
                        continue;
                    }

                    match serde_json::from_str::<StreamChunk>(data) {
                        Ok(chunk) => {
                            if let Some(choices) = chunk.choices {
                                for choice in choices {
                                    // Text delta
                                    if let Some(content) = choice.delta.content {
                                        if !content.is_empty() {
                                            let _ = tx.send(StreamEvent::TextDelta(content));
                                        }
                                    }

                                    // Reasoning content (DeepSeek reasoning models)
                                    if let Some(rc) = choice.delta.reasoning_content {
                                        if !rc.is_empty() {
                                            let _ = tx.send(StreamEvent::ReasoningDelta(rc));
                                        }
                                    }

                                    // Tool call deltas (accumulate by index)
                                    for tc in choice.delta.tool_calls {
                                        let idx = tc.index.unwrap_or(0);
                                        let acc = tool_acc.entry(idx).or_default();
                                        if let Some(id) = tc.id {
                                            acc.id = id;
                                        }
                                        if let Some(name) = tc.function.name {
                                            acc.name = name;
                                        }
                                        if let Some(args) = tc.function.arguments {
                                            acc.arguments.push_str(&args);
                                        }
                                    }

                                    // Finish reason
                                    if let Some(reason) = choice.finish_reason {
                                        if reason == "tool_calls" {
                                            // Emit accumulated tool calls
                                            let mut indices: Vec<_> = tool_acc.keys().copied().collect();
                                            indices.sort();
                                            for idx in indices {
                                                if let Some(acc) = tool_acc.remove(&idx) {
                                                    let _ = tx.send(StreamEvent::ToolCall {
                                                        id: acc.id,
                                                        name: acc.name,
                                                        arguments: acc.arguments,
                                                    });
                                                }
                                            }
                                        }
                                        let _ = tx.send(StreamEvent::Done);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(StreamEvent::Error(format!(
                                "JSON parse error: {} (data: {})",
                                e,
                                &data[..data.len().min(80)]
                            )));
                        }
                    }
                }
            }
        });

        Ok(Box::pin(UnboundedReceiverStream::new(rx)))
    }
}
