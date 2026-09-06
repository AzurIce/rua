use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::id::CursorId;
use crate::node::{Outcome, Usage};

/// Streaming/control events emitted while a turn is in flight. The engine
/// produces them; the server bridges them onto the WebSocket event stream.
/// node_id 是裸 Ulid（wire 领土）；它总是某轮的 id，但这里没有类型标记。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TurnEvent {
    /// Turn started; `node_id` is the pre-allocated landing node id.
    Started { cursor_id: CursorId, node_id: Ulid },
    TextDelta {
        cursor_id: CursorId,
        node_id: Ulid,
        delta: String,
    },
    ReasoningDelta {
        cursor_id: CursorId,
        node_id: Ulid,
        delta: String,
    },
    ToolExecStarted {
        cursor_id: CursorId,
        node_id: Ulid,
        call_id: String,
        name: String,
        args: serde_json::Value,
    },
    ToolExecFinished {
        cursor_id: CursorId,
        node_id: Ulid,
        call_id: String,
        output_preview: String,
        duration_ms: u64,
    },
    /// 一次 LLM 调用完成（该 step 已落盘）。`usage` 是截至本轮该次调用的
    /// **累计**用量（由 engine 汇总），`step` 是本次 LlmCall 在 turn step
    /// 列表中的下标（0 起）。UI 靠它做 in-flight 用量的实时刷新；provider
    /// 只在流末尾给数，所以粒度是每调用一次跳一格，不是逐 token。
    LlmCallFinished {
        cursor_id: CursorId,
        node_id: Ulid,
        step: usize,
        usage: Usage,
    },
    /// The turn node was committed to the graph.
    Committed {
        cursor_id: CursorId,
        node_id: Ulid,
        outcome: Outcome,
    },
}
