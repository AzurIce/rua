//! WebSocket wire protocol: every message is a flat JSON object carrying an
//! `event` tag. Turn increments (`TurnEvent` from the engine) are mapped 1:1
//! onto the matching variants; graph mutations are broadcast by the API layer.

use rua_graph::cursor::Cursor;
use rua_graph::id::{CursorId, NodeId};
use rua_graph::node::Outcome;
use rua_graph::TurnEvent;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ServerEvent {
    TurnStarted { cursor_id: CursorId, node_id: NodeId },
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
    /// A turn reached a terminal state and (normally) its node was committed.
    /// Also emitted with `outcome: failed` when the engine errored before a
    /// node could be committed at all.
    TurnCommitted {
        cursor_id: CursorId,
        node_id: NodeId,
        outcome: Outcome,
    },
    /// 节点提交广播：header 形状（信封 + kind tag + meta 平铺，无正文），
    /// 与 chain 端点同形。
    NodeCommitted { meta: serde_json::Value },
    CursorCreated { cursor: Cursor },
    CursorMoved { cursor_id: CursorId, node: Option<NodeId> },
    /// The daemon switched the active graph; clients should drop all
    /// graph-derived state and resync from scratch.
    GraphSwitched { name: String },
}

impl ServerEvent {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("ServerEvent serialization is infallible")
    }
}

impl From<TurnEvent> for ServerEvent {
    fn from(event: TurnEvent) -> Self {
        match event {
            TurnEvent::Started { cursor_id, node_id } => Self::TurnStarted { cursor_id, node_id },
            TurnEvent::TextDelta {
                cursor_id,
                node_id,
                delta,
            } => Self::TextDelta {
                cursor_id,
                node_id,
                delta,
            },
            TurnEvent::ReasoningDelta {
                cursor_id,
                node_id,
                delta,
            } => Self::ReasoningDelta {
                cursor_id,
                node_id,
                delta,
            },
            TurnEvent::ToolExecStarted {
                cursor_id,
                node_id,
                call_id,
                name,
                args,
            } => Self::ToolExecStarted {
                cursor_id,
                node_id,
                call_id,
                name,
                args,
            },
            TurnEvent::ToolExecFinished {
                cursor_id,
                node_id,
                call_id,
                output_preview,
                duration_ms,
            } => Self::ToolExecFinished {
                cursor_id,
                node_id,
                call_id,
                output_preview,
                duration_ms,
            },
            TurnEvent::Committed {
                cursor_id,
                node_id,
                outcome,
            } => Self::TurnCommitted {
                cursor_id,
                node_id,
                outcome,
            },
        }
    }
}
