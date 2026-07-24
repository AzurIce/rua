use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use super::Conversation;
use super::types::{
    AttemptId, ConversationRevision, ExecutionId, InstructionSet, Message, MessageId, ReplayClass,
    SessionId, StepId, StopReason, ToolCallId, TurnId,
};

pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StoreError>> + Send + 'a>>;

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct JournalSequence(pub u64);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub sequence: JournalSequence,
    pub record: JournalRecord,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JournalRecord {
    SessionCreated {
        instructions: InstructionSet,
    },
    ConversationAppended {
        message: Message,
        expected_revision: ConversationRevision,
        resulting_revision: ConversationRevision,
    },
    TurnOpened {
        turn_id: TurnId,
        stable_revision: ConversationRevision,
    },
    ModelStepPrepared {
        turn_id: TurnId,
        step_id: StepId,
        revision: ConversationRevision,
    },
    AttemptStarted {
        turn_id: TurnId,
        step_id: StepId,
        attempt_id: AttemptId,
    },
    AttemptFailed {
        turn_id: TurnId,
        step_id: StepId,
        attempt_id: AttemptId,
        error: String,
        retryable: bool,
    },
    AssistantCommitted {
        turn_id: TurnId,
        step_id: StepId,
        message_id: MessageId,
        resulting_revision: ConversationRevision,
    },
    ToolExecutionIntended {
        turn_id: TurnId,
        step_id: StepId,
        tool_call_id: ToolCallId,
        name: String,
        arguments: Value,
        replay_class: ReplayClass,
    },
    ToolApprovalRequested {
        tool_call_id: ToolCallId,
    },
    ToolApprovalResolved {
        tool_call_id: ToolCallId,
        approved: bool,
        reason: Option<String>,
    },
    ToolExecutionStarted {
        tool_call_id: ToolCallId,
        execution_id: ExecutionId,
    },
    ToolOutcomeRecorded {
        tool_call_id: ToolCallId,
        execution_id: ExecutionId,
        outcome: RecordedToolOutcome,
    },
    ToolResultCommitted {
        tool_call_id: ToolCallId,
        message_id: MessageId,
        resulting_revision: ConversationRevision,
    },
    ReconciliationResolved {
        tool_call_id: ToolCallId,
        decision: ReconciliationDecision,
    },
    SnapshotCreated {
        last_sequence: JournalSequence,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordedToolOutcome {
    Completed { content: String },
    FailedKnown { message: String },
    OutcomeUnknown { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconciliationDecision {
    MarkSucceeded { content: String },
    MarkFailed { message: String },
    RetryAnyway,
    AbandonTurn,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveredSession {
    pub session_id: SessionId,
    pub conversation: Conversation,
    pub active_turn: Option<DurableTurn>,
    pub last_sequence: JournalSequence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DurableTurn {
    pub turn_id: TurnId,
    pub phase: TurnPhase,
    pub stable_revision: ConversationRevision,
    pub next_step: u32,
    pub attempts: Vec<AttemptRecord>,
    pub pending_tools: Vec<DurableToolCall>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnPhase {
    AwaitingModel,
    WaitingToRetry,
    AwaitingApproval,
    ExecutingTools,
    NeedsReconciliation,
    Completed,
    Failed { recoverable: bool },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub step_id: StepId,
    pub attempt_id: AttemptId,
    pub failure: Option<AttemptFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptFailure {
    pub error: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DurableToolCall {
    pub tool_call_id: ToolCallId,
    pub name: String,
    pub arguments: Value,
    pub replay_class: ReplayClass,
    pub approval: ToolApprovalState,
    #[serde(default)]
    pub execution_count: u64,
    pub execution_id: Option<ExecutionId>,
    pub outcome: Option<RecordedToolOutcome>,
    pub result_committed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolApprovalState {
    NotRequested,
    Pending,
    Approved,
    Rejected { reason: String },
}

pub trait SessionStore: Send + Sync {
    fn append<'a>(
        &'a self,
        session_id: &'a SessionId,
        record: JournalRecord,
    ) -> StoreFuture<'a, JournalSequence>;

    fn load<'a>(&'a self, session_id: &'a SessionId) -> StoreFuture<'a, RecoveredSession>;

    fn checkpoint<'a>(&'a self, session_id: &'a SessionId) -> StoreFuture<'a, JournalSequence>;
}

#[derive(Default)]
pub struct InMemorySessionStore {
    entries: Mutex<HashMap<SessionId, Vec<JournalEntry>>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn entries(&self, session_id: &SessionId) -> Vec<JournalEntry> {
        self.entries
            .lock()
            .await
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }
}

impl SessionStore for InMemorySessionStore {
    fn append<'a>(
        &'a self,
        session_id: &'a SessionId,
        record: JournalRecord,
    ) -> StoreFuture<'a, JournalSequence> {
        Box::pin(async move {
            let mut sessions = self.entries.lock().await;
            let entries = sessions.entry(session_id.clone()).or_default();
            let sequence = JournalSequence(
                u64::try_from(entries.len())
                    .map_err(|_| StoreError::SequenceOverflow)?
                    .checked_add(1)
                    .ok_or(StoreError::SequenceOverflow)?,
            );
            entries.push(JournalEntry {
                schema_version: 1,
                session_id: session_id.clone(),
                sequence,
                record,
            });
            Ok(sequence)
        })
    }

    fn load<'a>(&'a self, session_id: &'a SessionId) -> StoreFuture<'a, RecoveredSession> {
        Box::pin(async move {
            let entries = self.entries(session_id).await;
            replay_session(session_id, &entries)
        })
    }

    fn checkpoint<'a>(&'a self, session_id: &'a SessionId) -> StoreFuture<'a, JournalSequence> {
        Box::pin(async move {
            let recovered = self.load(session_id).await?;
            let last_sequence = recovered.last_sequence;
            self.append(session_id, JournalRecord::SnapshotCreated { last_sequence })
                .await
        })
    }
}

pub fn replay_session(
    session_id: &SessionId,
    entries: &[JournalEntry],
) -> Result<RecoveredSession, StoreError> {
    replay_entries(session_id, None, None, JournalSequence::default(), entries)
}

pub fn replay_after(
    snapshot: RecoveredSession,
    entries: &[JournalEntry],
) -> Result<RecoveredSession, StoreError> {
    let session_id = snapshot.session_id.clone();
    replay_entries(
        &session_id,
        Some(snapshot.conversation),
        snapshot.active_turn,
        snapshot.last_sequence,
        entries,
    )
}

fn replay_entries(
    session_id: &SessionId,
    mut conversation: Option<Conversation>,
    mut active_turn: Option<DurableTurn>,
    mut last_sequence: JournalSequence,
    entries: &[JournalEntry],
) -> Result<RecoveredSession, StoreError> {
    for entry in entries {
        let expected_sequence = last_sequence
            .0
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?;
        if entry.schema_version != 1 {
            return Err(StoreError::UnsupportedSchema(entry.schema_version));
        }
        if &entry.session_id != session_id {
            return Err(StoreError::SessionMismatch {
                expected: session_id.clone(),
                actual: entry.session_id.clone(),
            });
        }
        if entry.sequence != JournalSequence(expected_sequence) {
            return Err(StoreError::SequenceGap {
                expected: JournalSequence(expected_sequence),
                actual: entry.sequence,
            });
        }

        match &entry.record {
            JournalRecord::SessionCreated { instructions } => {
                if conversation.is_some() || entry.sequence != JournalSequence(1) {
                    return Err(StoreError::InvalidJournal(
                        "SessionCreated must be the first and only session bootstrap".to_owned(),
                    ));
                }
                conversation = Some(Conversation::new(instructions.clone()));
            }
            JournalRecord::ConversationAppended {
                message,
                expected_revision,
                resulting_revision,
            } => {
                let conversation = conversation_mut(&mut conversation)?;
                if conversation.revision() != *expected_revision {
                    return Err(StoreError::RevisionMismatch {
                        expected: conversation.revision(),
                        actual: *expected_revision,
                    });
                }
                let actual = conversation
                    .append(message.clone())
                    .map_err(|error| StoreError::InvalidJournal(error.to_string()))?;
                if actual != *resulting_revision {
                    return Err(StoreError::RevisionMismatch {
                        expected: actual,
                        actual: *resulting_revision,
                    });
                }
            }
            JournalRecord::TurnOpened {
                turn_id,
                stable_revision,
            } => {
                let conversation = conversation_ref(&conversation)?;
                if conversation.revision() != *stable_revision {
                    return Err(StoreError::RevisionMismatch {
                        expected: conversation.revision(),
                        actual: *stable_revision,
                    });
                }
                if active_turn
                    .as_ref()
                    .is_some_and(|turn| !is_terminal(&turn.phase))
                {
                    return Err(StoreError::InvalidJournal(
                        "cannot open a second active turn".to_owned(),
                    ));
                }
                active_turn = Some(DurableTurn {
                    turn_id: turn_id.clone(),
                    phase: TurnPhase::AwaitingModel,
                    stable_revision: *stable_revision,
                    next_step: 0,
                    attempts: Vec::new(),
                    pending_tools: Vec::new(),
                });
            }
            JournalRecord::ModelStepPrepared {
                turn_id,
                step_id,
                revision,
            } => {
                let conversation = conversation_ref(&conversation)?;
                if conversation.revision() != *revision {
                    return Err(StoreError::RevisionMismatch {
                        expected: conversation.revision(),
                        actual: *revision,
                    });
                }
                let turn = active_turn_mut(&mut active_turn, turn_id)?;
                turn.phase = TurnPhase::AwaitingModel;
                turn.stable_revision = *revision;
                turn.next_step = step_number(turn_id, step_id)?;
            }
            JournalRecord::AttemptStarted {
                turn_id,
                step_id,
                attempt_id,
            } => {
                let turn = active_turn_mut(&mut active_turn, turn_id)?;
                if turn
                    .attempts
                    .iter()
                    .any(|attempt| attempt.attempt_id == *attempt_id)
                {
                    return Err(StoreError::InvalidJournal(format!(
                        "duplicate attempt id: {attempt_id}"
                    )));
                }
                turn.attempts.push(AttemptRecord {
                    step_id: step_id.clone(),
                    attempt_id: attempt_id.clone(),
                    failure: None,
                });
                turn.phase = TurnPhase::AwaitingModel;
            }
            JournalRecord::AttemptFailed {
                turn_id,
                step_id,
                attempt_id,
                error,
                retryable,
            } => {
                let turn = active_turn_mut(&mut active_turn, turn_id)?;
                let attempt = turn
                    .attempts
                    .iter_mut()
                    .find(|attempt| {
                        attempt.step_id == *step_id && attempt.attempt_id == *attempt_id
                    })
                    .ok_or_else(|| {
                        StoreError::InvalidJournal(format!(
                            "AttemptFailed references unknown attempt: {attempt_id}"
                        ))
                    })?;
                if attempt.failure.is_some() {
                    return Err(StoreError::InvalidJournal(format!(
                        "attempt already failed: {attempt_id}"
                    )));
                }
                attempt.failure = Some(AttemptFailure {
                    error: error.clone(),
                    retryable: *retryable,
                });
                if *retryable {
                    turn.phase = TurnPhase::WaitingToRetry;
                }
            }
            JournalRecord::AssistantCommitted {
                turn_id,
                step_id: _,
                message_id,
                resulting_revision,
            } => {
                let conversation = conversation_ref(&conversation)?;
                validate_commit_marker(conversation, message_id, *resulting_revision, "assistant")?;
                let stop_reason = match conversation.messages().last() {
                    Some(Message::Assistant(message)) => &message.stop_reason,
                    _ => unreachable!("validated assistant marker"),
                };
                let turn = active_turn_mut(&mut active_turn, turn_id)?;
                turn.phase = if *stop_reason == StopReason::ToolUse {
                    TurnPhase::ExecutingTools
                } else {
                    TurnPhase::Completed
                };
            }
            JournalRecord::ToolExecutionIntended {
                turn_id,
                step_id: _,
                tool_call_id,
                name,
                arguments,
                replay_class,
            } => {
                let turn = active_turn_mut(&mut active_turn, turn_id)?;
                if turn
                    .pending_tools
                    .iter()
                    .any(|tool| tool.tool_call_id == *tool_call_id)
                {
                    return Err(StoreError::InvalidJournal(format!(
                        "duplicate tool intent: {tool_call_id}"
                    )));
                }
                turn.phase = TurnPhase::ExecutingTools;
                turn.pending_tools.push(DurableToolCall {
                    tool_call_id: tool_call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    replay_class: replay_class.clone(),
                    approval: ToolApprovalState::NotRequested,
                    execution_count: 0,
                    execution_id: None,
                    outcome: None,
                    result_committed: false,
                });
            }
            JournalRecord::ToolApprovalRequested { tool_call_id } => {
                let tool = pending_tool_mut(&mut active_turn, tool_call_id)?;
                if tool.approval != ToolApprovalState::NotRequested {
                    return Err(StoreError::InvalidJournal(format!(
                        "approval already requested for tool: {tool_call_id}"
                    )));
                }
                tool.approval = ToolApprovalState::Pending;
                active_turn.as_mut().expect("active tool turn").phase = TurnPhase::AwaitingApproval;
            }
            JournalRecord::ToolApprovalResolved {
                tool_call_id,
                approved,
                reason,
            } => {
                let tool = pending_tool_mut(&mut active_turn, tool_call_id)?;
                if tool.approval != ToolApprovalState::Pending {
                    return Err(StoreError::InvalidJournal(format!(
                        "approval resolution without pending request: {tool_call_id}"
                    )));
                }
                if *approved {
                    tool.approval = ToolApprovalState::Approved;
                } else {
                    let message = reason
                        .clone()
                        .unwrap_or_else(|| "tool execution rejected".to_owned());
                    tool.approval = ToolApprovalState::Rejected {
                        reason: message.clone(),
                    };
                    tool.outcome = Some(RecordedToolOutcome::FailedKnown { message });
                }
                let turn = active_turn.as_mut().expect("active tool turn");
                turn.phase = if turn
                    .pending_tools
                    .iter()
                    .any(|tool| tool.approval == ToolApprovalState::Pending)
                {
                    TurnPhase::AwaitingApproval
                } else {
                    TurnPhase::ExecutingTools
                };
            }
            JournalRecord::ToolExecutionStarted {
                tool_call_id,
                execution_id,
            } => {
                let tool = pending_tool_mut(&mut active_turn, tool_call_id)?;
                if matches!(
                    tool.approval,
                    ToolApprovalState::Pending | ToolApprovalState::Rejected { .. }
                ) {
                    return Err(StoreError::InvalidJournal(format!(
                        "tool execution started without approval: {tool_call_id}"
                    )));
                }
                if tool.execution_id.is_some() {
                    return Err(StoreError::InvalidJournal(format!(
                        "tool already started: {tool_call_id}"
                    )));
                }
                tool.execution_count = tool.execution_count.checked_add(1).ok_or_else(|| {
                    StoreError::InvalidJournal(format!(
                        "tool execution counter overflow: {tool_call_id}"
                    ))
                })?;
                tool.execution_id = Some(execution_id.clone());
            }
            JournalRecord::ToolOutcomeRecorded {
                tool_call_id,
                execution_id,
                outcome,
            } => {
                let tool = pending_tool_mut(&mut active_turn, tool_call_id)?;
                if tool.execution_id.as_ref() != Some(execution_id) {
                    return Err(StoreError::InvalidJournal(format!(
                        "tool outcome execution id mismatch: {tool_call_id}"
                    )));
                }
                if tool.outcome.is_some() {
                    return Err(StoreError::InvalidJournal(format!(
                        "tool outcome already recorded: {tool_call_id}"
                    )));
                }
                tool.outcome = Some(outcome.clone());
            }
            JournalRecord::ToolResultCommitted {
                tool_call_id,
                message_id,
                resulting_revision,
            } => {
                let conversation = conversation_ref(&conversation)?;
                validate_commit_marker(
                    conversation,
                    message_id,
                    *resulting_revision,
                    "tool result",
                )?;
                let tool = pending_tool_mut(&mut active_turn, tool_call_id)?;
                if tool.outcome.is_none() {
                    return Err(StoreError::InvalidJournal(format!(
                        "tool result committed without recorded outcome: {tool_call_id}"
                    )));
                }
                tool.result_committed = true;
                if let Some(turn) = active_turn.as_mut()
                    && turn.pending_tools.iter().all(|tool| tool.result_committed)
                {
                    turn.phase = TurnPhase::AwaitingModel;
                    turn.next_step = turn
                        .next_step
                        .checked_add(1)
                        .ok_or(StoreError::StepOverflow)?;
                }
            }
            JournalRecord::ReconciliationResolved {
                tool_call_id,
                decision,
            } => match decision {
                ReconciliationDecision::MarkSucceeded { content } => {
                    let tool = pending_tool_mut(&mut active_turn, tool_call_id)?;
                    tool.outcome = Some(RecordedToolOutcome::Completed {
                        content: content.clone(),
                    });
                    active_turn.as_mut().expect("active tool turn").phase =
                        TurnPhase::ExecutingTools;
                }
                ReconciliationDecision::MarkFailed { message } => {
                    let tool = pending_tool_mut(&mut active_turn, tool_call_id)?;
                    tool.outcome = Some(RecordedToolOutcome::FailedKnown {
                        message: message.clone(),
                    });
                    active_turn.as_mut().expect("active tool turn").phase =
                        TurnPhase::ExecutingTools;
                }
                ReconciliationDecision::RetryAnyway => {
                    let tool = pending_tool_mut(&mut active_turn, tool_call_id)?;
                    tool.execution_id = None;
                    tool.outcome = None;
                    active_turn.as_mut().expect("active tool turn").phase =
                        TurnPhase::ExecutingTools;
                }
                ReconciliationDecision::AbandonTurn => {
                    pending_tool_mut(&mut active_turn, tool_call_id)?;
                    active_turn.as_mut().expect("active tool turn").phase = TurnPhase::Cancelled;
                }
            },
            JournalRecord::SnapshotCreated {
                last_sequence: snapshot_sequence,
            } => {
                let expected = JournalSequence(entry.sequence.0.saturating_sub(1));
                if *snapshot_sequence != expected {
                    return Err(StoreError::InvalidJournal(format!(
                        "snapshot marker expected sequence {:?}, got {:?}",
                        expected, snapshot_sequence
                    )));
                }
            }
            JournalRecord::TurnCompleted { turn_id } => {
                active_turn_mut(&mut active_turn, turn_id)?.phase = TurnPhase::Completed;
            }
            JournalRecord::TurnFailed {
                turn_id,
                error: _,
                recoverable,
            } => {
                active_turn_mut(&mut active_turn, turn_id)?.phase = TurnPhase::Failed {
                    recoverable: *recoverable,
                };
            }
            JournalRecord::TurnCancelled { turn_id } => {
                active_turn_mut(&mut active_turn, turn_id)?.phase = TurnPhase::Cancelled;
            }
        }

        last_sequence = entry.sequence;
    }

    let conversation = conversation.ok_or(StoreError::MissingSessionCreated)?;
    if let Some(turn) = active_turn.as_mut()
        && !is_terminal(&turn.phase)
        && turn.pending_tools.iter().any(|tool| {
            !tool.result_committed
                && (tool.execution_id.is_some() && tool.outcome.is_none()
                    || matches!(
                        tool.outcome,
                        Some(RecordedToolOutcome::OutcomeUnknown { .. })
                    ))
        })
    {
        turn.phase = TurnPhase::NeedsReconciliation;
    }

    Ok(RecoveredSession {
        session_id: session_id.clone(),
        conversation,
        active_turn,
        last_sequence,
    })
}

fn conversation_ref(conversation: &Option<Conversation>) -> Result<&Conversation, StoreError> {
    conversation
        .as_ref()
        .ok_or(StoreError::MissingSessionCreated)
}

fn conversation_mut(
    conversation: &mut Option<Conversation>,
) -> Result<&mut Conversation, StoreError> {
    conversation
        .as_mut()
        .ok_or(StoreError::MissingSessionCreated)
}

fn active_turn_mut<'a>(
    active_turn: &'a mut Option<DurableTurn>,
    turn_id: &TurnId,
) -> Result<&'a mut DurableTurn, StoreError> {
    let turn = active_turn
        .as_mut()
        .ok_or_else(|| StoreError::InvalidJournal("record requires an active turn".to_owned()))?;
    if turn.turn_id != *turn_id {
        return Err(StoreError::InvalidJournal(format!(
            "turn id mismatch: expected {}, got {turn_id}",
            turn.turn_id
        )));
    }
    Ok(turn)
}

fn pending_tool_mut<'a>(
    active_turn: &'a mut Option<DurableTurn>,
    tool_call_id: &ToolCallId,
) -> Result<&'a mut DurableToolCall, StoreError> {
    active_turn
        .as_mut()
        .and_then(|turn| {
            turn.pending_tools
                .iter_mut()
                .find(|tool| tool.tool_call_id == *tool_call_id)
        })
        .ok_or_else(|| {
            StoreError::InvalidJournal(format!(
                "record references unknown tool call: {tool_call_id}"
            ))
        })
}

fn validate_commit_marker(
    conversation: &Conversation,
    message_id: &MessageId,
    resulting_revision: ConversationRevision,
    kind: &str,
) -> Result<(), StoreError> {
    if conversation.revision() != resulting_revision
        || conversation.messages().last().map(Message::id) != Some(message_id)
    {
        return Err(StoreError::InvalidJournal(format!(
            "{kind} commit marker does not match conversation head"
        )));
    }
    match (kind, conversation.messages().last()) {
        ("assistant", Some(Message::Assistant(_)))
        | ("tool result", Some(Message::ToolResult(_))) => Ok(()),
        _ => Err(StoreError::InvalidJournal(format!(
            "{kind} commit marker has the wrong message kind"
        ))),
    }
}

fn is_terminal(phase: &TurnPhase) -> bool {
    matches!(
        phase,
        TurnPhase::Completed | TurnPhase::Failed { .. } | TurnPhase::Cancelled
    )
}

fn step_number(turn_id: &TurnId, step_id: &StepId) -> Result<u32, StoreError> {
    let prefix = format!("{turn_id}-step-");
    step_id
        .as_str()
        .strip_prefix(&prefix)
        .ok_or_else(|| {
            StoreError::InvalidJournal(format!(
                "step id {step_id} does not belong to turn {turn_id}"
            ))
        })?
        .parse::<u32>()
        .map_err(|error| StoreError::InvalidJournal(error.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("journal sequence overflow")]
    SequenceOverflow,
    #[error("journal step counter overflow")]
    StepOverflow,
    #[error("unsupported journal schema version: {0}")]
    UnsupportedSchema(u32),
    #[error("journal session mismatch: expected {expected}, got {actual}")]
    SessionMismatch {
        expected: SessionId,
        actual: SessionId,
    },
    #[error("journal sequence gap: expected {expected:?}, got {actual:?}")]
    SequenceGap {
        expected: JournalSequence,
        actual: JournalSequence,
    },
    #[error("journal conversation revision mismatch: expected {expected:?}, got {actual:?}")]
    RevisionMismatch {
        expected: ConversationRevision,
        actual: ConversationRevision,
    },
    #[error("journal is missing SessionCreated")]
    MissingSessionCreated,
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),
    #[error("invalid session id: {0}")]
    InvalidSessionId(String),
    #[error("session is already locked: {session_id}: {message}")]
    SessionLocked {
        session_id: SessionId,
        message: String,
    },
    #[error("invalid session manifest: {0}")]
    InvalidManifest(String),
    #[error("journal frame is too large")]
    FrameTooLarge,
    #[error("corrupt journal frame at byte {offset}: {message}")]
    CorruptFrame { offset: u64, message: String },
    #[error(
        "journal has an incomplete crash tail after byte {valid_bytes} (total {total_bytes}); run explicit session repair"
    )]
    IncompleteTail { valid_bytes: u64, total_bytes: u64 },
    #[error("invalid journal: {0}")]
    InvalidJournal(String),
    #[error("session store failure: {0}")]
    Backend(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn assigns_monotonic_sequences_per_session() {
        let store = InMemorySessionStore::new();
        let first: SessionId = "first".into();
        let second: SessionId = "second".into();

        assert_eq!(
            store
                .append(
                    &first,
                    JournalRecord::SessionCreated {
                        instructions: InstructionSet::new("system"),
                    },
                )
                .await
                .unwrap(),
            JournalSequence(1)
        );
        assert_eq!(
            store
                .append(
                    &first,
                    JournalRecord::TurnCompleted {
                        turn_id: "turn-1".into(),
                    },
                )
                .await
                .unwrap(),
            JournalSequence(2)
        );
        assert_eq!(
            store
                .append(
                    &second,
                    JournalRecord::SessionCreated {
                        instructions: InstructionSet::new("other"),
                    },
                )
                .await
                .unwrap(),
            JournalSequence(1)
        );
    }
}
