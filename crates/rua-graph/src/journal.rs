use serde::{Deserialize, Serialize};

use crate::cursor::Cursor;
use crate::id::{CursorId, NodeId};
use crate::node::{AnyNode, Outcome};

/// Control-plane event. The journal (`journal.jsonl`) is the source of truth
/// for cursors, in-flight handles and the node index; node bodies live in
/// `turns/<ulid>.jsonl` / `contexts/<ulid>.md` and are loaded lazily.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum JournalEvent {
    /// A node was committed to the data plane. Header only (信封 + kind tag
    /// + meta 平铺); the body is externalized. Replays as `data: None`.
    NodeCommitted { meta: AnyNode },
    CursorCreated { cursor: Cursor },
    CursorMoved { cursor_id: CursorId, node: NodeId },
    /// Cursor detached from the graph; its next input starts a new root.
    CursorDetached { cursor_id: CursorId },
    /// A turn started; its node id was pre-allocated.
    TurnStarted {
        cursor_id: CursorId,
        node_id: NodeId,
        started_at: u64,
    },
    /// The turn's node was committed (completed / failed / cancelled).
    TurnFinished {
        cursor_id: CursorId,
        node_id: NodeId,
        outcome: Outcome,
    },
}
