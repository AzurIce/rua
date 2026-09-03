//! rua-graph: the graph model for rua.
//!
//! 图模型定义 + 图数据能力，不含 LLM/工具消费。
//! Data plane: immutable, committed nodes (Input / Turn / Context). The
//! journal (`journal.jsonl`) is the single source of truth for structure
//! (node metas — Input bodies inline — + cursor state + turn lifecycle);
//! Turn bodies are append-only `turns/<ulid>.jsonl` event streams, Context
//! bodies are `contexts/<ulid>.md` text files.

pub mod cursor;
pub mod error;
pub mod events;
pub mod graph;
pub mod id;
pub mod journal;
pub mod message;
pub(crate) mod migrate;
pub mod node;
pub mod store;

pub use cursor::{Cursor, CursorRegistry};
pub use error::{Error, Result};
pub use events::TurnEvent;
pub use graph::{Graph, NodeMeta};
pub use id::{CursorId, NodeId};
pub use journal::JournalEvent;
pub use message::{CoreMessage, CoreToolCall};
pub use node::{Node, NodeKind, NodeKindTag, Outcome, Step, TurnLine, Usage};
pub use store::Store;
