use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use futures::Stream;
use tokio_util::sync::CancellationToken;

use super::types::{
    AssistantMessage, AssistantPart, ModelCapabilities, ModelRef, ModelRequest,
    OpaqueProviderState, PartIndex, ProviderCompletion, ProviderError, ReasoningPart, ResponseInfo,
    StopReason, TextPart, ToolCall, ToolCallId, Usage,
};

pub type ProviderStream = Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>;
pub type ProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProviderStream, ProviderError>> + Send + 'a>>;

pub trait Provider: Send + Sync {
    fn capabilities(&self, model: &ModelRef) -> ModelCapabilities;

    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> ProviderFuture<'_>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    Text,
    Reasoning,
    ToolCall,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    ResponseStarted(ResponseInfo),
    TextDelta {
        part: PartIndex,
        delta: String,
    },
    ReasoningDelta {
        part: PartIndex,
        delta: String,
    },
    ToolCallStarted {
        part: PartIndex,
        id: ToolCallId,
        name: String,
    },
    ToolArgumentsDelta {
        part: PartIndex,
        delta: String,
    },
    PartState {
        part: PartIndex,
        kind: PartKind,
        state: OpaqueProviderState,
    },
    UsageUpdated(Usage),
    Completed(ProviderCompletion),
    Failed(ProviderError),
}

#[derive(Debug, Clone, PartialEq)]
pub enum AccumulatorOutcome {
    Pending,
    Completed(Box<AssistantMessage>),
    Failed(ProviderError),
}

#[derive(Debug)]
pub struct ResponseAccumulator {
    message_id: super::types::MessageId,
    response: Option<ResponseInfo>,
    parts: BTreeMap<PartIndex, DraftPart>,
    usage: Option<Usage>,
    terminal: bool,
}

impl ResponseAccumulator {
    pub fn new(message_id: impl Into<super::types::MessageId>) -> Self {
        Self {
            message_id: message_id.into(),
            response: None,
            parts: BTreeMap::new(),
            usage: None,
            terminal: false,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    pub fn push(
        &mut self,
        event: ProviderEvent,
    ) -> Result<AccumulatorOutcome, ResponseValidationError> {
        if self.terminal {
            return Err(ResponseValidationError::EventAfterTerminal);
        }

        match event {
            ProviderEvent::ResponseStarted(info) => {
                if self.response.is_some() {
                    return Err(ResponseValidationError::DuplicateResponseStart);
                }
                self.response = Some(info);
            }
            ProviderEvent::TextDelta { part, delta } => {
                self.require_started()?;
                match self.parts.entry(part).or_insert_with(DraftPart::text) {
                    DraftPart::Text { text, .. } => text.push_str(&delta),
                    other => return Err(part_conflict(part, PartKind::Text, other.kind())),
                }
            }
            ProviderEvent::ReasoningDelta { part, delta } => {
                self.require_started()?;
                match self.parts.entry(part).or_insert_with(DraftPart::reasoning) {
                    DraftPart::Reasoning { text, .. } => text.push_str(&delta),
                    other => return Err(part_conflict(part, PartKind::Reasoning, other.kind())),
                }
            }
            ProviderEvent::ToolCallStarted { part, id, name } => {
                self.require_started()?;
                if self.parts.contains_key(&part) {
                    return Err(ResponseValidationError::DuplicatePart(part));
                }
                if id.is_empty() {
                    return Err(ResponseValidationError::EmptyToolCallId(part));
                }
                if name.is_empty() {
                    return Err(ResponseValidationError::EmptyToolName(part));
                }
                self.parts.insert(
                    part,
                    DraftPart::ToolCall {
                        id,
                        name,
                        arguments: String::new(),
                        provider_state: None,
                    },
                );
            }
            ProviderEvent::ToolArgumentsDelta { part, delta } => {
                self.require_started()?;
                let Some(draft) = self.parts.get_mut(&part) else {
                    return Err(ResponseValidationError::UnknownPart(part));
                };
                match draft {
                    DraftPart::ToolCall { arguments, .. } => arguments.push_str(&delta),
                    other => {
                        return Err(part_conflict(part, PartKind::ToolCall, other.kind()));
                    }
                }
            }
            ProviderEvent::PartState { part, kind, state } => {
                self.require_started()?;
                self.validate_state_scope(&state)?;
                if kind == PartKind::ToolCall && !self.parts.contains_key(&part) {
                    return Err(ResponseValidationError::UnknownPart(part));
                }
                let draft = self.parts.entry(part).or_insert_with(|| match kind {
                    PartKind::Text => DraftPart::text(),
                    PartKind::Reasoning => DraftPart::reasoning(),
                    PartKind::ToolCall => unreachable!("tool-call presence checked above"),
                });
                if draft.kind() != kind {
                    return Err(part_conflict(part, kind, draft.kind()));
                }
                draft.set_provider_state(state);
            }
            ProviderEvent::UsageUpdated(usage) => {
                self.require_started()?;
                self.usage = Some(usage);
            }
            ProviderEvent::Completed(completion) => {
                self.terminal = true;
                return self
                    .complete(completion)
                    .map(Box::new)
                    .map(AccumulatorOutcome::Completed);
            }
            ProviderEvent::Failed(error) => {
                self.terminal = true;
                return Ok(AccumulatorOutcome::Failed(error));
            }
        }

        Ok(AccumulatorOutcome::Pending)
    }

    pub fn finish_eof(&mut self) -> Result<(), ResponseValidationError> {
        if self.terminal {
            return Ok(());
        }
        self.terminal = true;
        Err(ResponseValidationError::MissingTerminalEvent)
    }

    fn complete(
        &mut self,
        completion: ProviderCompletion,
    ) -> Result<AssistantMessage, ResponseValidationError> {
        let response = self
            .response
            .take()
            .ok_or(ResponseValidationError::MissingResponseStart)?;
        if self.message_id.is_empty() {
            return Err(ResponseValidationError::EmptyMessageId);
        }
        if let Some(state) = &completion.provider_state {
            self.validate_state_scope_for(&response, state)?;
        }

        let mut parts = Vec::with_capacity(self.parts.len());
        let mut tool_call_count = 0;
        let mut tool_call_ids = std::collections::HashSet::new();
        for (index, draft) in std::mem::take(&mut self.parts) {
            let part = match draft {
                DraftPart::Text {
                    text,
                    provider_state,
                } => AssistantPart::Text(TextPart {
                    text,
                    provider_state,
                }),
                DraftPart::Reasoning {
                    text,
                    provider_state,
                } => AssistantPart::Reasoning(ReasoningPart {
                    text: (!text.is_empty()).then_some(text),
                    provider_state,
                }),
                DraftPart::ToolCall {
                    id,
                    name,
                    arguments,
                    provider_state,
                } => {
                    if !tool_call_ids.insert(id.clone()) {
                        return Err(ResponseValidationError::DuplicateToolCallId(id));
                    }
                    let arguments = serde_json::from_str(&arguments).map_err(|source| {
                        ResponseValidationError::InvalidToolArguments {
                            part: index,
                            source,
                        }
                    })?;
                    tool_call_count += 1;
                    AssistantPart::ToolCall(ToolCall {
                        id,
                        name,
                        arguments,
                        provider_state,
                    })
                }
            };
            parts.push(part);
        }

        match completion.stop_reason {
            StopReason::ToolUse if tool_call_count == 0 => {
                return Err(ResponseValidationError::ToolUseWithoutCalls);
            }
            StopReason::ToolUse => {}
            _ if tool_call_count > 0 => {
                return Err(ResponseValidationError::ToolCallsWithoutToolUseStop);
            }
            _ => {}
        }

        Ok(AssistantMessage {
            id: self.message_id.clone(),
            parts,
            stop_reason: completion.stop_reason,
            usage: completion.usage.or_else(|| self.usage.take()),
            provenance: response.provenance,
            provider_state: completion.provider_state,
        })
    }

    fn require_started(&self) -> Result<(), ResponseValidationError> {
        if self.response.is_none() {
            return Err(ResponseValidationError::EventBeforeResponseStart);
        }
        Ok(())
    }

    fn validate_state_scope(
        &self,
        state: &OpaqueProviderState,
    ) -> Result<(), ResponseValidationError> {
        let response = self
            .response
            .as_ref()
            .ok_or(ResponseValidationError::EventBeforeResponseStart)?;
        self.validate_state_scope_for(response, state)
    }

    fn validate_state_scope_for(
        &self,
        response: &ResponseInfo,
        state: &OpaqueProviderState,
    ) -> Result<(), ResponseValidationError> {
        let model = &response.provenance.requested;
        if state.provider != model.provider || state.api_family != model.api_family {
            return Err(ResponseValidationError::ProviderStateScopeMismatch);
        }
        Ok(())
    }
}

#[derive(Debug)]
enum DraftPart {
    Text {
        text: String,
        provider_state: Option<OpaqueProviderState>,
    },
    Reasoning {
        text: String,
        provider_state: Option<OpaqueProviderState>,
    },
    ToolCall {
        id: ToolCallId,
        name: String,
        arguments: String,
        provider_state: Option<OpaqueProviderState>,
    },
}

impl DraftPart {
    fn text() -> Self {
        Self::Text {
            text: String::new(),
            provider_state: None,
        }
    }

    fn reasoning() -> Self {
        Self::Reasoning {
            text: String::new(),
            provider_state: None,
        }
    }

    fn kind(&self) -> PartKind {
        match self {
            Self::Text { .. } => PartKind::Text,
            Self::Reasoning { .. } => PartKind::Reasoning,
            Self::ToolCall { .. } => PartKind::ToolCall,
        }
    }

    fn set_provider_state(&mut self, state: OpaqueProviderState) {
        match self {
            Self::Text { provider_state, .. }
            | Self::Reasoning { provider_state, .. }
            | Self::ToolCall { provider_state, .. } => *provider_state = Some(state),
        }
    }
}

fn part_conflict(part: PartIndex, expected: PartKind, actual: PartKind) -> ResponseValidationError {
    ResponseValidationError::PartKindConflict {
        part,
        expected,
        actual,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResponseValidationError {
    #[error("provider event arrived after the terminal event")]
    EventAfterTerminal,
    #[error("provider response started more than once")]
    DuplicateResponseStart,
    #[error("provider event arrived before response start")]
    EventBeforeResponseStart,
    #[error("provider stream completed without response start")]
    MissingResponseStart,
    #[error("assistant message id cannot be empty")]
    EmptyMessageId,
    #[error("provider stream ended without a terminal event")]
    MissingTerminalEvent,
    #[error("response part {0:?} was started more than once")]
    DuplicatePart(PartIndex),
    #[error("response part {0:?} does not exist")]
    UnknownPart(PartIndex),
    #[error("response part {part:?} expected {expected:?}, got {actual:?}")]
    PartKindConflict {
        part: PartIndex,
        expected: PartKind,
        actual: PartKind,
    },
    #[error("tool call at part {0:?} has an empty id")]
    EmptyToolCallId(PartIndex),
    #[error("tool call at part {0:?} has an empty name")]
    EmptyToolName(PartIndex),
    #[error("tool call id appears more than once: {0}")]
    DuplicateToolCallId(ToolCallId),
    #[error("tool call arguments at part {part:?} are not valid JSON: {source}")]
    InvalidToolArguments {
        part: PartIndex,
        #[source]
        source: serde_json::Error,
    },
    #[error("provider state scope does not match the response provider")]
    ProviderStateScopeMismatch,
    #[error("tool-use stop reason requires at least one tool call")]
    ToolUseWithoutCalls,
    #[error("a response containing tool calls must use the tool-use stop reason")]
    ToolCallsWithoutToolUseStop,
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use futures::{StreamExt, stream};

    use super::*;

    #[derive(Debug, Clone)]
    pub enum Script {
        Events(Vec<ProviderEvent>),
        EstablishmentFailure(ProviderError),
    }

    pub struct ScriptedProvider {
        scripts: Mutex<VecDeque<Script>>,
        requests: Mutex<Vec<ModelRequest>>,
        capabilities: ModelCapabilities,
    }

    impl ScriptedProvider {
        pub fn new(scripts: impl IntoIterator<Item = Script>) -> Self {
            Self {
                scripts: Mutex::new(scripts.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
                capabilities: ModelCapabilities::default(),
            }
        }

        pub fn requests(&self) -> Vec<ModelRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Provider for ScriptedProvider {
        fn capabilities(&self, _model: &ModelRef) -> ModelCapabilities {
            self.capabilities.clone()
        }

        fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> ProviderFuture<'_> {
            self.requests.lock().unwrap().push(request);
            let script = self.scripts.lock().unwrap().pop_front();
            Box::pin(async move {
                if cancel.is_cancelled() {
                    return Err(ProviderError {
                        kind: super::super::types::ProviderErrorKind::Cancelled,
                        message: "cancelled".to_string(),
                        retry: super::super::types::RetryHint::Never,
                    });
                }
                match script {
                    Some(Script::Events(events)) => Ok(stream::iter(events).boxed()),
                    Some(Script::EstablishmentFailure(error)) => Err(error),
                    None => Err(ProviderError::protocol("scripted provider is exhausted")),
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use serde_json::json;

    use super::test_support::{Script, ScriptedProvider};
    use super::*;
    use crate::agent::types::{
        ApiFamily, AttemptId, ConversationRevision, GenerationOptions, InstructionSet, MessageId,
        ProviderErrorKind, ProviderId, ResponseProvenance, RetryHint, StepId, TurnId,
    };

    fn model() -> ModelRef {
        ModelRef {
            provider: ProviderId::new("fake"),
            api_family: ApiFamily::new("test-api"),
            model: "test-model".to_owned(),
        }
    }

    fn started() -> ProviderEvent {
        ProviderEvent::ResponseStarted(ResponseInfo {
            provenance: ResponseProvenance {
                requested: model(),
                response_model: Some("resolved-model".to_owned()),
                response_id: Some("response-1".to_owned()),
            },
        })
    }

    fn completed(stop_reason: StopReason) -> ProviderEvent {
        ProviderEvent::Completed(ProviderCompletion {
            stop_reason,
            usage: None,
            provider_state: None,
        })
    }

    fn request() -> ModelRequest {
        ModelRequest {
            turn_id: TurnId::new("turn-1"),
            step_id: StepId::new("step-1"),
            attempt_id: AttemptId::new("attempt-1"),
            conversation_revision: ConversationRevision(3),
            instructions: InstructionSet::new("system"),
            messages: Vec::new(),
            tools: Vec::new(),
            model: model(),
            options: GenerationOptions::default(),
        }
    }

    #[test]
    fn commits_text_only_after_a_valid_terminal_event() {
        let mut accumulator = ResponseAccumulator::new("message-1");
        assert_eq!(
            accumulator.push(started()).unwrap(),
            AccumulatorOutcome::Pending
        );
        assert_eq!(
            accumulator
                .push(ProviderEvent::TextDelta {
                    part: PartIndex(0),
                    delta: "hello".to_owned(),
                })
                .unwrap(),
            AccumulatorOutcome::Pending
        );

        let AccumulatorOutcome::Completed(message) =
            accumulator.push(completed(StopReason::EndTurn)).unwrap()
        else {
            panic!("expected completed message");
        };

        assert_eq!(message.id, MessageId::new("message-1"));
        assert_eq!(message.stop_reason, StopReason::EndTurn);
        assert_eq!(
            message.parts,
            vec![AssistantPart::Text(TextPart {
                text: "hello".to_owned(),
                provider_state: None,
            })]
        );
        assert!(accumulator.is_terminal());
    }

    #[test]
    fn assembles_fragmented_tool_arguments_as_json() {
        let mut accumulator = ResponseAccumulator::new("message-1");
        accumulator.push(started()).unwrap();
        accumulator
            .push(ProviderEvent::ToolCallStarted {
                part: PartIndex(0),
                id: ToolCallId::new("call-1"),
                name: "read".to_owned(),
            })
            .unwrap();
        accumulator
            .push(ProviderEvent::ToolArgumentsDelta {
                part: PartIndex(0),
                delta: "{\"path\":".to_owned(),
            })
            .unwrap();
        accumulator
            .push(ProviderEvent::ToolArgumentsDelta {
                part: PartIndex(0),
                delta: "\"README.md\"}".to_owned(),
            })
            .unwrap();

        let AccumulatorOutcome::Completed(message) =
            accumulator.push(completed(StopReason::ToolUse)).unwrap()
        else {
            panic!("expected completed message");
        };

        let AssistantPart::ToolCall(call) = &message.parts[0] else {
            panic!("expected tool call");
        };
        assert_eq!(call.id, ToolCallId::new("call-1"));
        assert_eq!(call.arguments, json!({ "path": "README.md" }));
    }

    #[test]
    fn invalid_tool_arguments_fail_completion_without_a_committed_message() {
        let mut accumulator = ResponseAccumulator::new("message-1");
        accumulator.push(started()).unwrap();
        accumulator
            .push(ProviderEvent::ToolCallStarted {
                part: PartIndex(0),
                id: ToolCallId::new("call-1"),
                name: "read".to_owned(),
            })
            .unwrap();
        accumulator
            .push(ProviderEvent::ToolArgumentsDelta {
                part: PartIndex(0),
                delta: "not-json".to_owned(),
            })
            .unwrap();

        assert!(matches!(
            accumulator.push(completed(StopReason::ToolUse)),
            Err(ResponseValidationError::InvalidToolArguments { .. })
        ));
        assert!(accumulator.is_terminal());
    }

    #[test]
    fn provider_failure_is_terminal_but_not_an_assistant_message() {
        let mut accumulator = ResponseAccumulator::new("message-1");
        accumulator.push(started()).unwrap();
        accumulator
            .push(ProviderEvent::TextDelta {
                part: PartIndex(0),
                delta: "partial".to_owned(),
            })
            .unwrap();
        let failure = ProviderError {
            kind: ProviderErrorKind::Transport,
            message: "connection reset".to_owned(),
            retry: RetryHint::Retryable { after: None },
        };

        assert_eq!(
            accumulator
                .push(ProviderEvent::Failed(failure.clone()))
                .unwrap(),
            AccumulatorOutcome::Failed(failure)
        );
        assert!(accumulator.is_terminal());
    }

    #[test]
    fn eof_without_terminal_event_is_a_protocol_failure() {
        let mut accumulator = ResponseAccumulator::new("message-1");
        accumulator.push(started()).unwrap();

        assert!(matches!(
            accumulator.finish_eof(),
            Err(ResponseValidationError::MissingTerminalEvent)
        ));
        assert!(accumulator.is_terminal());
    }

    #[tokio::test]
    async fn scripted_provider_records_request_and_replays_events() {
        let provider = ScriptedProvider::new([Script::Events(vec![
            started(),
            completed(StopReason::EndTurn),
        ])]);
        let expected_request = request();

        let events: Vec<_> = provider
            .stream(expected_request.clone(), CancellationToken::new())
            .await
            .unwrap()
            .collect()
            .await;

        assert_eq!(events.len(), 2);
        assert_eq!(provider.requests(), vec![expected_request]);
    }

    #[tokio::test]
    async fn scripted_provider_can_fail_before_stream_establishment() {
        let failure = ProviderError {
            kind: ProviderErrorKind::Authentication,
            message: "invalid credentials".to_owned(),
            retry: RetryHint::Never,
        };
        let provider = ScriptedProvider::new([Script::EstablishmentFailure(failure.clone())]);

        let error = match provider.stream(request(), CancellationToken::new()).await {
            Ok(_) => panic!("expected establishment failure"),
            Err(error) => error,
        };

        assert_eq!(error, failure);
    }
}
