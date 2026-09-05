use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Tool call issued by the assistant inside a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoreToolCall {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// Provider-agnostic message IR produced by `assemble`.
///
/// The graph engine never depends on a concrete LLM provider; the engine
/// crate maps these onto rig's `Message` types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum CoreMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<CoreToolCall>,
    },
    ToolResult {
        call_id: String,
        name: String,
        output: String,
    },
    /// Distilled material injected via an input node's `context_refs`.
    /// Rendered by the engine as a user-role message with provenance header.
    /// sources 是异构溯源（裸 Ulid）。
    Context {
        body: String,
        sources: Vec<Ulid>,
    },
}
