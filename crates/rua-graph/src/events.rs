use serde::{Deserialize, Serialize};

use crate::id::{CursorId, NodeId};
use crate::node::Outcome;

/// Streaming/control events emitted while a turn is in flight. The engine
/// produces them; the server bridges them onto the WebSocket event stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TurnEvent {
    /// Turn started; `node_id` is the pre-allocated landing node id.
    Started { cursor_id: CursorId, node_id: NodeId },
    TextDelta {
        cursor_id: CursorId,
        node_id: NodeId,
        delta: String,
    },
    ReasoningDelta {
        cursor_id: CursorId,
        node_id: NodeId,
        delta: String,
    },
    ToolExecStarted {
        cursor_id: CursorId,
        node_id: NodeId,
        call_id: String,
        name: String,
        args: serde_json::Value,
    },
    ToolExecFinished {
        cursor_id: CursorId,
        node_id: NodeId,
        call_id: String,
        output_preview: String,
        duration_ms: u64,
    },
    /// The turn node was committed to the graph.
    Committed {
        cursor_id: CursorId,
        node_id: NodeId,
        outcome: Outcome,
    },
}
