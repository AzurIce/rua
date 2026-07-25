use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::conversation::Conversation;
use super::journal::{
    DurableToolCall, DurableTurn, InMemorySessionStore, JournalRecord, LegacyToolApprovalState,
    ReconciliationDecision, RecordedToolOutcome, RecoveredSession, SessionStore, StoreError,
    TurnPhase,
};
use super::provider::{Provider, ProviderEvent, ResponseAccumulator};
use super::tools::{ToolExecutor, ToolOutcome};
use super::types::{
    ApiFamily, AssistantMessage, ConversationRevision, ExecutionId, Message, MessageId, ModelRef,
    ModelRequest, ProviderError, ProviderErrorKind, ProviderId, RetryHint, SessionId, StepId,
    StopReason, ToolCall, ToolResultContent, ToolResultMessage, TurnId, UserContent, UserMessage,
};

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeEvent {
    SessionRecovered {
        session_id: SessionId,
        conversation: Conversation,
    },
    RecoveryRequired {
        turn_id: TurnId,
        tool_call_id: super::types::ToolCallId,
        message: String,
    },
    PersistenceFailed {
        message: String,
    },
    OperationFailed {
        message: String,
    },
    TurnStarted {
        turn_id: TurnId,
    },
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
        attempt_id: super::types::AttemptId,
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
        execution_id: ExecutionId,
        name: String,
        arguments: serde_json::Value,
    },
    ToolCompleted {
        turn_id: TurnId,
        call_id: super::types::ToolCallId,
        execution_id: ExecutionId,
        name: String,
        content: String,
    },
    ToolFailed {
        turn_id: TurnId,
        call_id: super::types::ToolCallId,
        execution_id: ExecutionId,
        name: String,
        message: String,
        outcome_unknown: bool,
    },
    ToolRejected {
        turn_id: TurnId,
        call_id: super::types::ToolCallId,
        name: String,
        message: String,
    },
    TurnCompleted {
        turn_id: TurnId,
    },
    TurnFailed {
        turn_id: TurnId,
        kind: RuntimeFailureKind,
        error: String,
        recoverable: bool,
    },
    TurnCancelled {
        turn_id: TurnId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeFailureKind {
    Provider,
    Protocol,
    Tool,
    Limit,
    Cancellation,
    Persistence,
    Internal,
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
    #[error("turn limit exceeded: {0}")]
    Limit(String),
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("persistence error: {0}")]
    Persistence(#[from] StoreError),
}

struct ActiveTurn {
    turn_id: TurnId,
    next_step: u32,
    tool_calls_used: u32,
    blocked: Option<String>,
    recovery: Option<DurableTurn>,
}

struct RuntimeState {
    conversation: Conversation,
    active: Option<ActiveTurn>,
    session_initialized: bool,
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
    session_id: SessionId,
    store: Arc<dyn SessionStore>,
    state: Mutex<RuntimeState>,
    max_attempts: u32,
    max_model_steps: u32,
    max_tool_calls: u32,
    next_attempt: AtomicU64,
    next_execution: AtomicU64,
}

impl AgentRuntime {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<dyn ToolExecutor>,
        instructions: impl Into<String>,
        model: ModelRef,
    ) -> Self {
        static NEXT_SESSION: AtomicU64 = AtomicU64::new(0);
        let session_sequence = NEXT_SESSION.fetch_add(1, Ordering::Relaxed) + 1;
        Self {
            provider,
            tools,
            model,
            session_id: new_session_id(session_sequence),
            store: Arc::new(InMemorySessionStore::new()),
            state: Mutex::new(RuntimeState {
                conversation: Conversation::new(super::types::InstructionSet::new(instructions)),
                active: None,
                session_initialized: false,
                next_message: 0,
                next_turn: 0,
            }),
            max_attempts: 2,
            max_model_steps: 16,
            max_tool_calls: 64,
            next_attempt: AtomicU64::new(0),
            next_execution: AtomicU64::new(0),
        }
    }

    pub fn with_session_store(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.store = store;
        self
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub async fn recover(
        provider: Arc<dyn Provider>,
        tools: Arc<dyn ToolExecutor>,
        model: ModelRef,
        store: Arc<dyn SessionStore>,
        session_id: SessionId,
    ) -> Result<Self, RuntimeError> {
        let recovered = store.load(&session_id).await?;
        Ok(Self::from_recovered(
            provider, tools, model, store, recovered,
        ))
    }

    fn from_recovered(
        provider: Arc<dyn Provider>,
        tools: Arc<dyn ToolExecutor>,
        model: ModelRef,
        store: Arc<dyn SessionStore>,
        recovered: RecoveredSession,
    ) -> Self {
        let next_message = max_numbered_message(&recovered.conversation);
        let next_turn = recovered
            .active_turn
            .as_ref()
            .and_then(|turn| numbered_suffix(turn.turn_id.as_str(), "turn-"))
            .unwrap_or(0);
        let next_attempt = recovered
            .active_turn
            .as_ref()
            .map(|turn| turn.attempts.len() as u64)
            .unwrap_or(0);
        let next_execution = recovered
            .active_turn
            .as_ref()
            .map(|turn| {
                turn.pending_tools
                    .iter()
                    .map(|tool| tool.execution_count)
                    .fold(0_u64, u64::saturating_add)
            })
            .unwrap_or(0);
        let active = recovered.active_turn.and_then(|turn| {
            if matches!(
                turn.phase,
                TurnPhase::Completed
                    | TurnPhase::Failed { recoverable: false }
                    | TurnPhase::Cancelled
            ) {
                return None;
            }
            let blocked = match turn.phase {
                TurnPhase::NeedsReconciliation => Some(
                    "recovered tool execution has an unknown outcome; reconciliation is required"
                        .to_owned(),
                ),
                TurnPhase::LegacyAwaitingApproval => None,
                _ => None,
            };
            Some(ActiveTurn {
                turn_id: turn.turn_id.clone(),
                next_step: turn.next_step,
                tool_calls_used: u32::try_from(turn.pending_tools.len()).unwrap_or(u32::MAX),
                blocked,
                recovery: Some(turn),
            })
        });
        Self {
            provider,
            tools,
            model,
            session_id: recovered.session_id,
            store,
            state: Mutex::new(RuntimeState {
                conversation: recovered.conversation,
                active,
                session_initialized: true,
                next_message,
                next_turn,
            }),
            max_attempts: 2,
            max_model_steps: 16,
            max_tool_calls: 64,
            next_attempt: AtomicU64::new(next_attempt),
            next_execution: AtomicU64::new(next_execution),
        }
    }

    pub async fn publish_recovery(&self, emit: &tokio::sync::mpsc::UnboundedSender<RuntimeEvent>) {
        let state = self.state.lock().await;
        let _ = emit.send(RuntimeEvent::SessionRecovered {
            session_id: self.session_id.clone(),
            conversation: state.conversation.clone(),
        });
        if let Some(recovery) = state
            .active
            .as_ref()
            .and_then(|active| active.recovery.as_ref())
            && matches!(recovery.phase, TurnPhase::NeedsReconciliation)
        {
            for tool in recovery.pending_tools.iter().filter(|tool| {
                tool.execution_id.is_some() && tool.outcome.is_none() && !tool.result_committed
            }) {
                let _ = emit.send(RuntimeEvent::RecoveryRequired {
                    turn_id: recovery.turn_id.clone(),
                    tool_call_id: tool.tool_call_id.clone(),
                    message: format!(
                        "{} may have executed before the previous process stopped",
                        tool.name
                    ),
                });
            }
        }
    }

    pub async fn recovery_tools(&self) -> Vec<DurableToolCall> {
        self.state
            .lock()
            .await
            .active
            .as_ref()
            .and_then(|active| active.recovery.as_ref())
            .map(|turn| {
                turn.pending_tools
                    .iter()
                    .filter(|tool| {
                        tool.execution_id.is_some()
                            && tool.outcome.is_none()
                            && !tool.result_committed
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub async fn reconcile_tool(
        &self,
        tool_call_id: super::types::ToolCallId,
        decision: ReconciliationDecision,
        emit: &tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
        cancel: CancellationToken,
    ) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().await;
        let recovery = state
            .active
            .as_ref()
            .and_then(|active| active.recovery.as_ref())
            .ok_or_else(|| RuntimeError::Blocked("no recovery is pending".to_owned()))?;
        if !matches!(recovery.phase, TurnPhase::NeedsReconciliation)
            || !recovery.pending_tools.iter().any(|tool| {
                tool.tool_call_id == tool_call_id
                    && tool.execution_id.is_some()
                    && tool.outcome.is_none()
                    && !tool.result_committed
            })
        {
            return Err(RuntimeError::Blocked(format!(
                "tool call {tool_call_id} does not require reconciliation"
            )));
        }

        self.record(JournalRecord::ReconciliationResolved {
            tool_call_id: tool_call_id.clone(),
            decision: decision.clone(),
        })
        .await?;
        if matches!(decision, ReconciliationDecision::AbandonTurn) {
            let turn_id = recovery.turn_id.clone();
            self.record(JournalRecord::TurnCancelled {
                turn_id: turn_id.clone(),
            })
            .await?;
            state.active = None;
            let _ = emit.send(RuntimeEvent::TurnCancelled { turn_id });
            return Ok(());
        }

        let active = state.active.as_mut().expect("recovery active turn");
        let durable = active.recovery.as_mut().expect("recovery state");
        let tool = durable
            .pending_tools
            .iter_mut()
            .find(|tool| tool.tool_call_id == tool_call_id)
            .expect("validated recovery tool");
        match decision {
            ReconciliationDecision::MarkSucceeded { content } => {
                tool.outcome = Some(RecordedToolOutcome::Completed { content });
            }
            ReconciliationDecision::MarkFailed { message } => {
                tool.outcome = Some(RecordedToolOutcome::FailedKnown { message });
            }
            ReconciliationDecision::RetryAnyway => {
                tool.execution_id = None;
                tool.outcome = None;
            }
            ReconciliationDecision::AbandonTurn => unreachable!(),
        }
        durable.phase = TurnPhase::ExecutingTools;
        active.blocked = None;
        self.resume_recovered_tools(&mut state, emit, cancel.clone())
            .await?;
        self.drive_locked(&mut state, emit, cancel).await
    }

    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    pub fn with_turn_limits(mut self, max_model_steps: u32, max_tool_calls: u32) -> Self {
        self.max_model_steps = max_model_steps.max(1);
        self.max_tool_calls = max_tool_calls.max(1);
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
        self.ensure_session_initialized(&mut state).await?;
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
        self.durable_append(
            &mut state,
            Message::User(UserMessage {
                id: message_id,
                content: vec![UserContent::Text { text: text.clone() }],
            }),
        )
        .await?;
        state.active = Some(ActiveTurn {
            turn_id: turn_id.clone(),
            next_step: 0,
            tool_calls_used: 0,
            blocked: None,
            recovery: None,
        });
        if let Err(error) = self
            .record(JournalRecord::TurnOpened {
                turn_id: turn_id.clone(),
                stable_revision: state.conversation.revision(),
            })
            .await
        {
            block_turn(&mut state, &error);
            return Err(error);
        }
        let _ = emit.send(RuntimeEvent::TurnStarted {
            turn_id: turn_id.clone(),
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
        if active.recovery.is_some() {
            self.resume_recovered_tools(&mut state, emit, cancel.clone())
                .await?;
        }
        self.drive_locked(&mut state, emit, cancel).await
    }

    async fn resume_recovered_tools(
        &self,
        state: &mut RuntimeState,
        emit: &tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
        cancel: CancellationToken,
    ) -> Result<(), RuntimeError> {
        let mut recovery = state
            .active
            .as_ref()
            .and_then(|active| active.recovery.clone())
            .expect("checked recovery state");
        if matches!(
            recovery.phase,
            TurnPhase::AwaitingModel
                | TurnPhase::WaitingToRetry
                | TurnPhase::Failed { recoverable: true }
        ) {
            state.active.as_mut().expect("active turn").recovery = None;
            return Ok(());
        }
        if matches!(recovery.phase, TurnPhase::LegacyAwaitingApproval) {
            // Legacy sessions may contain this phase. The current runtime has
            // no built-in approval gate, so pending calls continue normally.
            recovery.phase = TurnPhase::ExecutingTools;
        }
        if !matches!(recovery.phase, TurnPhase::ExecutingTools) {
            return Err(RuntimeError::Blocked(format!(
                "recovered turn cannot resume from phase {:?}",
                recovery.phase
            )));
        }

        let mut completed_missing_result = false;
        for index in 0..recovery.pending_tools.len() {
            if recovery.pending_tools[index].result_committed {
                continue;
            }
            let tool = recovery.pending_tools[index].clone();
            let call = ToolCall {
                id: tool.tool_call_id.clone(),
                name: tool.name.clone(),
                arguments: tool.arguments.clone(),
                provider_state: None,
            };
            if let LegacyToolApprovalState::Rejected { reason } = &tool.legacy_approval {
                if let Err(error) = self
                    .append_tool_result(state, &call, reason.clone(), true)
                    .await
                {
                    block_recovery(state, &recovery, &error);
                    return Err(error);
                }
                let _ = emit.send(RuntimeEvent::ToolRejected {
                    turn_id: recovery.turn_id.clone(),
                    call_id: call.id,
                    name: call.name,
                    message: reason.clone(),
                });
                recovery.pending_tools[index].result_committed = true;
                completed_missing_result = true;
                continue;
            }

            let (outcome, execution_id) = if let Some(outcome) = &tool.outcome {
                let execution_id = tool.execution_id.clone().ok_or_else(|| {
                    RuntimeError::Blocked(format!(
                        "recorded outcome for {} has no execution id",
                        tool.tool_call_id
                    ))
                })?;
                (outcome.clone(), execution_id)
            } else if tool.execution_id.is_none() {
                let execution_sequence = self.next_execution.fetch_add(1, Ordering::Relaxed) + 1;
                let execution_id: ExecutionId =
                    format!("{}-execution-{execution_sequence}", call.id).into();
                if let Err(error) = self
                    .record(JournalRecord::ToolExecutionStarted {
                        tool_call_id: call.id.clone(),
                        execution_id: execution_id.clone(),
                    })
                    .await
                {
                    block_recovery(state, &recovery, &error);
                    return Err(error);
                }
                recovery.pending_tools[index].execution_id = Some(execution_id.clone());
                let _ = emit.send(RuntimeEvent::ToolStarted {
                    turn_id: recovery.turn_id.clone(),
                    call_id: call.id.clone(),
                    execution_id: execution_id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
                let outcome = self.tools.execute(call.clone(), cancel.clone()).await;
                let recorded = recorded_outcome(&outcome);
                if let Err(error) = self
                    .record(JournalRecord::ToolOutcomeRecorded {
                        tool_call_id: call.id.clone(),
                        execution_id: execution_id.clone(),
                        outcome: recorded.clone(),
                    })
                    .await
                {
                    block_recovery(state, &recovery, &error);
                    return Err(error);
                }
                recovery.pending_tools[index].outcome = Some(recorded.clone());
                (recorded, execution_id)
            } else {
                let error = RuntimeError::Blocked(format!(
                    "tool {} has an unknown recovered outcome",
                    call.id
                ));
                block_turn(state, &error);
                return Err(error);
            };

            match outcome {
                RecordedToolOutcome::Completed { content } => {
                    if let Err(error) = self
                        .append_tool_result(state, &call, content.clone(), false)
                        .await
                    {
                        block_recovery(state, &recovery, &error);
                        return Err(error);
                    }
                    let _ = emit.send(RuntimeEvent::ToolCompleted {
                        turn_id: recovery.turn_id.clone(),
                        call_id: call.id,
                        execution_id,
                        name: call.name,
                        content,
                    });
                }
                RecordedToolOutcome::FailedKnown { message } => {
                    if let Err(error) = self
                        .append_tool_result(state, &call, message.clone(), true)
                        .await
                    {
                        block_recovery(state, &recovery, &error);
                        return Err(error);
                    }
                    let _ = emit.send(RuntimeEvent::ToolFailed {
                        turn_id: recovery.turn_id.clone(),
                        call_id: call.id,
                        execution_id,
                        name: call.name,
                        message,
                        outcome_unknown: false,
                    });
                }
                RecordedToolOutcome::OutcomeUnknown { message } => {
                    let error = RuntimeError::Blocked(message.clone());
                    recovery.phase = TurnPhase::NeedsReconciliation;
                    state.active.as_mut().expect("active turn").recovery = Some(recovery.clone());
                    block_turn(state, &error);
                    let _ = emit.send(RuntimeEvent::ToolFailed {
                        turn_id: recovery.turn_id.clone(),
                        call_id: call.id.clone(),
                        execution_id,
                        name: call.name,
                        message: message.clone(),
                        outcome_unknown: true,
                    });
                    let _ = emit.send(RuntimeEvent::RecoveryRequired {
                        turn_id: recovery.turn_id.clone(),
                        tool_call_id: call.id,
                        message: message.clone(),
                    });
                    return Err(error);
                }
            }
            recovery.pending_tools[index].result_committed = true;
            completed_missing_result = true;
        }
        let active = state.active.as_mut().expect("active turn");
        if completed_missing_result {
            recovery.next_step = recovery
                .next_step
                .checked_add(1)
                .ok_or_else(|| RuntimeError::Blocked("model step counter overflow".to_owned()))?;
            active.next_step = recovery.next_step;
        }
        active.recovery = None;
        Ok(())
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
                if let Err(error) = self
                    .record(JournalRecord::TurnCancelled {
                        turn_id: turn_id.clone(),
                    })
                    .await
                {
                    block_turn(state, &error);
                    return Err(error);
                }
                let _ = emit.send(RuntimeEvent::TurnCancelled {
                    turn_id: turn_id.clone(),
                });
                state.active = None;
                return Err(RuntimeError::Provider(ProviderError {
                    kind: ProviderErrorKind::Cancelled,
                    message: "turn cancelled".to_owned(),
                    retry: RetryHint::Never,
                }));
            }
            let step_number = state.active.as_ref().expect("active turn").next_step;
            if step_number >= self.max_model_steps {
                let error = RuntimeError::Limit(format!(
                    "model step limit {} reached",
                    self.max_model_steps
                ));
                self.record(JournalRecord::TurnFailed {
                    turn_id: turn_id.clone(),
                    error: error.to_string(),
                    recoverable: false,
                })
                .await?;
                state.active = None;
                let _ = emit.send(RuntimeEvent::TurnFailed {
                    turn_id: turn_id.clone(),
                    kind: RuntimeFailureKind::Limit,
                    error: error.to_string(),
                    recoverable: false,
                });
                return Err(error);
            }
            let step_id: StepId = format!("{}-step-{}", turn_id, step_number).into();
            let request_messages = state.conversation.messages().to_vec();
            let revision = state.conversation.revision();
            let assistant = match self
                .run_model_step(
                    ModelStepContext {
                        turn_id: turn_id.clone(),
                        step_id: step_id.clone(),
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
                    if matches!(
                        &error,
                        RuntimeError::Provider(provider_error)
                            if provider_error.kind == ProviderErrorKind::Cancelled
                    ) {
                        self.record(JournalRecord::TurnCancelled {
                            turn_id: turn_id.clone(),
                        })
                        .await?;
                        state.active = None;
                        let _ = emit.send(RuntimeEvent::TurnCancelled {
                            turn_id: turn_id.clone(),
                        });
                        return Err(error);
                    }
                    let recoverable = matches!(&error, RuntimeError::Provider(provider_error) if !matches!(
                        provider_error.kind,
                        ProviderErrorKind::InvalidRequest
                            | ProviderErrorKind::Authentication
                            | ProviderErrorKind::Authorization
                            | ProviderErrorKind::UnsupportedCapability
                            | ProviderErrorKind::ContextLength
                    ));
                    if matches!(&error, RuntimeError::Persistence(_)) {
                        block_turn(state, &error);
                    } else if !recoverable {
                        state.active = None;
                    }
                    if !matches!(&error, RuntimeError::Persistence(_)) {
                        self.record(JournalRecord::TurnFailed {
                            turn_id: turn_id.clone(),
                            error: error.to_string(),
                            recoverable,
                        })
                        .await?;
                    }
                    let _ = emit.send(RuntimeEvent::TurnFailed {
                        turn_id: turn_id.clone(),
                        kind: runtime_failure_kind(&error),
                        error: error.to_string(),
                        recoverable,
                    });
                    return Err(error);
                }
            };

            let tool_calls = assistant.tool_calls().cloned().collect::<Vec<_>>();
            if assistant.stop_reason == StopReason::ToolUse {
                let active = state.active.as_ref().expect("active turn");
                let requested = u32::try_from(tool_calls.len()).unwrap_or(u32::MAX);
                if active.tool_calls_used.saturating_add(requested) > self.max_tool_calls {
                    let error = RuntimeError::Limit(format!(
                        "tool call limit {} would be exceeded",
                        self.max_tool_calls
                    ));
                    self.record(JournalRecord::TurnFailed {
                        turn_id: turn_id.clone(),
                        error: error.to_string(),
                        recoverable: false,
                    })
                    .await?;
                    state.active = None;
                    let _ = emit.send(RuntimeEvent::TurnFailed {
                        turn_id: turn_id.clone(),
                        kind: RuntimeFailureKind::Limit,
                        error: error.to_string(),
                        recoverable: false,
                    });
                    return Err(error);
                }
            }

            let resulting_revision = self
                .durable_append(state, Message::Assistant(Box::new(assistant.clone())))
                .await?;
            if let Err(error) = self
                .record(JournalRecord::AssistantCommitted {
                    turn_id: turn_id.clone(),
                    step_id: step_id.clone(),
                    message_id: assistant.id.clone(),
                    resulting_revision,
                })
                .await
            {
                block_turn(state, &error);
                return Err(error);
            }
            let _ = emit.send(RuntimeEvent::AssistantCommitted {
                turn_id: turn_id.clone(),
                message: Box::new(assistant.clone()),
            });

            if assistant.stop_reason != StopReason::ToolUse {
                if let Err(error) = self
                    .record(JournalRecord::TurnCompleted {
                        turn_id: turn_id.clone(),
                    })
                    .await
                {
                    block_turn(state, &error);
                    return Err(error);
                }
                if let Err(error) = self.store.checkpoint(&self.session_id).await {
                    let _ = emit.send(RuntimeEvent::PersistenceFailed {
                        message: format!("session checkpoint failed: {error}"),
                    });
                }
                state.active = None;
                let _ = emit.send(RuntimeEvent::TurnCompleted {
                    turn_id: turn_id.clone(),
                });
                return Ok(());
            }

            for call in &tool_calls {
                if let Err(error) = self
                    .record(JournalRecord::ToolExecutionIntended {
                        turn_id: turn_id.clone(),
                        step_id: step_id.clone(),
                        tool_call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        replay_class: self
                            .tools
                            .definition(&call.name)
                            .map(|definition| definition.replay_class)
                            .unwrap_or(super::types::ReplayClass::Unknown),
                    })
                    .await
                {
                    block_turn(state, &error);
                    return Err(error);
                }
            }
            let recovered = self.store.load(&self.session_id).await?;
            let durable = recovered.active_turn.ok_or_else(|| {
                RuntimeError::Blocked("tool intents did not recover an active turn".to_owned())
            })?;
            let active = state.active.as_mut().expect("active turn");
            active.tool_calls_used = u32::try_from(durable.pending_tools.len()).unwrap_or(u32::MAX);
            active.recovery = Some(durable);
            self.resume_recovered_tools(state, emit, cancel.clone())
                .await?;
        }
    }

    async fn run_model_step(
        &self,
        context: ModelStepContext,
        emit: &tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
        cancel: CancellationToken,
    ) -> Result<AssistantMessage, RuntimeError> {
        self.record(JournalRecord::ModelStepPrepared {
            turn_id: context.turn_id.clone(),
            step_id: context.step_id.clone(),
            revision: context.revision,
        })
        .await?;
        let tools = self.tools.definitions();
        let capabilities = self.provider.capabilities(&self.model);
        if !tools.is_empty() && !capabilities.tools {
            return Err(RuntimeError::Provider(ProviderError {
                kind: ProviderErrorKind::UnsupportedCapability,
                message: format!("model {} does not support tool calling", self.model.model),
                retry: RetryHint::Never,
            }));
        }
        for attempt in 1..=self.max_attempts {
            let attempt_sequence = self.next_attempt.fetch_add(1, Ordering::Relaxed) + 1;
            let attempt_id: super::types::AttemptId =
                format!("{}-attempt-{}", context.step_id, attempt_sequence).into();
            self.record(JournalRecord::AttemptStarted {
                turn_id: context.turn_id.clone(),
                step_id: context.step_id.clone(),
                attempt_id: attempt_id.clone(),
            })
            .await?;
            let _ = emit.send(RuntimeEvent::ModelStepStarted {
                turn_id: context.turn_id.clone(),
                step_id: context.step_id.clone(),
                attempt_id: attempt_id.clone(),
            });
            let request = ModelRequest {
                turn_id: context.turn_id.clone(),
                step_id: context.step_id.clone(),
                attempt_id: attempt_id.clone(),
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
                    self.record(JournalRecord::AttemptFailed {
                        turn_id: context.turn_id.clone(),
                        step_id: context.step_id.clone(),
                        attempt_id: attempt_id.clone(),
                        error: error.to_string(),
                        retryable: true,
                    })
                    .await?;
                    let _ = emit.send(RuntimeEvent::ModelStepRetrying {
                        turn_id: context.turn_id.clone(),
                        step_id: context.step_id.clone(),
                        attempt_id,
                        attempt,
                        error: error.to_string(),
                    });
                    continue;
                }
                Err(error) => {
                    self.record(JournalRecord::AttemptFailed {
                        turn_id: context.turn_id.clone(),
                        step_id: context.step_id.clone(),
                        attempt_id,
                        error: error.to_string(),
                        retryable: is_retryable(&error),
                    })
                    .await?;
                    return Err(RuntimeError::Provider(error));
                }
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

    async fn append_tool_result(
        &self,
        state: &mut RuntimeState,
        call: &ToolCall,
        content: String,
        is_error: bool,
    ) -> Result<(), RuntimeError> {
        let message_id: MessageId = format!("{}-result", call.id).into();
        let resulting_revision = self
            .durable_append(
                state,
                Message::ToolResult(ToolResultMessage {
                    id: message_id.clone(),
                    tool_call_id: call.id.clone(),
                    name: call.name.clone(),
                    content: vec![ToolResultContent::Text { text: content }],
                    is_error,
                }),
            )
            .await?;
        self.record(JournalRecord::ToolResultCommitted {
            tool_call_id: call.id.clone(),
            message_id,
            resulting_revision,
        })
        .await?;
        Ok(())
    }

    async fn ensure_session_initialized(
        &self,
        state: &mut RuntimeState,
    ) -> Result<(), RuntimeError> {
        if state.session_initialized {
            return Ok(());
        }
        self.record(JournalRecord::SessionCreated {
            instructions: state.conversation.instructions().clone(),
        })
        .await?;
        state.session_initialized = true;
        Ok(())
    }

    async fn durable_append(
        &self,
        state: &mut RuntimeState,
        message: Message,
    ) -> Result<ConversationRevision, RuntimeError> {
        let expected_revision = state.conversation.revision();
        let mut next = state.conversation.clone();
        let resulting_revision = next
            .append(message.clone())
            .map_err(|error| RuntimeError::Conversation(error.to_string()))?;
        self.record(JournalRecord::ConversationAppended {
            message,
            expected_revision,
            resulting_revision,
        })
        .await?;
        state.conversation = next;
        Ok(resulting_revision)
    }

    async fn record(&self, record: JournalRecord) -> Result<(), RuntimeError> {
        self.store.append(&self.session_id, record).await?;
        Ok(())
    }
}

fn recorded_outcome(outcome: &ToolOutcome) -> RecordedToolOutcome {
    match outcome {
        ToolOutcome::Completed { content } => RecordedToolOutcome::Completed {
            content: content.clone(),
        },
        ToolOutcome::FailedKnown { message } => RecordedToolOutcome::FailedKnown {
            message: message.clone(),
        },
        ToolOutcome::OutcomeUnknown { message } => RecordedToolOutcome::OutcomeUnknown {
            message: message.clone(),
        },
    }
}

fn block_turn(state: &mut RuntimeState, error: &RuntimeError) {
    if let Some(active) = state.active.as_mut() {
        active.blocked = Some(error.to_string());
    }
}

fn block_recovery(state: &mut RuntimeState, recovery: &DurableTurn, error: &RuntimeError) {
    if let Some(active) = state.active.as_mut() {
        active.recovery = Some(recovery.clone());
    }
    block_turn(state, error);
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

fn runtime_failure_kind(error: &RuntimeError) -> RuntimeFailureKind {
    match error {
        RuntimeError::Provider(error) if error.kind == ProviderErrorKind::Protocol => {
            RuntimeFailureKind::Protocol
        }
        RuntimeError::Provider(error) if error.kind == ProviderErrorKind::Cancelled => {
            RuntimeFailureKind::Cancellation
        }
        RuntimeError::Provider(_) => RuntimeFailureKind::Provider,
        RuntimeError::Limit(_) => RuntimeFailureKind::Limit,
        RuntimeError::Persistence(_) => RuntimeFailureKind::Persistence,
        RuntimeError::Busy
        | RuntimeError::NoResumableTurn
        | RuntimeError::Blocked(_)
        | RuntimeError::Conversation(_) => RuntimeFailureKind::Internal,
    }
}

fn next_id(prefix: &str, counter: &mut u64) -> String {
    *counter += 1;
    format!("{prefix}-{}", *counter)
}

fn new_session_id(sequence: u64) -> SessionId {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("session-{timestamp}-{}-{sequence}", std::process::id()).into()
}

fn max_numbered_message(conversation: &Conversation) -> u64 {
    conversation
        .messages()
        .iter()
        .filter_map(|message| numbered_suffix(message.id().as_str(), "message-"))
        .max()
        .unwrap_or(0)
}

fn numbered_suffix(value: &str, prefix: &str) -> Option<u64> {
    value.strip_prefix(prefix)?.parse().ok()
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
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use serde_json::json;

    use super::*;
    use crate::agent::journal::StoreFuture;
    use crate::agent::provider::ProviderEvent;
    use crate::agent::provider::test_support::{Script, ScriptedProvider};
    use crate::agent::tools::{Tool, ToolFuture, ToolRegistry};
    use crate::agent::types::{
        ModelCapabilities, PartIndex, ProviderCompletion, ResponseInfo, ResponseProvenance,
        ToolCallId, ToolDefinition,
    };
    use crate::agent::{
        InMemorySessionStore, JournalRecord, JournalSequence, LocalSessionStore, SessionStore,
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
        let store = Arc::new(InMemorySessionStore::new());
        let runtime = AgentRuntime::new(
            provider.clone(),
            Arc::new(ToolRegistry::new()),
            "system",
            model(),
        )
        .with_session_store(store.clone());
        let session_id = runtime.session_id().clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

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
        let mut first_attempt = None;
        let mut retry_attempt = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                RuntimeEvent::ModelStepStarted { attempt_id, .. } if first_attempt.is_none() => {
                    first_attempt = Some(attempt_id)
                }
                RuntimeEvent::ModelStepRetrying { attempt_id, .. } => {
                    retry_attempt = Some(attempt_id)
                }
                _ => {}
            }
        }
        assert_eq!(retry_attempt, first_attempt);
        let entries = store.entries(&session_id).await;
        let failed_index = entries
            .iter()
            .position(|entry| matches!(entry.record, JournalRecord::AttemptFailed { .. }))
            .unwrap();
        let waiting =
            crate::agent::journal::replay_session(&session_id, &entries[..=failed_index]).unwrap();
        assert!(matches!(
            waiting.active_turn.unwrap().phase,
            TurnPhase::WaitingToRetry
        ));
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
                parameters: json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"],
                    "additionalProperties": false
                }),
                replay_class: crate::agent::ReplayClass::ReadOnly,
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

    struct UnknownOutcomeTool;

    impl Tool for UnknownOutcomeTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "unknown_outcome".to_owned(),
                description: "returns an unknown outcome".to_owned(),
                parameters: json!({ "type": "object", "additionalProperties": false }),
                replay_class: crate::agent::ReplayClass::Unknown,
            }
        }

        fn execute(
            &self,
            _arguments: serde_json::Value,
            _cancel: CancellationToken,
        ) -> ToolFuture<'_> {
            Box::pin(async {
                ToolOutcome::OutcomeUnknown {
                    message: "external effect cannot be confirmed".to_owned(),
                }
            })
        }
    }

    #[tokio::test]
    async fn unknown_tool_outcome_publishes_recovery_without_a_terminal_failure() {
        let provider = Arc::new(ScriptedProvider::new([Script::Events(vec![
            started(),
            ProviderEvent::ToolCallStarted {
                part: PartIndex(0),
                id: ToolCallId::new("call-1"),
                name: "unknown_outcome".to_owned(),
            },
            ProviderEvent::ToolArgumentsDelta {
                part: PartIndex(0),
                delta: "{}".to_owned(),
            },
            completed(StopReason::ToolUse),
        ])]));
        let mut tools = ToolRegistry::new();
        tools.register(UnknownOutcomeTool).unwrap();
        let runtime = AgentRuntime::new(provider, Arc::new(tools), "system", model());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        assert!(matches!(
            runtime
                .run_user_turn("act".to_owned(), &tx, CancellationToken::new())
                .await,
            Err(RuntimeError::Blocked(_))
        ));

        let mut saw_unknown = false;
        let mut saw_recovery = false;
        let mut saw_terminal = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                RuntimeEvent::ToolFailed {
                    outcome_unknown: true,
                    ..
                } => saw_unknown = true,
                RuntimeEvent::RecoveryRequired { .. } => saw_recovery = true,
                RuntimeEvent::TurnCompleted { .. }
                | RuntimeEvent::TurnFailed { .. }
                | RuntimeEvent::TurnCancelled { .. } => saw_terminal = true,
                _ => {}
            }
        }
        assert!(saw_unknown);
        assert!(saw_recovery);
        assert!(!saw_terminal);
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
        let store = Arc::new(InMemorySessionStore::new());
        let runtime = AgentRuntime::new(provider.clone(), Arc::new(registry), "system", model())
            .with_session_store(store.clone());
        let session_id = runtime.session_id().clone();
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

        let entries = store.entries(&session_id).await;
        let records: Vec<_> = entries.iter().map(|entry| entry.record.clone()).collect();
        let intended_index = records
            .iter()
            .position(|record| matches!(record, JournalRecord::ToolExecutionIntended { .. }))
            .unwrap();
        let started_index = records
            .iter()
            .position(|record| matches!(record, JournalRecord::ToolExecutionStarted { .. }))
            .unwrap();
        let outcome_index = records
            .iter()
            .position(|record| matches!(record, JournalRecord::ToolOutcomeRecorded { .. }))
            .unwrap();
        let result_index = records
            .iter()
            .position(|record| matches!(record, JournalRecord::ToolResultCommitted { .. }))
            .unwrap();
        assert!(
            intended_index < started_index
                && started_index < outcome_index
                && outcome_index < result_index
        );

        let started_recovery =
            crate::agent::journal::replay_session(&session_id, &entries[..=started_index]).unwrap();
        assert!(matches!(
            started_recovery.active_turn.unwrap().phase,
            crate::agent::TurnPhase::NeedsReconciliation
        ));

        let known_outcome_recovery =
            crate::agent::journal::replay_session(&session_id, &entries[..=outcome_index]).unwrap();
        let recovered_turn = known_outcome_recovery.active_turn.unwrap();
        assert!(matches!(
            recovered_turn.phase,
            crate::agent::TurnPhase::ExecutingTools
        ));
        assert!(recovered_turn.pending_tools[0].outcome.is_some());
        assert!(!recovered_turn.pending_tools[0].result_committed);

        let completed_recovery = store.load(&session_id).await.unwrap();
        assert_eq!(completed_recovery.conversation.messages().len(), 4);
        assert!(matches!(
            completed_recovery.active_turn.unwrap().phase,
            crate::agent::TurnPhase::Completed
        ));

        let outcome_store = Arc::new(InMemorySessionStore::new());
        for entry in &entries[..=outcome_index] {
            outcome_store
                .append(&session_id, entry.record.clone())
                .await
                .unwrap();
        }
        let recovery_provider = Arc::new(ScriptedProvider::new([Script::Events(vec![
            started(),
            ProviderEvent::TextDelta {
                part: PartIndex(0),
                delta: "recovered".to_owned(),
            },
            completed(StopReason::EndTurn),
        ])]));
        let executions = Arc::new(AtomicUsize::new(0));
        let mut recovery_tools = ToolRegistry::new();
        recovery_tools
            .register(CountingTool(executions.clone()))
            .unwrap();
        let recovered_runtime = AgentRuntime::recover(
            recovery_provider.clone(),
            Arc::new(recovery_tools),
            model(),
            outcome_store,
            session_id.clone(),
        )
        .await
        .unwrap();
        recovered_runtime
            .resume_turn(&tx, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(executions.load(AtomicOrdering::SeqCst), 0);
        assert!(matches!(
            recovery_provider.requests()[0].messages[2],
            Message::ToolResult(_)
        ));

        let started_store = Arc::new(InMemorySessionStore::new());
        for entry in &entries[..=started_index] {
            started_store
                .append(&session_id, entry.record.clone())
                .await
                .unwrap();
        }
        let blocked_runtime = AgentRuntime::recover(
            Arc::new(ScriptedProvider::new([])),
            Arc::new(ToolRegistry::new()),
            model(),
            started_store,
            session_id,
        )
        .await
        .unwrap();
        let (recovery_tx, mut recovery_rx) = tokio::sync::mpsc::unbounded_channel();
        blocked_runtime.publish_recovery(&recovery_tx).await;
        assert!(matches!(
            recovery_rx.recv().await,
            Some(RuntimeEvent::SessionRecovered { .. })
        ));
        assert!(matches!(
            recovery_rx.recv().await,
            Some(RuntimeEvent::RecoveryRequired { .. })
        ));
        assert!(matches!(
            blocked_runtime
                .resume_turn(&recovery_tx, CancellationToken::new())
                .await,
            Err(RuntimeError::Blocked(_))
        ));

        let resolved_store = Arc::new(InMemorySessionStore::new());
        let resolved_session: SessionId = "resolved-session".into();
        for entry in &entries[..=started_index] {
            resolved_store
                .append(&resolved_session, entry.record.clone())
                .await
                .unwrap();
        }
        let resolved_provider = Arc::new(ScriptedProvider::new([Script::Events(vec![
            started(),
            completed(StopReason::EndTurn),
        ])]));
        let resolved_executions = Arc::new(AtomicUsize::new(0));
        let mut resolved_tools = ToolRegistry::new();
        resolved_tools
            .register(CountingTool(resolved_executions.clone()))
            .unwrap();
        let resolved_runtime = AgentRuntime::recover(
            resolved_provider,
            Arc::new(resolved_tools),
            model(),
            resolved_store.clone(),
            resolved_session.clone(),
        )
        .await
        .unwrap();
        resolved_runtime
            .reconcile_tool(
                "call-1".into(),
                ReconciliationDecision::MarkSucceeded {
                    content: "manually verified".to_owned(),
                },
                &recovery_tx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(resolved_executions.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(
            resolved_runtime
                .conversation_snapshot()
                .await
                .messages()
                .len(),
            4
        );
        assert!(
            resolved_store
                .entries(&resolved_session)
                .await
                .iter()
                .any(|entry| matches!(entry.record, JournalRecord::ReconciliationResolved { .. }))
        );

        let original_execution_id = match &records[started_index] {
            JournalRecord::ToolExecutionStarted { execution_id, .. } => execution_id.clone(),
            _ => unreachable!(),
        };
        let retry_store = Arc::new(InMemorySessionStore::new());
        let retry_session: SessionId = "retry-session".into();
        for entry in &entries[..=started_index] {
            retry_store
                .append(&retry_session, entry.record.clone())
                .await
                .unwrap();
        }
        retry_store
            .append(
                &retry_session,
                JournalRecord::ReconciliationResolved {
                    tool_call_id: "call-1".into(),
                    decision: ReconciliationDecision::RetryAnyway,
                },
            )
            .await
            .unwrap();
        let retry_provider = Arc::new(ScriptedProvider::new([Script::Events(vec![
            started(),
            completed(StopReason::EndTurn),
        ])]));
        let retry_executions = Arc::new(AtomicUsize::new(0));
        let mut retry_tools = ToolRegistry::new();
        retry_tools
            .register(CountingTool(retry_executions.clone()))
            .unwrap();
        let retry_runtime = AgentRuntime::recover(
            retry_provider,
            Arc::new(retry_tools),
            model(),
            retry_store,
            retry_session,
        )
        .await
        .unwrap();
        let (retry_tx, mut retry_rx) = tokio::sync::mpsc::unbounded_channel();
        retry_runtime
            .resume_turn(&retry_tx, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(retry_executions.load(AtomicOrdering::SeqCst), 1);
        let retried_execution_id = loop {
            match retry_rx.recv().await {
                Some(RuntimeEvent::ToolStarted { execution_id, .. }) => break execution_id,
                Some(_) => continue,
                None => panic!("retry emitted no ToolStarted event"),
            }
        };
        assert_ne!(retried_execution_id, original_execution_id);

        let abandoned_store = Arc::new(InMemorySessionStore::new());
        let abandoned_session: SessionId = "abandoned-session".into();
        for entry in &entries[..=started_index] {
            abandoned_store
                .append(&abandoned_session, entry.record.clone())
                .await
                .unwrap();
        }
        let abandoned_runtime = AgentRuntime::recover(
            Arc::new(ScriptedProvider::new([])),
            Arc::new(ToolRegistry::new()),
            model(),
            abandoned_store.clone(),
            abandoned_session.clone(),
        )
        .await
        .unwrap();
        abandoned_runtime
            .reconcile_tool(
                "call-1".into(),
                ReconciliationDecision::AbandonTurn,
                &recovery_tx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            abandoned_store
                .load(&abandoned_session)
                .await
                .unwrap()
                .active_turn
                .unwrap()
                .phase,
            TurnPhase::Cancelled
        ));
    }

    #[tokio::test]
    async fn tool_call_limit_rejects_the_uncommitted_assistant_draft() {
        let provider = Arc::new(ScriptedProvider::new([Script::Events(vec![
            started(),
            ProviderEvent::ToolCallStarted {
                part: PartIndex(0),
                id: ToolCallId::new("call-1"),
                name: "echo".to_owned(),
            },
            ProviderEvent::ToolArgumentsDelta {
                part: PartIndex(0),
                delta: r#"{"text":"one"}"#.to_owned(),
            },
            ProviderEvent::ToolCallStarted {
                part: PartIndex(1),
                id: ToolCallId::new("call-2"),
                name: "echo".to_owned(),
            },
            ProviderEvent::ToolArgumentsDelta {
                part: PartIndex(1),
                delta: r#"{"text":"two"}"#.to_owned(),
            },
            completed(StopReason::ToolUse),
        ])]));
        let runtime = AgentRuntime::new(provider, Arc::new(ToolRegistry::new()), "system", model())
            .with_turn_limits(4, 1);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let error = runtime
            .run_user_turn("hello".to_owned(), &tx, CancellationToken::new())
            .await
            .unwrap_err();

        assert!(matches!(error, RuntimeError::Limit(_)));
        assert_eq!(runtime.conversation_snapshot().await.messages().len(), 1);
    }

    #[tokio::test]
    async fn model_step_limit_stops_a_tool_loop_at_a_valid_commit_point() {
        let provider = Arc::new(ScriptedProvider::new([Script::Events(vec![
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
        ])]));
        let mut tools = ToolRegistry::new();
        tools.register(EchoTool).unwrap();
        let runtime = AgentRuntime::new(provider.clone(), Arc::new(tools), "system", model())
            .with_turn_limits(1, 4);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let error = runtime
            .run_user_turn("hello".to_owned(), &tx, CancellationToken::new())
            .await
            .unwrap_err();

        assert!(matches!(error, RuntimeError::Limit(_)));
        assert_eq!(provider.requests().len(), 1);
        assert_eq!(runtime.conversation_snapshot().await.messages().len(), 3);
    }

    struct CountingTool(Arc<AtomicUsize>);

    impl Tool for CountingTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "echo".to_owned(),
                description: "count executions".to_owned(),
                parameters: json!({"type": "object"}),
                replay_class: crate::agent::ReplayClass::ReadOnly,
            }
        }

        fn execute(
            &self,
            _arguments: serde_json::Value,
            _cancel: CancellationToken,
        ) -> ToolFuture<'_> {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
            Box::pin(async {
                ToolOutcome::Completed {
                    content: "done".to_owned(),
                }
            })
        }
    }

    struct EffectfulCountingTool(Arc<AtomicUsize>);

    impl Tool for EffectfulCountingTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "effect".to_owned(),
                description: "count effectful executions".to_owned(),
                parameters: json!({"type": "object"}),
                replay_class: crate::agent::ReplayClass::Effectful,
            }
        }

        fn execute(
            &self,
            _arguments: serde_json::Value,
            _cancel: CancellationToken,
        ) -> ToolFuture<'_> {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
            Box::pin(async {
                ToolOutcome::Completed {
                    content: "effect completed".to_owned(),
                }
            })
        }
    }

    #[derive(Default)]
    struct FailOnToolStartStore {
        inner: InMemorySessionStore,
    }

    #[derive(Default)]
    struct FailOnToolOutcomeStore {
        inner: InMemorySessionStore,
    }

    impl SessionStore for FailOnToolOutcomeStore {
        fn append<'a>(
            &'a self,
            session_id: &'a SessionId,
            record: JournalRecord,
        ) -> StoreFuture<'a, JournalSequence> {
            if matches!(record, JournalRecord::ToolOutcomeRecorded { .. }) {
                return Box::pin(async {
                    Err(StoreError::Backend(
                        "injected failure after tool execution".to_owned(),
                    ))
                });
            }
            self.inner.append(session_id, record)
        }

        fn load<'a>(
            &'a self,
            session_id: &'a SessionId,
        ) -> StoreFuture<'a, crate::agent::RecoveredSession> {
            self.inner.load(session_id)
        }

        fn checkpoint<'a>(&'a self, session_id: &'a SessionId) -> StoreFuture<'a, JournalSequence> {
            self.inner.checkpoint(session_id)
        }
    }

    impl SessionStore for FailOnToolStartStore {
        fn append<'a>(
            &'a self,
            session_id: &'a SessionId,
            record: JournalRecord,
        ) -> StoreFuture<'a, JournalSequence> {
            if matches!(record, JournalRecord::ToolExecutionStarted { .. }) {
                return Box::pin(async {
                    Err(StoreError::Backend(
                        "injected failure before tool execution".to_owned(),
                    ))
                });
            }
            self.inner.append(session_id, record)
        }

        fn load<'a>(
            &'a self,
            session_id: &'a SessionId,
        ) -> StoreFuture<'a, crate::agent::RecoveredSession> {
            self.inner.load(session_id)
        }

        fn checkpoint<'a>(&'a self, session_id: &'a SessionId) -> StoreFuture<'a, JournalSequence> {
            self.inner.checkpoint(session_id)
        }
    }

    #[tokio::test]
    async fn tool_is_not_invoked_when_started_record_cannot_be_persisted() {
        let provider = Arc::new(ScriptedProvider::new([Script::Events(vec![
            started(),
            ProviderEvent::ToolCallStarted {
                part: PartIndex(0),
                id: ToolCallId::new("call-1"),
                name: "echo".to_owned(),
            },
            ProviderEvent::ToolArgumentsDelta {
                part: PartIndex(0),
                delta: "{}".to_owned(),
            },
            completed(StopReason::ToolUse),
        ])]));
        let executions = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(CountingTool(executions.clone())).unwrap();
        let runtime = AgentRuntime::new(provider, Arc::new(registry), "system", model())
            .with_session_store(Arc::new(FailOnToolStartStore::default()));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let error = runtime
            .run_user_turn("echo".to_owned(), &tx, CancellationToken::new())
            .await
            .unwrap_err();

        assert!(matches!(error, RuntimeError::Persistence(_)));
        assert_eq!(executions.load(AtomicOrdering::SeqCst), 0);
        assert!(matches!(
            runtime.resume_turn(&tx, CancellationToken::new()).await,
            Err(RuntimeError::Blocked(_))
        ));
    }

    #[tokio::test]
    async fn outcome_persistence_failure_requires_reconciliation_without_rerunning() {
        let provider = Arc::new(ScriptedProvider::new([Script::Events(vec![
            started(),
            ProviderEvent::ToolCallStarted {
                part: PartIndex(0),
                id: ToolCallId::new("call-1"),
                name: "echo".to_owned(),
            },
            ProviderEvent::ToolArgumentsDelta {
                part: PartIndex(0),
                delta: "{}".to_owned(),
            },
            completed(StopReason::ToolUse),
        ])]));
        let executions = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(CountingTool(executions.clone())).unwrap();
        let store = Arc::new(FailOnToolOutcomeStore::default());
        let runtime = AgentRuntime::new(provider, Arc::new(registry), "system", model())
            .with_session_store(store.clone());
        let session_id = runtime.session_id().clone();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let error = runtime
            .run_user_turn("echo".to_owned(), &tx, CancellationToken::new())
            .await
            .unwrap_err();

        assert!(matches!(error, RuntimeError::Persistence(_)));
        assert_eq!(executions.load(AtomicOrdering::SeqCst), 1);
        assert!(matches!(
            store
                .load(&session_id)
                .await
                .unwrap()
                .active_turn
                .unwrap()
                .phase,
            TurnPhase::NeedsReconciliation
        ));
        assert!(matches!(
            runtime.resume_turn(&tx, CancellationToken::new()).await,
            Err(RuntimeError::Blocked(_))
        ));
        assert_eq!(executions.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn effectful_tools_execute_without_a_builtin_gate() {
        let provider = Arc::new(ScriptedProvider::new([
            Script::Events(vec![
                started(),
                ProviderEvent::ToolCallStarted {
                    part: PartIndex(0),
                    id: ToolCallId::new("call-never"),
                    name: "effect".to_owned(),
                },
                ProviderEvent::ToolArgumentsDelta {
                    part: PartIndex(0),
                    delta: "{}".to_owned(),
                },
                completed(StopReason::ToolUse),
            ]),
            Script::Events(vec![started(), completed(StopReason::EndTurn)]),
        ]));
        let executions = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolRegistry::new();
        tools
            .register(EffectfulCountingTool(executions.clone()))
            .unwrap();
        let runtime = AgentRuntime::new(provider, Arc::new(tools), "system", model());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        runtime
            .run_user_turn("perform effect".to_owned(), &tx, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(executions.load(AtomicOrdering::SeqCst), 1);
        let conversation = runtime.conversation_snapshot().await;
        let Message::ToolResult(result) = &conversation.messages()[2] else {
            panic!("expected rejected tool result");
        };
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn completed_runtime_session_reopens_from_local_storage() {
        let root = std::env::temp_dir().join(format!(
            "rua-runtime-recovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let provider = Arc::new(ScriptedProvider::new([Script::Events(vec![
            started(),
            ProviderEvent::TextDelta {
                part: PartIndex(0),
                delta: "persisted".to_owned(),
            },
            completed(StopReason::EndTurn),
        ])]));
        let store = Arc::new(LocalSessionStore::new(&root));
        let runtime = AgentRuntime::new(provider, Arc::new(ToolRegistry::new()), "system", model())
            .with_session_store(store.clone());
        let session_id = runtime.session_id().clone();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        runtime
            .run_user_turn("hello".to_owned(), &tx, CancellationToken::new())
            .await
            .unwrap();
        drop(runtime);
        drop(store);

        let reopened = LocalSessionStore::new(&root)
            .load(&session_id)
            .await
            .unwrap();
        assert_eq!(reopened.conversation.messages().len(), 2);
        assert!(matches!(
            reopened.active_turn.unwrap().phase,
            TurnPhase::Completed
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cancellation_has_one_terminal_outcome_and_is_not_resumable() {
        let runtime = AgentRuntime::new(
            Arc::new(ScriptedProvider::new([])),
            Arc::new(ToolRegistry::new()),
            "system",
            model(),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = runtime
            .run_user_turn("hello".to_owned(), &tx, cancel)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            RuntimeError::Provider(ProviderError {
                kind: ProviderErrorKind::Cancelled,
                ..
            })
        ));
        let mut cancelled = 0;
        let mut failed = 0;
        while let Ok(event) = rx.try_recv() {
            match event {
                RuntimeEvent::TurnCancelled { .. } => cancelled += 1,
                RuntimeEvent::TurnFailed { .. } => failed += 1,
                _ => {}
            }
        }
        assert_eq!(cancelled, 1);
        assert_eq!(failed, 0);
        assert!(matches!(
            runtime.resume_turn(&tx, CancellationToken::new()).await,
            Err(RuntimeError::NoResumableTurn)
        ));
    }

    #[tokio::test]
    async fn rejects_tools_before_requesting_an_unsupported_model() {
        let provider = Arc::new(
            ScriptedProvider::new([]).with_capabilities(ModelCapabilities {
                tools: false,
                reasoning: true,
                image_input: false,
            }),
        );
        let mut tools = ToolRegistry::new();
        tools.register(EchoTool).unwrap();
        let runtime = AgentRuntime::new(provider.clone(), Arc::new(tools), "system", model());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let error = runtime
            .run_user_turn("use a tool".to_owned(), &tx, CancellationToken::new())
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            RuntimeError::Provider(ProviderError {
                kind: ProviderErrorKind::UnsupportedCapability,
                ..
            })
        ));
        assert!(provider.requests().is_empty());
        assert!(matches!(
            runtime.resume_turn(&tx, CancellationToken::new()).await,
            Err(RuntimeError::NoResumableTurn)
        ));
    }
}
