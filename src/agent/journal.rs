use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use super::Conversation;
use super::session_tree::{
    ConversationHead, DirectoryChangeSource, SessionEntry, SessionEntryPayload, SessionTree,
};
use super::tools::ToolEffect;
use super::types::{
    AttemptId, ConversationRevision, DirectorySnapshot, ExecutionId, HeadRevision, InstructionSet,
    Message, MessageId, ReplayClass, SessionId, SessionLocator, StableHead, StepId, StopReason,
    ToolCallId, TurnContextSnapshot, TurnId,
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
        #[serde(default)]
        initial_directory: Option<DirectorySnapshot>,
    },
    CwdChanged {
        input: String,
        from: DirectorySnapshot,
        to: DirectorySnapshot,
        source: DirectoryChangeSource,
    },
    SessionEntryAppended {
        entry: SessionEntry,
        expected_head: ConversationHead,
        resulting_head_revision: HeadRevision,
    },
    ConversationHeadMoved {
        expected_head: ConversationHead,
        target_entry_id: Option<super::types::EntryId>,
        resulting_head_revision: HeadRevision,
    },
    SessionRelocationPrepared {
        from: SessionLocator,
        to: SessionLocator,
        locator_generation: u64,
    },
    SessionRelocationCommitted {
        to: SessionLocator,
        locator_generation: u64,
    },
    ConversationAppended {
        message: Message,
        expected_revision: ConversationRevision,
        resulting_revision: ConversationRevision,
    },
    TurnOpened {
        turn_id: TurnId,
        stable_revision: ConversationRevision,
        #[serde(default)]
        stable_head: Option<StableHead>,
    },
    ModelStepPrepared {
        turn_id: TurnId,
        step_id: StepId,
        revision: ConversationRevision,
        #[serde(default)]
        context: Option<TurnContextSnapshot>,
        #[serde(default)]
        stable_head: Option<StableHead>,
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
        #[serde(default)]
        entry_id: Option<super::types::EntryId>,
        #[serde(default)]
        resulting_head_revision: Option<HeadRevision>,
    },
    ToolExecutionIntended {
        turn_id: TurnId,
        step_id: StepId,
        tool_call_id: ToolCallId,
        name: String,
        arguments: Value,
        replay_class: ReplayClass,
        #[serde(default)]
        directory: Option<DirectorySnapshot>,
        #[serde(default)]
        stable_head: Option<StableHead>,
    },
    /// Legacy frame retained only so sessions written before D0004's direct
    /// execution model remain readable. New runtimes never append it.
    #[serde(rename = "ToolApprovalRequested")]
    LegacyToolApprovalRequested {
        tool_call_id: ToolCallId,
    },
    /// Legacy frame retained for backward-compatible replay.
    #[serde(rename = "ToolApprovalResolved")]
    LegacyToolApprovalResolved {
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
        #[serde(default)]
        entry_id: Option<super::types::EntryId>,
        #[serde(default)]
        resulting_head_revision: Option<HeadRevision>,
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
    CompletedWithEffect { content: String, effect: ToolEffect },
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
    #[serde(default)]
    pub tree: SessionTree,
    #[serde(default)]
    pub initial_directory: Option<DirectorySnapshot>,
    #[serde(default)]
    pub directory: Option<DirectorySnapshot>,
    pub active_turn: Option<DurableTurn>,
    #[serde(default)]
    pub pending_relocation: Option<DurableRelocation>,
    pub last_sequence: JournalSequence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableRelocation {
    pub from: SessionLocator,
    pub to: SessionLocator,
    pub locator_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DurableTurn {
    pub turn_id: TurnId,
    pub phase: TurnPhase,
    pub stable_revision: ConversationRevision,
    #[serde(default)]
    pub stable_head: Option<StableHead>,
    pub next_step: u32,
    pub attempts: Vec<AttemptRecord>,
    pub pending_tools: Vec<DurableToolCall>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnPhase {
    AwaitingModel,
    WaitingToRetry,
    /// Legacy persisted phase; recovery normalizes it to `ExecutingTools`.
    #[serde(rename = "AwaitingApproval")]
    LegacyAwaitingApproval,
    ExecutingTools,
    NeedsReconciliation,
    Completed,
    Failed {
        recoverable: bool,
    },
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
    #[serde(default)]
    pub directory: Option<DirectorySnapshot>,
    /// Legacy replay metadata. It is not consulted for new tool calls.
    #[serde(rename = "approval", default)]
    pub legacy_approval: LegacyToolApprovalState,
    #[serde(default)]
    pub execution_count: u64,
    pub execution_id: Option<ExecutionId>,
    pub outcome: Option<RecordedToolOutcome>,
    #[serde(default)]
    pub effect_committed: bool,
    pub result_committed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[doc(hidden)]
#[derive(Default)]
pub enum LegacyToolApprovalState {
    #[default]
    NotRequested,
    Pending,
    Approved,
    Rejected {
        reason: String,
    },
}

pub trait SessionStore: Send + Sync {
    fn append<'a>(
        &'a self,
        session_id: &'a SessionId,
        record: JournalRecord,
    ) -> StoreFuture<'a, JournalSequence>;

    fn load<'a>(&'a self, session_id: &'a SessionId) -> StoreFuture<'a, RecoveredSession>;

    fn checkpoint<'a>(&'a self, session_id: &'a SessionId) -> StoreFuture<'a, JournalSequence>;

    fn locator<'a>(
        &'a self,
        _session_id: &'a SessionId,
    ) -> StoreFuture<'a, Option<SessionLocator>> {
        Box::pin(async { Ok(None) })
    }

    fn relocate<'a>(
        &'a self,
        _session_id: &'a SessionId,
        _target: SessionLocator,
    ) -> StoreFuture<'a, SessionLocator> {
        Box::pin(async {
            Err(StoreError::Backend(
                "session store does not support relocation".to_owned(),
            ))
        })
    }

    fn complete_relocation<'a>(
        &'a self,
        session_id: &'a SessionId,
        pending: DurableRelocation,
    ) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.append(
                session_id,
                JournalRecord::SessionRelocationCommitted {
                    to: pending.to,
                    locator_generation: pending.locator_generation,
                },
            )
            .await?;
            Ok(())
        })
    }
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
    replay_entries(
        session_id,
        ReplayState::default(),
        JournalSequence::default(),
        entries,
    )
}

pub fn replay_after(
    snapshot: RecoveredSession,
    entries: &[JournalEntry],
) -> Result<RecoveredSession, StoreError> {
    let session_id = snapshot.session_id.clone();
    let mut tree = snapshot.tree;
    if tree.entries().is_empty() && !snapshot.conversation.messages().is_empty() {
        tree = SessionTree::from_legacy_messages(snapshot.conversation.messages())
            .map_err(|error| StoreError::InvalidJournal(error.to_string()))?;
    }
    replay_entries(
        &session_id,
        ReplayState {
            conversation: Some(snapshot.conversation),
            initial_directory: snapshot.initial_directory,
            directory: snapshot.directory,
            tree,
            active_turn: snapshot.active_turn,
            pending_relocation: snapshot.pending_relocation,
        },
        snapshot.last_sequence,
        entries,
    )
}

#[derive(Default)]
struct ReplayState {
    conversation: Option<Conversation>,
    initial_directory: Option<DirectorySnapshot>,
    directory: Option<DirectorySnapshot>,
    tree: SessionTree,
    active_turn: Option<DurableTurn>,
    pending_relocation: Option<DurableRelocation>,
}

fn replay_entries(
    session_id: &SessionId,
    state: ReplayState,
    mut last_sequence: JournalSequence,
    entries: &[JournalEntry],
) -> Result<RecoveredSession, StoreError> {
    let ReplayState {
        mut conversation,
        mut initial_directory,
        mut directory,
        mut tree,
        mut active_turn,
        mut pending_relocation,
    } = state;
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
            JournalRecord::SessionCreated {
                instructions,
                initial_directory: bootstrap_directory,
            } => {
                if conversation.is_some() || entry.sequence != JournalSequence(1) {
                    return Err(StoreError::InvalidJournal(
                        "SessionCreated must be the first and only session bootstrap".to_owned(),
                    ));
                }
                conversation = Some(Conversation::new(instructions.clone()));
                initial_directory = bootstrap_directory.clone();
                directory = bootstrap_directory.clone();
            }
            JournalRecord::CwdChanged {
                input,
                from,
                to,
                source,
            } => {
                if directory.as_ref() != Some(from) {
                    return Err(StoreError::InvalidJournal(format!(
                        "working directory transition expected {:?}, found {:?}",
                        from, directory
                    )));
                }
                let expected_revision = from
                    .revision
                    .0
                    .checked_add(1)
                    .ok_or(StoreError::SequenceOverflow)?;
                if to.revision.0 != expected_revision {
                    return Err(StoreError::InvalidJournal(format!(
                        "working directory revision must advance from {} to {}",
                        from.revision.0, expected_revision
                    )));
                }
                directory = Some(to.clone());
                append_replayed_payload(
                    &mut tree,
                    conversation_ref(&conversation)?.instructions(),
                    initial_directory.as_ref(),
                    SessionEntryPayload::CwdChanged {
                        input: input.clone(),
                        from: Some(from.clone()),
                        to: to.clone(),
                        source: source.clone(),
                    },
                    entry.sequence,
                )?;
            }
            JournalRecord::SessionEntryAppended {
                entry: session_entry,
                expected_head,
                resulting_head_revision,
            } => {
                if pending_relocation.is_some() {
                    return Err(StoreError::InvalidJournal(
                        "branch state cannot change while session relocation is pending".to_owned(),
                    ));
                }
                if active_turn
                    .as_ref()
                    .is_some_and(|turn| !is_terminal(&turn.phase))
                    && matches!(
                        &session_entry.payload,
                        SessionEntryPayload::Message(Message::User(_))
                            | SessionEntryPayload::CwdChanged {
                                source: DirectoryChangeSource::UserCommand,
                                ..
                            }
                    )
                {
                    return Err(StoreError::InvalidJournal(
                        "user branch state cannot be appended during an active turn".to_owned(),
                    ));
                }
                let instructions = conversation_ref(&conversation)?.instructions().clone();
                let branch = tree
                    .append(
                        session_entry.clone(),
                        expected_head,
                        *resulting_head_revision,
                        &instructions,
                        initial_directory.as_ref(),
                    )
                    .map_err(|error| StoreError::InvalidJournal(error.to_string()))?;
                conversation = Some(branch.conversation);
                directory = branch.directory;
                if let SessionEntryPayload::CwdChanged {
                    input,
                    from,
                    to,
                    source: DirectoryChangeSource::ModelTool { tool_call_id },
                } = &session_entry.payload
                {
                    let tool = pending_tool_mut(&mut active_turn, tool_call_id)?;
                    let expected = RecordedToolOutcome::CompletedWithEffect {
                        content: match tool.outcome.as_ref() {
                            Some(RecordedToolOutcome::CompletedWithEffect { content, .. }) => {
                                content.clone()
                            }
                            _ => {
                                return Err(StoreError::InvalidJournal(format!(
                                    "directory effect for {tool_call_id} has no recorded outcome"
                                )));
                            }
                        },
                        effect: ToolEffect::ChangeDirectory {
                            input: input.clone(),
                            from: from.clone().ok_or_else(|| {
                                StoreError::InvalidJournal(format!(
                                    "model directory effect for {tool_call_id} has no source directory"
                                ))
                            })?,
                            to: to.clone(),
                        },
                    };
                    if tool.outcome.as_ref() != Some(&expected) {
                        return Err(StoreError::InvalidJournal(format!(
                            "directory effect for {tool_call_id} does not match recorded outcome"
                        )));
                    }
                    if tool.effect_committed {
                        return Err(StoreError::InvalidJournal(format!(
                            "tool effect already committed: {tool_call_id}"
                        )));
                    }
                    tool.effect_committed = true;
                }
            }
            JournalRecord::ConversationHeadMoved {
                expected_head,
                target_entry_id,
                resulting_head_revision,
            } => {
                if pending_relocation.is_some() {
                    return Err(StoreError::InvalidJournal(
                        "conversation head cannot move while session relocation is pending"
                            .to_owned(),
                    ));
                }
                if active_turn
                    .as_ref()
                    .is_some_and(|turn| !is_terminal(&turn.phase))
                {
                    return Err(StoreError::InvalidJournal(
                        "conversation head cannot move during an active turn".to_owned(),
                    ));
                }
                let instructions = conversation_ref(&conversation)?.instructions().clone();
                let branch = tree
                    .move_head(
                        expected_head,
                        target_entry_id.clone(),
                        *resulting_head_revision,
                        &instructions,
                        initial_directory.as_ref(),
                    )
                    .map_err(|error| StoreError::InvalidJournal(error.to_string()))?;
                conversation = Some(branch.conversation);
                directory = branch.directory;
            }
            JournalRecord::SessionRelocationPrepared {
                from,
                to,
                locator_generation,
            } => {
                if active_turn
                    .as_ref()
                    .is_some_and(|turn| !is_terminal(&turn.phase))
                {
                    return Err(StoreError::InvalidJournal(
                        "session relocation cannot start during an active turn".to_owned(),
                    ));
                }
                if pending_relocation.is_some() {
                    return Err(StoreError::InvalidJournal(
                        "cannot prepare a second session relocation".to_owned(),
                    ));
                }
                pending_relocation = Some(DurableRelocation {
                    from: from.clone(),
                    to: to.clone(),
                    locator_generation: *locator_generation,
                });
            }
            JournalRecord::SessionRelocationCommitted {
                to,
                locator_generation,
            } => {
                let pending = pending_relocation.as_ref().ok_or_else(|| {
                    StoreError::InvalidJournal(
                        "session relocation committed without preparation".to_owned(),
                    )
                })?;
                if pending.to != *to || pending.locator_generation != *locator_generation {
                    return Err(StoreError::InvalidJournal(
                        "session relocation commit does not match preparation".to_owned(),
                    ));
                }
                pending_relocation = None;
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
                if let Some(initial) = initial_directory.as_ref() {
                    append_replayed_payload(
                        &mut tree,
                        conversation.instructions(),
                        Some(initial),
                        SessionEntryPayload::Message(message.clone()),
                        entry.sequence,
                    )?;
                }
            }
            JournalRecord::TurnOpened {
                turn_id,
                stable_revision,
                stable_head,
            } => {
                if pending_relocation.is_some() {
                    return Err(StoreError::InvalidJournal(
                        "cannot open a turn while session relocation is pending".to_owned(),
                    ));
                }
                let conversation = conversation_ref(&conversation)?;
                if conversation.revision() != *stable_revision {
                    return Err(StoreError::RevisionMismatch {
                        expected: conversation.revision(),
                        actual: *stable_revision,
                    });
                }
                validate_stable_head(&tree, directory.as_ref(), stable_head.as_ref())?;
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
                    stable_head: stable_head.clone(),
                    next_step: 0,
                    attempts: Vec::new(),
                    pending_tools: Vec::new(),
                });
            }
            JournalRecord::ModelStepPrepared {
                turn_id,
                step_id,
                revision,
                context,
                stable_head,
            } => {
                let conversation = conversation_ref(&conversation)?;
                if conversation.revision() != *revision {
                    return Err(StoreError::RevisionMismatch {
                        expected: conversation.revision(),
                        actual: *revision,
                    });
                }
                if let Some(context) = context
                    && directory.as_ref() != Some(&context.directory)
                {
                    return Err(StoreError::InvalidJournal(
                        "model step directory does not match recovered session state".to_owned(),
                    ));
                }
                if let (Some(context), Some(stable_head)) = (context, stable_head)
                    && (context.context_revision.0 != stable_head.revision.0
                        || context.directory.revision != stable_head.directory_revision)
                {
                    return Err(StoreError::InvalidJournal(
                        "model step context revision does not match stable head".to_owned(),
                    ));
                }
                validate_stable_head(&tree, directory.as_ref(), stable_head.as_ref())?;
                let turn = active_turn_mut(&mut active_turn, turn_id)?;
                turn.phase = TurnPhase::AwaitingModel;
                turn.stable_revision = *revision;
                turn.stable_head = stable_head.clone();
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
                entry_id,
                resulting_head_revision,
            } => {
                let conversation = conversation_ref(&conversation)?;
                validate_commit_marker(
                    conversation,
                    &tree,
                    message_id,
                    *resulting_revision,
                    entry_id.as_ref(),
                    *resulting_head_revision,
                    "assistant",
                )?;
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
                directory: tool_directory,
                stable_head,
            } => {
                if let Some(tool_directory) = tool_directory
                    && directory.as_ref() != Some(tool_directory)
                {
                    return Err(StoreError::InvalidJournal(
                        "tool execution directory does not match recovered session state"
                            .to_owned(),
                    ));
                }
                validate_stable_head(&tree, directory.as_ref(), stable_head.as_ref())?;
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
                    directory: tool_directory.clone(),
                    legacy_approval: LegacyToolApprovalState::NotRequested,
                    execution_count: 0,
                    execution_id: None,
                    outcome: None,
                    effect_committed: false,
                    result_committed: false,
                });
            }
            JournalRecord::LegacyToolApprovalRequested { tool_call_id } => {
                let tool = pending_tool_mut(&mut active_turn, tool_call_id)?;
                if tool.legacy_approval != LegacyToolApprovalState::NotRequested {
                    return Err(StoreError::InvalidJournal(format!(
                        "approval already requested for tool: {tool_call_id}"
                    )));
                }
                tool.legacy_approval = LegacyToolApprovalState::Pending;
                active_turn.as_mut().expect("active tool turn").phase =
                    TurnPhase::LegacyAwaitingApproval;
            }
            JournalRecord::LegacyToolApprovalResolved {
                tool_call_id,
                approved,
                reason,
            } => {
                let tool = pending_tool_mut(&mut active_turn, tool_call_id)?;
                if tool.legacy_approval != LegacyToolApprovalState::Pending {
                    return Err(StoreError::InvalidJournal(format!(
                        "approval resolution without pending request: {tool_call_id}"
                    )));
                }
                if *approved {
                    tool.legacy_approval = LegacyToolApprovalState::Approved;
                } else {
                    let message = reason
                        .clone()
                        .unwrap_or_else(|| "tool execution rejected".to_owned());
                    tool.legacy_approval = LegacyToolApprovalState::Rejected {
                        reason: message.clone(),
                    };
                    tool.outcome = Some(RecordedToolOutcome::FailedKnown { message });
                }
                let turn = active_turn.as_mut().expect("active tool turn");
                turn.phase = if turn
                    .pending_tools
                    .iter()
                    .any(|tool| tool.legacy_approval == LegacyToolApprovalState::Pending)
                {
                    TurnPhase::LegacyAwaitingApproval
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
                    tool.legacy_approval,
                    LegacyToolApprovalState::Rejected { .. }
                ) {
                    return Err(StoreError::InvalidJournal(format!(
                        "rejected legacy tool execution started: {tool_call_id}"
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
                active_turn.as_mut().expect("active tool turn").phase = TurnPhase::ExecutingTools;
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
                entry_id,
                resulting_head_revision,
            } => {
                let conversation = conversation_ref(&conversation)?;
                validate_commit_marker(
                    conversation,
                    &tree,
                    message_id,
                    *resulting_revision,
                    entry_id.as_ref(),
                    *resulting_head_revision,
                    "tool result",
                )?;
                let tool = pending_tool_mut(&mut active_turn, tool_call_id)?;
                if tool.outcome.is_none() {
                    return Err(StoreError::InvalidJournal(format!(
                        "tool result committed without recorded outcome: {tool_call_id}"
                    )));
                }
                if matches!(
                    tool.outcome,
                    Some(RecordedToolOutcome::CompletedWithEffect { .. })
                ) && !tool.effect_committed
                {
                    return Err(StoreError::InvalidJournal(format!(
                        "tool result committed before its effect: {tool_call_id}"
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
    if tree.entries().is_empty() && !conversation.messages().is_empty() {
        tree = SessionTree::from_legacy_messages(conversation.messages())
            .map_err(|error| StoreError::InvalidJournal(error.to_string()))?;
    }
    tree.validate_structure()
        .map_err(|error| StoreError::InvalidJournal(error.to_string()))?;
    let branch = tree
        .validate(conversation.instructions(), initial_directory.as_ref())
        .map_err(|error| StoreError::InvalidJournal(error.to_string()))?;
    if branch.conversation != conversation || branch.directory != directory {
        return Err(StoreError::InvalidJournal(
            "recovered conversation or directory does not match the durable tree head".to_owned(),
        ));
    }
    if let Some(turn) = active_turn.as_mut() {
        for tool in &mut turn.pending_tools {
            if tool.name == "change_directory"
                && tool.execution_id.is_some()
                && tool.outcome.is_none()
            {
                tool.execution_id = None;
            }
        }
    }
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
        tree,
        initial_directory,
        directory,
        active_turn,
        pending_relocation,
        last_sequence,
    })
}

fn append_replayed_payload(
    tree: &mut SessionTree,
    instructions: &InstructionSet,
    initial_directory: Option<&DirectorySnapshot>,
    payload: SessionEntryPayload,
    sequence: JournalSequence,
) -> Result<(), StoreError> {
    let expected_head = tree.head().clone();
    let resulting_revision = HeadRevision(
        expected_head
            .revision
            .0
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow)?,
    );
    tree.append(
        SessionEntry {
            id: super::types::EntryId::new(format!("legacy-entry-{}", sequence.0)),
            parent_id: expected_head.entry_id.clone(),
            timestamp_unix_ms: 0,
            payload,
        },
        &expected_head,
        resulting_revision,
        instructions,
        initial_directory,
    )
    .map_err(|error| StoreError::InvalidJournal(error.to_string()))?;
    Ok(())
}

fn validate_stable_head(
    tree: &SessionTree,
    directory: Option<&DirectorySnapshot>,
    stable_head: Option<&StableHead>,
) -> Result<(), StoreError> {
    let Some(stable_head) = stable_head else {
        return Ok(());
    };
    let directory = directory.ok_or_else(|| {
        StoreError::InvalidJournal("stable head has no working directory".to_owned())
    })?;
    if stable_head.entry_id != tree.head().entry_id
        || stable_head.revision != tree.head().revision
        || stable_head.directory_revision != directory.revision
    {
        return Err(StoreError::InvalidJournal(
            "stable head does not match recovered branch state".to_owned(),
        ));
    }
    Ok(())
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
    tree: &SessionTree,
    message_id: &MessageId,
    resulting_revision: ConversationRevision,
    entry_id: Option<&super::types::EntryId>,
    resulting_head_revision: Option<HeadRevision>,
    kind: &str,
) -> Result<(), StoreError> {
    if conversation.revision() != resulting_revision
        || conversation.messages().last().map(Message::id) != Some(message_id)
    {
        return Err(StoreError::InvalidJournal(format!(
            "{kind} commit marker does not match conversation head"
        )));
    }
    match (entry_id, resulting_head_revision) {
        (None, None) => {}
        (Some(entry_id), Some(head_revision))
            if tree.head().entry_id.as_ref() == Some(entry_id)
                && tree.head().revision == head_revision => {}
        _ => {
            return Err(StoreError::InvalidJournal(format!(
                "{kind} commit marker does not match durable tree head"
            )));
        }
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
    use crate::agent::{UserContent, UserMessage};

    fn journal_entry(session_id: &SessionId, sequence: u64, record: JournalRecord) -> JournalEntry {
        JournalEntry {
            schema_version: 1,
            session_id: session_id.clone(),
            sequence: JournalSequence(sequence),
            record,
        }
    }

    #[test]
    fn legacy_approval_wire_names_remain_readable() {
        let record: JournalRecord =
            serde_json::from_str(r#"{"ToolApprovalRequested":{"tool_call_id":"call-1"}}"#).unwrap();
        assert!(matches!(
            record,
            JournalRecord::LegacyToolApprovalRequested { .. }
        ));
        let phase: TurnPhase = serde_json::from_str(r#""AwaitingApproval""#).unwrap();
        assert_eq!(phase, TurnPhase::LegacyAwaitingApproval);
    }

    #[test]
    fn replay_rejects_a_tool_directory_from_a_different_branch_context() {
        let session_id = SessionId::new("session-1");
        let recorded_directory = DirectorySnapshot {
            path: std::path::PathBuf::from("/recorded"),
            revision: super::super::types::DirectoryRevision(0),
        };
        let wrong_directory = DirectorySnapshot {
            path: std::path::PathBuf::from("/wrong"),
            revision: super::super::types::DirectoryRevision(0),
        };
        let stable_head = StableHead {
            entry_id: None,
            revision: HeadRevision(0),
            directory_revision: recorded_directory.revision,
        };
        let entries = vec![
            journal_entry(
                &session_id,
                1,
                JournalRecord::SessionCreated {
                    instructions: InstructionSet::new("system"),
                    initial_directory: Some(recorded_directory.clone()),
                },
            ),
            journal_entry(
                &session_id,
                2,
                JournalRecord::TurnOpened {
                    turn_id: TurnId::new("turn-1"),
                    stable_revision: ConversationRevision(0),
                    stable_head: Some(stable_head.clone()),
                },
            ),
            journal_entry(
                &session_id,
                3,
                JournalRecord::ToolExecutionIntended {
                    turn_id: TurnId::new("turn-1"),
                    step_id: StepId::new("turn-1-step-0"),
                    tool_call_id: ToolCallId::new("call-1"),
                    name: "bash".to_owned(),
                    arguments: serde_json::json!({"command": "pwd"}),
                    replay_class: ReplayClass::Unknown,
                    directory: Some(wrong_directory),
                    stable_head: Some(stable_head),
                },
            ),
        ];

        let error = replay_session(&session_id, &entries).unwrap_err();

        assert!(error.to_string().contains("tool execution directory"));
    }

    #[test]
    fn replay_rejects_a_head_move_during_an_active_turn() {
        let session_id = SessionId::new("session-1");
        let directory = DirectorySnapshot {
            path: std::path::PathBuf::from("/recorded"),
            revision: super::super::types::DirectoryRevision(0),
        };
        let entries = vec![
            journal_entry(
                &session_id,
                1,
                JournalRecord::SessionCreated {
                    instructions: InstructionSet::new("system"),
                    initial_directory: Some(directory.clone()),
                },
            ),
            journal_entry(
                &session_id,
                2,
                JournalRecord::TurnOpened {
                    turn_id: TurnId::new("turn-1"),
                    stable_revision: ConversationRevision(0),
                    stable_head: Some(StableHead {
                        entry_id: None,
                        revision: HeadRevision(0),
                        directory_revision: directory.revision,
                    }),
                },
            ),
            journal_entry(
                &session_id,
                3,
                JournalRecord::ConversationHeadMoved {
                    expected_head: ConversationHead::default(),
                    target_entry_id: None,
                    resulting_head_revision: HeadRevision(1),
                },
            ),
        ];

        let error = replay_session(&session_id, &entries).unwrap_err();

        assert!(error.to_string().contains("head cannot move"));
    }

    #[test]
    fn replay_rejects_a_snapshot_projection_that_disagrees_with_the_tree() {
        let session_id = SessionId::new("session-1");
        let initial_directory = DirectorySnapshot {
            path: std::path::PathBuf::from("/recorded"),
            revision: super::super::types::DirectoryRevision(0),
        };
        let snapshot = RecoveredSession {
            session_id: session_id.clone(),
            conversation: Conversation::new(InstructionSet::new("system")),
            tree: SessionTree::default(),
            initial_directory: Some(initial_directory),
            directory: Some(DirectorySnapshot {
                path: std::path::PathBuf::from("/wrong"),
                revision: super::super::types::DirectoryRevision(0),
            }),
            active_turn: None,
            pending_relocation: None,
            last_sequence: JournalSequence(1),
        };

        let error = replay_after(snapshot, &[]).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not match the durable tree")
        );
    }

    #[test]
    fn replay_migrates_a_legacy_snapshot_prefix_before_its_wal_suffix() {
        let session_id = SessionId::new("session-1");
        let directory = DirectorySnapshot {
            path: std::path::PathBuf::from("/recorded"),
            revision: super::super::types::DirectoryRevision(0),
        };
        let mut conversation = Conversation::new(InstructionSet::new("system"));
        conversation
            .append(Message::User(UserMessage {
                id: MessageId::new("message-1"),
                content: vec![UserContent::Text {
                    text: "first".to_owned(),
                }],
            }))
            .unwrap();
        let snapshot = RecoveredSession {
            session_id: session_id.clone(),
            conversation,
            tree: SessionTree::default(),
            initial_directory: Some(directory.clone()),
            directory: Some(directory),
            active_turn: None,
            pending_relocation: None,
            last_sequence: JournalSequence(2),
        };
        let suffix = vec![journal_entry(
            &session_id,
            3,
            JournalRecord::ConversationAppended {
                message: Message::User(UserMessage {
                    id: MessageId::new("message-2"),
                    content: vec![UserContent::Text {
                        text: "second".to_owned(),
                    }],
                }),
                expected_revision: ConversationRevision(1),
                resulting_revision: ConversationRevision(2),
            },
        )];

        let recovered = replay_after(snapshot, &suffix).unwrap();

        assert_eq!(recovered.conversation.messages().len(), 2);
        assert_eq!(recovered.tree.entries().len(), 2);
        assert_eq!(
            recovered
                .tree
                .materialize(
                    recovered.conversation.instructions(),
                    recovered.initial_directory.as_ref(),
                )
                .unwrap()
                .conversation,
            recovered.conversation
        );
    }

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
                        initial_directory: None,
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
                        initial_directory: None,
                    },
                )
                .await
                .unwrap(),
            JournalSequence(1)
        );
    }
}
