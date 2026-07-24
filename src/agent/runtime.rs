use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::conversation::Conversation;
use super::provider::{Provider, ProviderEvent, ResponseAccumulator};
use super::tools::{ToolExecutor, ToolOutcome};
use super::types::{
    ApiFamily, AssistantMessage, ConversationRevision, Message, MessageId, ModelRef, ModelRequest,
    ProviderError, ProviderErrorKind, ProviderId, RetryHint, StepId, StopReason, ToolCall,
    ToolResultContent, ToolResultMessage, TurnId, UserContent, UserMessage,
};

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeEvent {
    UserCommitted {
        turn_id: TurnId,
        text: String,
    },
    ModelStepStarted {
        turn_id: TurnId,
        step_id: StepId,
        attempt_id: super::types::AttemptId,
    },
    TextDelta {
        turn_id: TurnId,
        delta: String,
    },
    ReasoningDelta {
        turn_id: TurnId,
        delta: String,
    },
    ModelStepRetrying {
        turn_id: TurnId,
        step_id: StepId,
        attempt: u32,
        error: String,
    },
    AssistantCommitted {
        turn_id: TurnId,
        message: Box<AssistantMessage>,
    },
    ToolStarted {
        turn_id: TurnId,
        call_id: super::types::ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
    ToolCompleted {
        turn_id: TurnId,
        call_id: super::types::ToolCallId,
        name: String,
        content: String,
    },
    ToolFailed {
        turn_id: TurnId,
        call_id: super::types::ToolCallId,
        name: String,
        message: String,
        outcome_unknown: bool,
    },
    TurnCompleted {
        turn_id: TurnId,
    },
    TurnFailed {
        turn_id: TurnId,
        error: String,
        recoverable: bool,
    },
    TurnCancelled {
        turn_id: TurnId,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("runtime is already executing a turn")]
    Busy,
    #[error("runtime has no turn that can be resumed")]
    NoResumableTurn,
    #[error("turn is blocked: {0}")]
    Blocked(String),
    #[error("conversation error: {0}")]
    Conversation(String),
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
}

struct ActiveTurn {
    turn_id: TurnId,
    next_step: u32,
    blocked: Option<String>,
}

struct RuntimeState {
    conversation: Conversation,
    active: Option<ActiveTurn>,
    next_message: u64,
    next_turn: u64,
}

struct ModelStepContext {
    turn_id: TurnId,
    step_id: StepId,
    revision: ConversationRevision,
    messages: Vec<Message>,
    instructions: super::types::InstructionSet,
}

pub struct AgentRuntime {
    provider: Arc<dyn Provider>,
    tools: Arc<dyn ToolExecutor>,
    model: ModelRef,
    state: Mutex<RuntimeState>,
    max_attempts: u32,
    next_attempt: AtomicU64,
}

impl AgentRuntime {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<dyn ToolExecutor>,
        instructions: impl Into<String>,
        model: ModelRef,
    ) -> Self {
        Self {
            provider,
            tools,
            model,
            state: Mutex::new(RuntimeState {
                conversation: Conversation::new(super::types::InstructionSet::new(instructions)),
                active: None,
                next_message: 0,
                next_turn: 0,
            }),
            max_attempts: 2,
            next_attempt: AtomicU64::new(0),
        }
    }

    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    pub async fn conversation_snapshot(&self) -> Conversation {
        self.state.lock().await.conversation.clone()
    }

    pub async fn run_user_turn(
        &self,
        text: String,
        emit: &tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
        cancel: CancellationToken,
    ) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().await;
        if state.active.is_some() {
            return Err(RuntimeError::Busy);
        }
        if text.trim().is_empty() {
            return Err(RuntimeError::Conversation(
                "user message is empty".to_owned(),
            ));
        }
        let turn_id: TurnId = next_id("turn", &mut state.next_turn).into();
        let message_id = next_id("message", &mut state.next_message).into();
        state
            .conversation
            .append(Message::User(UserMessage {
                id: message_id,
                content: vec![UserContent::Text { text: text.clone() }],
            }))
            .map_err(|error| RuntimeError::Conversation(error.to_string()))?;
        state.active = Some(ActiveTurn {
            turn_id: turn_id.clone(),
            next_step: 0,
            blocked: None,
        });
        let _ = emit.send(RuntimeEvent::UserCommitted { turn_id, text });
        self.drive_locked(&mut state, emit, cancel).await
    }

    pub async fn resume_turn(
        &self,
        emit: &tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
        cancel: CancellationToken,
    ) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().await;
        let Some(active) = state.active.as_ref() else {
            return Err(RuntimeError::NoResumableTurn);
        };
        if let Some(error) = &active.blocked {
            return Err(RuntimeError::Blocked(error.clone()));
        }
        self.drive_locked(&mut state, emit, cancel).await
    }

    async fn drive_locked(
        &self,
        state: &mut RuntimeState,
        emit: &tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
        cancel: CancellationToken,
    ) -> Result<(), RuntimeError> {
        let turn_id = state.active.as_ref().expect("active turn").turn_id.clone();
        loop {
            if cancel.is_cancelled() {
                let _ = emit.send(RuntimeEvent::TurnCancelled {
                    turn_id: turn_id.clone(),
                });
                let _ = emit.send(RuntimeEvent::TurnFailed {
                    turn_id: turn_id.clone(),
                    error: "turn cancelled".to_owned(),
                    recoverable: true,
                });
                return Err(RuntimeError::Provider(ProviderError {
                    kind: ProviderErrorKind::Cancelled,
                    message: "turn cancelled".to_owned(),
                    retry: RetryHint::Never,
                }));
            }
            let step_number = state.active.as_ref().expect("active turn").next_step;
            let step_id: StepId = format!("{}-step-{}", turn_id, step_number).into();
            let request_messages = state.conversation.messages().to_vec();
            let revision = state.conversation.revision();
            let assistant = match self
                .run_model_step(
                    ModelStepContext {
                        turn_id: turn_id.clone(),
                        step_id,
                        revision,
                        messages: request_messages,
                        instructions: state.conversation.instructions().clone(),
                    },
                    emit,
                    cancel.clone(),
                )
                .await
            {
                Ok(message) => message,
                Err(error) => {
                    let recoverable = !matches!(
                        error.kind,
                        ProviderErrorKind::InvalidRequest
                            | ProviderErrorKind::Authentication
                            | ProviderErrorKind::Authorization
                    );
                    if !recoverable {
                        state.active = None;
                    }
                    let _ = emit.send(RuntimeEvent::TurnFailed {
                        turn_id: turn_id.clone(),
                        error: error.to_string(),
                        recoverable,
                    });
                    return Err(RuntimeError::Provider(error));
                }
            };

            state
                .conversation
                .append(Message::Assistant(Box::new(assistant.clone())))
                .map_err(|error| RuntimeError::Conversation(error.to_string()))?;
            let _ = emit.send(RuntimeEvent::AssistantCommitted {
                turn_id: turn_id.clone(),
                message: Box::new(assistant.clone()),
            });

            if assistant.stop_reason != StopReason::ToolUse {
                state.active = None;
                let _ = emit.send(RuntimeEvent::TurnCompleted {
                    turn_id: turn_id.clone(),
                });
                return Ok(());
            }

            for call in assistant.tool_calls().cloned().collect::<Vec<_>>() {
                let _ = emit.send(RuntimeEvent::ToolStarted {
                    turn_id: turn_id.clone(),
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
                let outcome = self.tools.execute(call.clone(), cancel.clone()).await;
                match outcome {
                    ToolOutcome::Completed { content } => {
                        self.append_tool_result(state, &call, content.clone(), false)?;
                        let _ = emit.send(RuntimeEvent::ToolCompleted {
                            turn_id: turn_id.clone(),
                            call_id: call.id,
                            name: call.name,
                            content,
                        });
                    }
                    ToolOutcome::FailedKnown { message } => {
                        self.append_tool_result(state, &call, message.clone(), true)?;
                        let _ = emit.send(RuntimeEvent::ToolFailed {
                            turn_id: turn_id.clone(),
                            call_id: call.id,
                            name: call.name,
                            message,
                            outcome_unknown: false,
                        });
                    }
                    ToolOutcome::OutcomeUnknown { message } => {
                        let _ = emit.send(RuntimeEvent::ToolFailed {
                            turn_id: turn_id.clone(),
                            call_id: call.id,
                            name: call.name,
                            message: message.clone(),
                            outcome_unknown: true,
                        });
                        state.active.as_mut().expect("active turn").blocked = Some(message.clone());
                        let _ = emit.send(RuntimeEvent::TurnFailed {
                            turn_id: turn_id.clone(),
                            error: message.clone(),
                            recoverable: false,
                        });
                        return Err(RuntimeError::Blocked(message));
                    }
                }
            }
            state.active.as_mut().expect("active turn").next_step += 1;
        }
    }

    async fn run_model_step(
        &self,
        context: ModelStepContext,
        emit: &tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
        cancel: CancellationToken,
    ) -> Result<AssistantMessage, ProviderError> {
        let tools = self.tools.definitions();
        for attempt in 1..=self.max_attempts {
            let attempt_sequence = self.next_attempt.fetch_add(1, Ordering::Relaxed) + 1;
            let attempt_id: super::types::AttemptId =
                format!("{}-attempt-{}", context.step_id, attempt_sequence).into();
            let _ = emit.send(RuntimeEvent::ModelStepStarted {
                turn_id: context.turn_id.clone(),
                step_id: context.step_id.clone(),
                attempt_id: attempt_id.clone(),
            });
            let request = ModelRequest {
                turn_id: context.turn_id.clone(),
                step_id: context.step_id.clone(),
                attempt_id,
                conversation_revision: context.revision,
                instructions: context.instructions.clone(),
                messages: context.messages.clone(),
                tools: tools.clone(),
                model: self.model.clone(),
                options: Default::default(),
            };
            let result = self.run_attempt(request, emit, cancel.clone()).await;
            match result {
                Ok(message) => return Ok(message),
                Err(error) if attempt < self.max_attempts && is_retryable(&error) => {
                    let _ = emit.send(RuntimeEvent::ModelStepRetrying {
                        turn_id: context.turn_id.clone(),
                        step_id: context.step_id.clone(),
                        attempt,
                        error: error.to_string(),
                    });
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!()
    }

    async fn run_attempt(
        &self,
        request: ModelRequest,
        emit: &tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
        cancel: CancellationToken,
    ) -> Result<AssistantMessage, ProviderError> {
        let message_id: MessageId = format!("{}-assistant", request.attempt_id).into();
        let mut stream = self.provider.stream(request.clone(), cancel).await?;
        let mut accumulator = ResponseAccumulator::new(message_id);
        while let Some(event) = stream.next().await {
            let runtime_event = match &event {
                ProviderEvent::TextDelta { delta, .. } => Some(RuntimeEvent::TextDelta {
                    turn_id: request.turn_id.clone(),
                    delta: delta.clone(),
                }),
                ProviderEvent::ReasoningDelta { delta, .. } => Some(RuntimeEvent::ReasoningDelta {
                    turn_id: request.turn_id.clone(),
                    delta: delta.clone(),
                }),
                _ => None,
            };
            if let Some(event) = runtime_event {
                let _ = emit.send(event);
            }
            match accumulator.push(event) {
                Ok(super::provider::AccumulatorOutcome::Completed(message)) => return Ok(*message),
                Ok(super::provider::AccumulatorOutcome::Failed(error)) => return Err(error),
                Ok(super::provider::AccumulatorOutcome::Pending) => {}
                Err(error) => return Err(ProviderError::protocol(error.to_string())),
            }
        }
        accumulator
            .finish_eof()
            .map_err(|error| ProviderError::protocol(error.to_string()))?;
        Err(ProviderError::protocol(
            "provider stream ended without completion",
        ))
    }

    fn append_tool_result(
        &self,
        state: &mut RuntimeState,
        call: &ToolCall,
        content: String,
        is_error: bool,
    ) -> Result<(), RuntimeError> {
        state
            .conversation
            .append(Message::ToolResult(ToolResultMessage {
                id: format!("{}-result", call.id).into(),
                tool_call_id: call.id.clone(),
                name: call.name.clone(),
                content: vec![ToolResultContent::Text { text: content }],
                is_error,
            }))
            .map(|_| ())
            .map_err(|error| RuntimeError::Conversation(error.to_string()))
    }
}

fn is_retryable(error: &ProviderError) -> bool {
    matches!(error.retry, RetryHint::Retryable { .. })
        || matches!(
            error.kind,
            ProviderErrorKind::Timeout
                | ProviderErrorKind::Transport
                | ProviderErrorKind::RateLimit
                | ProviderErrorKind::Server
        )
}

fn next_id(prefix: &str, counter: &mut u64) -> String {
    *counter += 1;
    format!("{prefix}-{}", *counter)
}

#[allow(dead_code)]
fn _model_ref(provider: &str, family: &str, model: &str) -> ModelRef {
    ModelRef {
        provider: ProviderId::new(provider),
        api_family: ApiFamily::new(family),
        model: model.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::provider::ProviderEvent;
    use crate::agent::provider::test_support::{Script, ScriptedProvider};
    use crate::agent::tools::{Tool, ToolFuture, ToolRegistry};
    use crate::agent::types::{
        PartIndex, ProviderCompletion, ResponseInfo, ResponseProvenance, ToolCallId, ToolDefinition,
    };

    fn model() -> ModelRef {
        _model_ref("fake", "test", "model")
    }

    fn started() -> ProviderEvent {
        ProviderEvent::ResponseStarted(ResponseInfo {
            provenance: ResponseProvenance {
                requested: model(),
                response_model: None,
                response_id: None,
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

    #[tokio::test]
    async fn retry_reuses_step_and_snapshot_without_committing_failed_draft() {
        let failure = ProviderError {
            kind: ProviderErrorKind::Transport,
            message: "connection reset".to_owned(),
            retry: RetryHint::Retryable { after: None },
        };
        let provider = Arc::new(ScriptedProvider::new([
            Script::EstablishmentFailure(failure),
            Script::Events(vec![
                started(),
                ProviderEvent::TextDelta {
                    part: PartIndex(0),
                    delta: "done".to_owned(),
                },
                completed(StopReason::EndTurn),
            ]),
        ]));
        let runtime = AgentRuntime::new(
            provider.clone(),
            Arc::new(ToolRegistry::new()),
            "system",
            model(),
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        runtime
            .run_user_turn("hello".to_owned(), &tx, CancellationToken::new())
            .await
            .unwrap();

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].step_id, requests[1].step_id);
        assert_eq!(
            requests[0].conversation_revision,
            requests[1].conversation_revision
        );
        assert_eq!(requests[0].messages, requests[1].messages);
        assert_ne!(requests[0].attempt_id, requests[1].attempt_id);
        let conversation = runtime.conversation_snapshot().await;
        assert_eq!(conversation.messages().len(), 2);
    }

    #[tokio::test]
    async fn resume_reenters_the_failed_step_from_the_committed_revision() {
        let failure = ProviderError {
            kind: ProviderErrorKind::Server,
            message: "unavailable".to_owned(),
            retry: RetryHint::Retryable { after: None },
        };
        let provider = Arc::new(ScriptedProvider::new([
            Script::EstablishmentFailure(failure),
            Script::Events(vec![started(), completed(StopReason::EndTurn)]),
        ]));
        let runtime = AgentRuntime::new(
            provider.clone(),
            Arc::new(ToolRegistry::new()),
            "system",
            model(),
        )
        .with_max_attempts(1);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        assert!(
            runtime
                .run_user_turn("hello".to_owned(), &tx, CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(runtime.conversation_snapshot().await.messages().len(), 1);
        runtime
            .resume_turn(&tx, CancellationToken::new())
            .await
            .unwrap();

        let requests = provider.requests();
        assert_eq!(requests[0].step_id, requests[1].step_id);
        assert_eq!(
            requests[0].conversation_revision,
            requests[1].conversation_revision
        );
        assert_ne!(requests[0].attempt_id, requests[1].attempt_id);
    }

    struct EchoTool;

    impl Tool for EchoTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "echo".to_owned(),
                description: "echo input".to_owned(),
                parameters: json!({"type": "object"}),
            }
        }

        fn execute(
            &self,
            arguments: serde_json::Value,
            _cancel: CancellationToken,
        ) -> ToolFuture<'_> {
            Box::pin(async move {
                ToolOutcome::Completed {
                    content: arguments["text"].as_str().unwrap().to_owned(),
                }
            })
        }
    }

    #[tokio::test]
    async fn tool_result_is_committed_before_the_follow_up_model_step() {
        let provider = Arc::new(ScriptedProvider::new([
            Script::Events(vec![
                started(),
                ProviderEvent::ToolCallStarted {
                    part: PartIndex(0),
                    id: ToolCallId::new("call-1"),
                    name: "echo".to_owned(),
                },
                ProviderEvent::ToolArgumentsDelta {
                    part: PartIndex(0),
                    delta: r#"{"text":"hello"}"#.to_owned(),
                },
                completed(StopReason::ToolUse),
            ]),
            Script::Events(vec![
                started(),
                ProviderEvent::TextDelta {
                    part: PartIndex(0),
                    delta: "finished".to_owned(),
                },
                completed(StopReason::EndTurn),
            ]),
        ]));
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool).unwrap();
        let runtime = AgentRuntime::new(provider.clone(), Arc::new(registry), "system", model());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        runtime
            .run_user_turn("echo".to_owned(), &tx, CancellationToken::new())
            .await
            .unwrap();

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].messages.len(), 3);
        assert!(matches!(requests[1].messages[2], Message::ToolResult(_)));
        assert_eq!(runtime.conversation_snapshot().await.messages().len(), 4);
    }
}
