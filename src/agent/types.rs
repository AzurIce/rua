use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }
    };
}

string_id!(MessageId);
string_id!(ToolCallId);
string_id!(TurnId);
string_id!(StepId);
string_id!(AttemptId);
string_id!(ExecutionId);
string_id!(SessionId);
string_id!(EntryId);
string_id!(ProviderId);
string_id!(ApiFamily);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SessionEntryName(String);

impl SessionEntryName {
    pub fn try_new(value: impl Into<String>) -> Result<Self, SessionEntryNameError> {
        let value = value.into();
        let mut components = std::path::Path::new(&value).components();
        if value.is_empty()
            || !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
        {
            return Err(SessionEntryNameError(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionEntryName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl<'de> Deserialize<'de> for SessionEntryName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::try_new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid session entry name: {0}")]
pub struct SessionEntryNameError(String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionLocator {
    pub project_root: PathBuf,
    pub entry_name: SessionEntryName,
}

impl<'de> Deserialize<'de> for SessionLocator {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireLocator {
            project_root: PathBuf,
            entry_name: SessionEntryName,
        }

        let locator = WireLocator::deserialize(deserializer)?;
        if !locator.project_root.is_absolute() {
            return Err(serde::de::Error::custom(
                "session locator project root must be absolute",
            ));
        }
        Ok(Self {
            project_root: locator.project_root,
            entry_name: locator.entry_name,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentSessionRef {
    pub session_id: SessionId,
    pub fork_entry_id: Option<EntryId>,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ConversationRevision(pub u64);

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct DirectoryRevision(pub u64);

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct HeadRevision(pub u64);

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ContextRevision(pub u64);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StableHead {
    pub entry_id: Option<EntryId>,
    pub revision: HeadRevision,
    pub directory_revision: DirectoryRevision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectorySnapshot {
    pub path: PathBuf,
    pub revision: DirectoryRevision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContextSnapshot {
    pub directory: DirectorySnapshot,
    pub context_revision: ContextRevision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PartIndex(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionSet {
    pub text: String,
    pub revision: u64,
}

impl InstructionSet {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            revision: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    User(UserMessage),
    Assistant(Box<AssistantMessage>),
    ToolResult(ToolResultMessage),
}

impl Message {
    pub fn id(&self) -> &MessageId {
        match self {
            Self::User(message) => &message.id,
            Self::Assistant(message) => &message.id,
            Self::ToolResult(message) => &message.id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    pub id: MessageId,
    pub content: Vec<UserContent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserContent {
    Text { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub id: MessageId,
    pub parts: Vec<AssistantPart>,
    pub stop_reason: StopReason,
    pub usage: Option<Usage>,
    pub provenance: ResponseProvenance,
    pub provider_state: Option<OpaqueProviderState>,
}

impl AssistantMessage {
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.parts.iter().filter_map(|part| match part {
            AssistantPart::ToolCall(call) => Some(call),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AssistantPart {
    Text(TextPart),
    Reasoning(ReasoningPart),
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextPart {
    pub text: String,
    pub provider_state: Option<OpaqueProviderState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningPart {
    pub text: Option<String>,
    pub provider_state: Option<OpaqueProviderState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: Value,
    pub provider_state: Option<OpaqueProviderState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultMessage {
    pub id: MessageId,
    pub tool_call_id: ToolCallId,
    pub name: String,
    pub content: Vec<ToolResultContent>,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolResultContent {
    Text { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpaqueProviderState {
    pub provider: ProviderId,
    pub api_family: ApiFamily,
    pub schema_version: u32,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: ProviderId,
    pub api_family: ApiFamily,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseProvenance {
    pub requested: ModelRef,
    pub response_model: Option<String>,
    pub response_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    Length,
    ContentFilter,
    Other(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub replay_class: ReplayClass,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplayClass {
    ReadOnly,
    Idempotent { key_source: String },
    Effectful,
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GenerationOptions {
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u64>,
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub turn_id: TurnId,
    pub step_id: StepId,
    pub attempt_id: AttemptId,
    pub conversation_revision: ConversationRevision,
    pub context: TurnContextSnapshot,
    pub instructions: InstructionSet,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub model: ModelRef,
    pub options: GenerationOptions,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub tools: bool,
    pub reasoning: bool,
    pub image_input: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResponseInfo {
    pub provenance: ResponseProvenance,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderCompletion {
    pub stop_reason: StopReason,
    pub usage: Option<Usage>,
    pub provider_state: Option<OpaqueProviderState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Authentication,
    Authorization,
    InvalidRequest,
    UnsupportedCapability,
    ContextLength,
    RateLimit,
    Timeout,
    Transport,
    Server,
    Protocol,
    Cancelled,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryHint {
    Never,
    Retryable { after: Option<Duration> },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
    pub retry: RetryHint,
}

impl ProviderError {
    pub fn protocol(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Protocol,
            message: message.into(),
            retry: RetryHint::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_entry_name_deserialization_enforces_single_component_validation() {
        assert_eq!(
            serde_json::from_str::<SessionEntryName>(r#""friendly-name""#)
                .unwrap()
                .as_str(),
            "friendly-name"
        );
        assert!(serde_json::from_str::<SessionEntryName>(r#""../outside""#).is_err());
        assert!(serde_json::from_str::<SessionEntryName>(r#""nested/entry""#).is_err());
    }

    #[test]
    fn session_locator_deserialization_rejects_relative_project_roots() {
        let json = r#"{"project_root":"relative/project","entry_name":"session"}"#;

        let error = serde_json::from_str::<SessionLocator>(json).unwrap_err();

        assert!(error.to_string().contains("project root must be absolute"));
    }
}
