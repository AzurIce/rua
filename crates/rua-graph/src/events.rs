use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::id::CursorId;
use crate::node::Outcome;

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
    /// The turn node was committed to the graph.
    Committed {
        cursor_id: CursorId,
        node_id: Ulid,
        outcome: Outcome,
    },
}
