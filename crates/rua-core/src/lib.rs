//! rua-core: the graph engine for rua.
//!
//! Data plane: immutable, committed nodes (Input / Turn / Context), stored as
//! one JSON file per node. Control plane: cursors (sessions) + in-flight turn
//! handles, persisted as an append-only journal and rebuilt by replay.

pub mod assemble;
pub mod config;
pub mod cursor;
pub mod error;
pub mod events;
pub mod graph;
pub mod id;
pub mod journal;
pub mod message;
pub mod node;
pub mod store;

pub use assemble::assemble;
pub use cursor::{Cursor, CursorRegistry};
pub use error::{Error, Result};
pub use events::TurnEvent;
pub use graph::{Graph, NodeMeta};
pub use id::{CursorId, NodeId};
pub use journal::JournalEvent;
pub use message::{CoreMessage, CoreToolCall};
pub use node::{Node, NodeKind, NodeKindTag, Outcome, Step, Usage};
pub use store::Store;
