use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::error::{Error, Result};
use crate::id::{CursorId, NodeId};
use crate::node::Turn;

/// A session: a movable pointer onto the graph, bound to an actor.
/// "Fork is free" — cursors are cheap, nodes are immutable and shared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    pub id: CursorId,
    /// The tip this session is attached to (Input/Turn 均可，裸 Ulid 领土).
    /// `None` = empty conversation; the next input becomes a root node.
    pub node: Option<Ulid>,
    pub actor: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    pub created_at: u64,
}

/// An in-flight turn handle. The node id is pre-allocated at turn start so
/// UIs can reference the landing spot before the turn commits. 落点必然是
/// Turn：id 带类型。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TurnHandle {
    pub cursor_id: CursorId,
    pub node_id: NodeId<Turn>,
    pub started_at: u64,
}

/// Control-plane registry: cursors + at most one in-flight turn per cursor.
/// Graph-level there is no locking: any number of readers/forkers per node;
/// contention only exists at the cursor level.
#[derive(Debug, Default)]
pub struct CursorRegistry {
    pub cursors: HashMap<CursorId, Cursor>,
    in_flight: HashMap<CursorId, TurnHandle>,
}

impl CursorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&mut self, actor: impl Into<String>, capabilities: Vec<String>) -> Cursor {
        let cursor = Cursor {
            id: CursorId::new(),
            node: None,
            actor: actor.into(),
            capabilities,
            created_at: crate::node::now_millis(),
        };
        self.cursors.insert(cursor.id, cursor.clone());
        cursor
    }

    pub fn get(&self, id: CursorId) -> Result<&Cursor> {
        self.cursors.get(&id).ok_or(Error::CursorNotFound(id))
    }

    pub fn list(&self) -> Vec<&Cursor> {
        let mut v: Vec<_> = self.cursors.values().collect();
        v.sort_by_key(|c| c.created_at);
        v
    }

    /// Move the cursor's tip. Landing-point validity (structural nodes only,
    /// node must exist) is checked by the caller against the graph.
    pub fn move_to(&mut self, id: CursorId, node: Ulid) -> Result<()> {
        let cursor = self.cursors.get_mut(&id).ok_or(Error::CursorNotFound(id))?;
        cursor.node = Some(node);
        Ok(())
    }

    /// Detach the cursor from the graph entirely: the next input becomes a
    /// fresh root node (a new, disconnected conversation tree).
    pub fn detach(&mut self, id: CursorId) -> Result<()> {
        let cursor = self.cursors.get_mut(&id).ok_or(Error::CursorNotFound(id))?;
        cursor.node = None;
        Ok(())
    }

    /// Begin an in-flight turn; returns the pre-allocated typed node id.
    pub fn begin_turn(&mut self, cursor_id: CursorId, node_id: NodeId<Turn>) -> Result<TurnHandle> {
        if !self.cursors.contains_key(&cursor_id) {
            return Err(Error::CursorNotFound(cursor_id));
        }
        if self.in_flight.contains_key(&cursor_id) {
            return Err(Error::CursorBusy(cursor_id));
        }
        let handle = TurnHandle {
            cursor_id,
            node_id,
            started_at: crate::node::now_millis(),
        };
        self.in_flight.insert(cursor_id, handle);
        Ok(handle)
    }

    /// Finish the in-flight turn (any outcome). Returns the handle.
    pub fn finish_turn(&mut self, cursor_id: CursorId) -> Result<TurnHandle> {
        self.in_flight
            .remove(&cursor_id)
            .ok_or(Error::CursorIdle(cursor_id))
    }

    pub fn in_flight(&self, cursor_id: CursorId) -> Option<&TurnHandle> {
        self.in_flight.get(&cursor_id)
    }

    pub fn in_flight_all(&self) -> impl Iterator<Item = &TurnHandle> {
        self.in_flight.values()
    }

    /// Journal replay: restore a cursor without minting a new id.
    pub fn restore(&mut self, cursor: Cursor) {
        self.cursors.insert(cursor.id, cursor);
    }

    /// Journal replay: restore an in-flight handle.
    pub fn restore_in_flight(&mut self, handle: TurnHandle) {
        self.in_flight.insert(handle.cursor_id, handle);
    }
}
