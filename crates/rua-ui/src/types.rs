//! Mirror of the frozen REST/WS contract with rua-server.
//!
//! Ids are ULID strings; we keep them opaque (`String`) on the UI side.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKindTag {
    Input,
    Turn,
    Context,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Completed => "completed",
            Outcome::Failed => "failed",
            Outcome::Cancelled => "cancelled",
            Outcome::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeMeta {
    pub id: String,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub context_refs: Vec<String>,
    pub kind: NodeKindTag,
    #[serde(default)]
    pub outcome: Option<Outcome>,
    pub actor: String,
    pub created_at: u64,
    #[serde(default)]
    pub usage: Option<Usage>,
    /// 该回合最后一次 LLM 调用的上下文量（input tokens，turn 节点才有）。
    #[serde(default)]
    pub context_tokens: Option<u64>,
    /// 创建者（哪个 turn 的 spawn_turn 产生）；None = 用户/直接操作。
    #[serde(default)]
    pub created_by: Option<String>,
    /// 该 turn 使用的模型（turn 节点才有）。
    #[serde(default)]
    pub model: Option<String>,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    pub id: String,
    pub node: Option<String>,
    pub actor: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnHandle {
    pub cursor_id: String,
    pub node_id: String,
    pub started_at: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GraphResponse {
    pub nodes: Vec<NodeMeta>,
    pub cursors: Vec<Cursor>,
    #[serde(default)]
    pub interrupted: Vec<TurnHandle>,
    /// Turns currently running (as opposed to `interrupted` leftovers).
    #[serde(default)]
    pub in_flight: Vec<TurnHandle>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Step {
    LlmCall {
        /// Kept opaque: the UI never inspects the request messages.
        request: serde_json::Value,
        response_text: String,
        #[serde(default)]
        tool_calls: Vec<serde_json::Value>,
        #[serde(default)]
        reasoning: Option<String>,
        #[serde(default)]
        usage: Usage,
    },
    ToolExec {
        call_id: String,
        name: String,
        args: serde_json::Value,
        output: String,
        duration_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeKind {
    Input { text: String, actor: String },
    Turn {
        steps: Vec<Step>,
        outcome: Outcome,
        actor: String,
        model: String,
        #[serde(default)]
        usage: Usage,
    },
    Context {
        body: String,
        created_by: String,
        model: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub context_refs: Vec<String>,
    pub created_at: u64,
    pub kind: NodeKind,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct InputResponse {
    pub input_node: NodeMeta,
    pub turn_node_id: String,
}

/// Response of `POST /api/inputs`: the atomically created cursor + root input
/// + started turn (draft state's first send).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RootInputResponse {
    pub cursor: Cursor,
    pub input_node: NodeMeta,
    pub turn_node_id: String,
}

/// Response of `GET /api/graphs`: all graph names + the active one.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GraphsResponse {
    pub graphs: Vec<String>,
    pub current: String,
}

/// Response of `GET /api/models`: per-provider model list + daemon default.
/// `id` 是 model ref（默认 provider 为裸模型名，具名 provider 为
/// `"provider/model"`），直接作为发送时的模型覆盖值。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ModelsResponse {
    pub models: Vec<ModelEntry>,
    pub default: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub provider: String,
    pub model: String,
}

/// Flat WS event stream: every message carries an `event` tag.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WsEvent {
    TurnStarted {
        cursor_id: String,
        node_id: String,
    },
    TextDelta {
        cursor_id: String,
        node_id: String,
        delta: String,
    },
    ReasoningDelta {
        cursor_id: String,
        node_id: String,
        delta: String,
    },
    ToolExecStarted {
        cursor_id: String,
        node_id: String,
        call_id: String,
        name: String,
        args: serde_json::Value,
    },
    ToolExecFinished {
        cursor_id: String,
        node_id: String,
        call_id: String,
        output_preview: String,
        duration_ms: u64,
    },
    TurnCommitted {
        cursor_id: String,
        node_id: String,
        outcome: Outcome,
    },
    NodeCommitted {
        meta: NodeMeta,
    },
    CursorCreated {
        cursor: Cursor,
    },
    CursorMoved {
        cursor_id: String,
        node: Option<String>,
    },
    /// 活跃图被切换（新建/切换/删除当前图）：客户端应丢弃所有图状态重同步。
    GraphSwitched {
        #[allow(dead_code)]
        name: String,
    },
}

/// First 8 chars of a ULID, for compact display.
pub fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}
