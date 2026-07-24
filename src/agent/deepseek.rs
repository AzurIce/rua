use std::collections::BTreeMap;

use futures::StreamExt;
use reqwest::header::{self, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::config::DeepSeekConfig;

use super::provider::{Provider, ProviderEvent, ProviderFuture, ProviderStream};
use super::types::{
    AssistantPart, Message, ModelCapabilities, ModelRef, ModelRequest, PartIndex,
    ProviderCompletion, ProviderError, ProviderErrorKind, ResponseInfo, ResponseProvenance,
    RetryHint, StopReason, ToolDefinition, Usage,
};

#[derive(Clone)]
pub struct DeepSeekProvider {
    client: reqwest::Client,
    base_url: String,
}

impl DeepSeekProvider {
    pub fn new(config: &DeepSeekConfig) -> Result<Self, ProviderError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        let authorization =
            HeaderValue::from_str(&format!("Bearer {}", config.api_key)).map_err(|_| {
                ProviderError {
                    kind: ProviderErrorKind::Authentication,
                    message: "invalid API key".to_owned(),
                    retry: RetryHint::Never,
                }
            })?;
        headers.insert(header::AUTHORIZATION, authorization);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|error| ProviderError {
                kind: ProviderErrorKind::Transport,
                message: format!("failed to build HTTP client: {error}"),
                retry: RetryHint::Never,
            })?;
        Ok(Self {
            client,
            base_url: config.base_url.trim_end_matches('/').to_owned(),
        })
    }
}

impl Provider for DeepSeekProvider {
    fn capabilities(&self, _model: &ModelRef) -> ModelCapabilities {
        ModelCapabilities {
            tools: true,
            reasoning: true,
            image_input: false,
        }
    }

    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> ProviderFuture<'_> {
        let client = self.client.clone();
        let url = format!("{}/chat/completions", self.base_url);
        Box::pin(async move {
            let body = ChatRequest::from_request(&request);
            let response = tokio::select! {
                _ = cancel.cancelled() => return Err(cancelled_error()),
                result = client.post(&url).json(&body).send() => result,
            }
            .map_err(|error| ProviderError {
                kind: ProviderErrorKind::Transport,
                message: format!("failed to POST to provider: {error}"),
                retry: RetryHint::Retryable { after: None },
            })?;

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(http_error(status.as_u16(), &text));
            }

            let mut bytes = response.bytes_stream();
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let requested = request.model.clone();
            tokio::spawn(async move {
                let mut parser = SseParser::new(requested);
                loop {
                    let next = tokio::select! {
                        _ = cancel.cancelled() => {
                            let _ = tx.send(ProviderEvent::Failed(cancelled_error()));
                            return;
                        }
                        value = bytes.next() => value,
                    };
                    let Some(next) = next else {
                        if !parser.terminal {
                            let _ = tx.send(ProviderEvent::Failed(ProviderError::protocol(
                                "provider stream ended without a terminal event",
                            )));
                        }
                        return;
                    };
                    let chunk = match next {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            let _ = tx.send(ProviderEvent::Failed(ProviderError {
                                kind: ProviderErrorKind::Transport,
                                message: format!("provider stream error: {error}"),
                                retry: RetryHint::Retryable { after: None },
                            }));
                            return;
                        }
                    };
                    parser.buffer.push_str(&String::from_utf8_lossy(&chunk));
                    while let Some(pos) = parser.buffer.find('\n') {
                        let line = parser.buffer[..pos].trim_end_matches('\r').to_owned();
                        parser.buffer = parser.buffer[pos + 1..].to_owned();
                        if let Some(data) = line.strip_prefix("data: ") {
                            if let Err(error) = parser.consume(data, &tx) {
                                let _ = tx.send(ProviderEvent::Failed(error));
                                return;
                            }
                            if parser.terminal {
                                return;
                            }
                        }
                    }
                }
            });
            Ok(Box::pin(UnboundedReceiverStream::new(rx)) as ProviderStream)
        })
    }
}

fn cancelled_error() -> ProviderError {
    ProviderError {
        kind: ProviderErrorKind::Cancelled,
        message: "provider request cancelled".to_owned(),
        retry: RetryHint::Never,
    }
}

fn http_error(status: u16, body: &str) -> ProviderError {
    let (kind, retry) = match status {
        401 => (ProviderErrorKind::Authentication, RetryHint::Never),
        403 => (ProviderErrorKind::Authorization, RetryHint::Never),
        400..=499 if status == 429 => (
            ProviderErrorKind::RateLimit,
            RetryHint::Retryable { after: None },
        ),
        400..=499 => (ProviderErrorKind::InvalidRequest, RetryHint::Never),
        500..=599 => (
            ProviderErrorKind::Server,
            RetryHint::Retryable { after: None },
        ),
        _ => (ProviderErrorKind::Other, RetryHint::Unknown),
    };
    ProviderError {
        kind,
        message: format!(
            "provider HTTP {status}: {}",
            body.trim().chars().take(400).collect::<String>()
        ),
        retry,
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<WireMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
}

impl ChatRequest {
    fn from_request(request: &ModelRequest) -> Self {
        Self {
            model: request.model.model.clone(),
            messages: std::iter::once(WireMessage {
                role: "system".to_owned(),
                content: Some(request.instructions.text.clone()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            })
            .chain(request.messages.iter().map(WireMessage::from_canonical))
            .collect(),
            stream: true,
            tools: request
                .tools
                .iter()
                .map(WireTool::from_definition)
                .collect(),
            temperature: request.options.temperature,
            max_tokens: request.options.max_output_tokens,
        }
    }
}

#[derive(Debug, Serialize)]
struct WireTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction,
}

#[derive(Debug, Serialize)]
struct WireFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

impl WireTool {
    fn from_definition(definition: &ToolDefinition) -> Self {
        Self {
            kind: "function",
            function: WireFunction {
                name: definition.name.clone(),
                description: definition.description.clone(),
                parameters: definition.parameters.clone(),
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct WireMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
}

impl WireMessage {
    fn from_canonical(message: &Message) -> Self {
        match message {
            Message::User(message) => Self {
                role: "user".to_owned(),
                content: Some(
                    message
                        .content
                        .iter()
                        .map(|part| match part {
                            super::types::UserContent::Text { text } => text.as_str(),
                        })
                        .collect::<String>(),
                ),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            Message::Assistant(message) => {
                let content = message
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        AssistantPart::Text(part) => Some(part.text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                let reasoning_content = message
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        AssistantPart::Reasoning(part) => part.text.as_deref(),
                        _ => None,
                    })
                    .collect::<String>();
                let calls: Vec<_> = message.tool_calls().collect();
                let tool_calls = calls
                    .iter()
                    .map(|call| WireToolCall::from_canonical(call))
                    .collect();
                Self {
                    role: "assistant".to_owned(),
                    content: (!content.is_empty()).then_some(content),
                    tool_calls: (!calls.is_empty()).then_some(tool_calls),
                    tool_call_id: None,
                    reasoning_content: (!reasoning_content.is_empty()).then_some(reasoning_content),
                }
            }
            Message::ToolResult(message) => Self {
                role: "tool".to_owned(),
                content: Some(
                    message
                        .content
                        .iter()
                        .map(|part| match part {
                            super::types::ToolResultContent::Text { text } => text.as_str(),
                        })
                        .collect::<String>(),
                ),
                tool_calls: None,
                tool_call_id: Some(message.tool_call_id.to_string()),
                reasoning_content: None,
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireCallFunction,
}

#[derive(Debug, Serialize)]
struct WireCallFunction {
    name: String,
    arguments: String,
}

impl WireToolCall {
    fn from_canonical(call: &super::types::ToolCall) -> Self {
        Self {
            id: call.id.to_string(),
            kind: "function",
            function: WireCallFunction {
                name: call.name.clone(),
                arguments: call.arguments.to_string(),
            },
        }
    }
}

#[derive(Debug, Deserialize)]
struct StreamChunk {
    id: Option<String>,
    model: Option<String>,
    choices: Option<Vec<StreamChoice>>,
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: ChoiceDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ChoiceDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Deserialize, Default)]
struct ToolCallDelta {
    index: Option<u32>,
    id: Option<String>,
    #[serde(default)]
    function: FunctionDelta,
}

#[derive(Debug, Deserialize, Default)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

impl From<WireUsage> for Usage {
    fn from(usage: WireUsage) -> Self {
        Self {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            ..Self::default()
        }
    }
}

struct ToolAccumulator {
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

struct SseParser {
    buffer: String,
    requested: ModelRef,
    tools: BTreeMap<u32, ToolAccumulator>,
    terminal: bool,
    started: bool,
    response_id: Option<String>,
    response_model: Option<String>,
}

impl SseParser {
    fn new(requested: ModelRef) -> Self {
        Self {
            buffer: String::new(),
            requested,
            tools: BTreeMap::new(),
            terminal: false,
            started: false,
            response_id: None,
            response_model: None,
        }
    }

    fn consume(
        &mut self,
        data: &str,
        tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        if data == "[DONE]" {
            if !self.terminal {
                self.finish("stop", tx)?;
            }
            return Ok(());
        }
        let chunk: StreamChunk = serde_json::from_str(data).map_err(|error| ProviderError {
            kind: ProviderErrorKind::Protocol,
            message: format!("invalid provider SSE payload: {error}"),
            retry: RetryHint::Unknown,
        })?;
        self.response_id = self.response_id.clone().or(chunk.id);
        self.response_model = self.response_model.clone().or(chunk.model);
        self.ensure_started(tx);
        if let Some(usage) = chunk.usage {
            let _ = tx.send(ProviderEvent::UsageUpdated(usage.into()));
        }
        for choice in chunk.choices.unwrap_or_default() {
            if let Some(text) = choice.delta.content.filter(|text| !text.is_empty()) {
                let _ = tx.send(ProviderEvent::TextDelta {
                    part: PartIndex(0),
                    delta: text,
                });
            }
            if let Some(text) = choice
                .delta
                .reasoning_content
                .filter(|text| !text.is_empty())
            {
                let _ = tx.send(ProviderEvent::ReasoningDelta {
                    part: PartIndex(1),
                    delta: text,
                });
            }
            for delta in choice.delta.tool_calls {
                let index = delta.index.unwrap_or(0);
                let tool = self.tools.entry(index).or_insert_with(|| ToolAccumulator {
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                    started: false,
                });
                if let Some(id) = delta.id {
                    tool.id = id;
                }
                if let Some(name) = delta.function.name {
                    tool.name = name;
                }
                if let Some(arguments) = delta.function.arguments {
                    tool.arguments.push_str(&arguments);
                }
                if !tool.started && !tool.id.is_empty() && !tool.name.is_empty() {
                    let _ = tx.send(ProviderEvent::ToolCallStarted {
                        part: PartIndex(2 + index),
                        id: tool.id.clone().into(),
                        name: tool.name.clone(),
                    });
                    if !tool.arguments.is_empty() {
                        let _ = tx.send(ProviderEvent::ToolArgumentsDelta {
                            part: PartIndex(2 + index),
                            delta: std::mem::take(&mut tool.arguments),
                        });
                    }
                    tool.started = true;
                }
                if tool.started && !tool.arguments.is_empty() {
                    let _ = tx.send(ProviderEvent::ToolArgumentsDelta {
                        part: PartIndex(2 + index),
                        delta: std::mem::take(&mut tool.arguments),
                    });
                }
            }
            if let Some(reason) = choice.finish_reason {
                self.finish(&reason, tx)?;
                break;
            }
        }
        Ok(())
    }

    fn finish(
        &mut self,
        reason: &str,
        tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        self.ensure_started(tx);
        for (index, tool) in &mut self.tools {
            if !tool.started {
                return Err(ProviderError::protocol(format!(
                    "tool call {index} did not provide an id and name"
                )));
            }
            if !tool.arguments.is_empty() {
                let _ = tx.send(ProviderEvent::ToolArgumentsDelta {
                    part: PartIndex(2 + *index),
                    delta: std::mem::take(&mut tool.arguments),
                });
            }
        }
        let stop_reason = match reason {
            "tool_calls" => StopReason::ToolUse,
            "length" => StopReason::Length,
            "content_filter" => StopReason::ContentFilter,
            "stop" | "" => StopReason::EndTurn,
            other => StopReason::Other(other.to_owned()),
        };
        let _ = tx.send(ProviderEvent::Completed(ProviderCompletion {
            stop_reason,
            usage: None,
            provider_state: None,
        }));
        self.terminal = true;
        Ok(())
    }

    fn ensure_started(&mut self, tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        let _ = tx.send(ProviderEvent::ResponseStarted(ResponseInfo {
            provenance: ResponseProvenance {
                requested: self.requested.clone(),
                response_model: self.response_model.clone(),
                response_id: self.response_id.clone(),
            },
        }));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::types::{
        ApiFamily, AttemptId, ConversationRevision, GenerationOptions, InstructionSet, MessageId,
        ModelRef, ProviderId, StepId, TurnId,
    };
    use super::*;

    #[test]
    fn wire_request_preserves_tool_call_relationship() {
        let request = ModelRequest {
            turn_id: TurnId::new("t"),
            step_id: StepId::new("s"),
            attempt_id: AttemptId::new("a"),
            conversation_revision: ConversationRevision(1),
            instructions: InstructionSet::new("system"),
            messages: vec![Message::User(super::super::types::UserMessage {
                id: MessageId::new("m"),
                content: vec![super::super::types::UserContent::Text { text: "hi".into() }],
            })],
            tools: vec![],
            model: ModelRef {
                provider: ProviderId::new("deepseek"),
                api_family: ApiFamily::new("openai-chat"),
                model: "test".into(),
            },
            options: GenerationOptions::default(),
        };
        let body = ChatRequest::from_request(&request);
        assert_eq!(body.messages[0].role, "system");
        assert_eq!(body.messages[1].role, "user");
        assert_eq!(json!(body.messages[1].content), json!("hi"));
    }
}
