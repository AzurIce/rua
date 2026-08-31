use serde::{Deserialize, Serialize};

use crate::id::NodeId;
use crate::message::CoreMessage;

/// Normalized token usage, summed over a turn's LLM calls.
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

impl Usage {
    pub fn add_assign(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
    }
}

/// Terminal state of a turn. A node existing at all means its turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Failed,
    Cancelled,
    /// The daemon died mid-turn; discovered on journal replay.
    Interrupted,
}

/// One step inside a turn. The full interior of a turn is preserved;
/// operations on the graph only ever address whole turns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Step {
    /// One LLM call: the exact request messages and what came back.
    LlmCall {
        request: Vec<CoreMessage>,
        /// Final assistant text (may be empty when the call only requested tools).
        response_text: String,
        /// Tool calls requested by this response.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<crate::message::CoreToolCall>,
        /// Reasoning trace, kept for audit but never fed back into context.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default)]
        usage: Usage,
    },
    /// One tool execution (incl. distill calls, per the design memo).
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
    /// Input on an edge, modelled as a node. Roots a conversation when
    /// `parent` is `None`.
    Input { text: String, actor: String },
    /// A complete turn: LLM calls + tool executions + outcome.
    Turn {
        steps: Vec<Step>,
        outcome: Outcome,
        actor: String,
        model: String,
        #[serde(default)]
        usage: Usage,
    },
    /// Distilled material. Never a cursor/attach/fork landing point; its
    /// `context_refs` are provenance edges to the nodes it was distilled
    /// from.
    Context {
        body: String,
        created_by: NodeId,
        model: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKindTag {
    Input,
    Turn,
    Context,
}

/// An immutable, committed node. `parent` is the immutable structural link;
/// `context_refs` are material links to already-committed nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<NodeId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_refs: Vec<NodeId>,
    /// 创建者（provenance）：由哪个 turn 内部的 spawn_turn 工具调用产生。
    /// 只打在 spawn 出的根 Input 上（子树归属沿 chain 传递推导）；
    /// None = 用户/直接操作产生。不影响装配，纯属图上的溯源。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<NodeId>,
    /// Unix epoch milliseconds.
    pub created_at: u64,
    pub kind: NodeKind,
}

impl Node {
    pub fn kind_tag(&self) -> NodeKindTag {
        match &self.kind {
            NodeKind::Input { .. } => NodeKindTag::Input,
            NodeKind::Turn { .. } => NodeKindTag::Turn,
            NodeKind::Context { .. } => NodeKindTag::Context,
        }
    }

    pub fn is_structural(&self) -> bool {
        !matches!(self.kind, NodeKind::Context { .. })
    }

    pub fn now_millis() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}
