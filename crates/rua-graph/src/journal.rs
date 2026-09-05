use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::cursor::Cursor;
use crate::id::{CursorId, NodeId};
use crate::node::{Meta, Outcome, Turn};

/// Control-plane event. The journal (`journal.jsonl`) is the source of truth
/// for cursors, in-flight handles and the node meta index; node bodies live in
/// `turns/<ulid>.jsonl` / `contexts/<ulid>.md`，经 DataStore 惰性加载。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum JournalEvent {
    /// A node was committed to the data plane. Header only（信封 + kind tag
    /// + meta 平铺）；正文永不进 journal。
    NodeCommitted { meta: Meta },
    CursorCreated { cursor: Cursor },
    CursorMoved { cursor_id: CursorId, node: Ulid },
    /// Cursor detached from the graph; its next input starts a new root.
    CursorDetached { cursor_id: CursorId },
    /// A turn started; its node id was pre-allocated（数据面同时注册了空
    /// 条目，悬空正文文件可归因到这条事件）。
    TurnStarted {
        cursor_id: CursorId,
        node_id: NodeId<Turn>,
        started_at: u64,
    },
    /// The turn's node was committed (completed / failed / cancelled).
    TurnFinished {
        cursor_id: CursorId,
        node_id: NodeId<Turn>,
        outcome: Outcome,
    },
}
