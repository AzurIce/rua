//! rua-graph: the graph model for rua.
//!
//! 图模型定义 + 图数据能力，不含 LLM/工具消费。每个节点同时活在两个世界：
//! 结构世界（信封 + meta，`journal.jsonl` 是唯一事实源，Graph 重放为 meta
//! 索引）与数据世界（正文——Turn 的 `turns/<ulid>.jsonl` 轮内事件流、
//! Context 的 `contexts/<ulid>.md` 文本——由 [`DataStore`] 统一入口做并发
//! 与缓存，惰性加载）。Input 正文内联在 header 的 `text` 里。

pub mod cursor;
pub mod datastore;
pub mod error;
pub mod events;
pub mod graph;
pub mod id;
pub mod journal;
pub mod message;
pub(crate) mod migrate;
pub mod node;
pub mod store;

pub use cursor::{Cursor, CursorRegistry, TurnHandle};
pub use datastore::{DataStore, Entry};
pub use error::{Error, Result};
pub use events::TurnEvent;
pub use graph::{CursorMut, Graph, OpenTurn};
pub use id::{CursorId, NodeId};
pub use journal::JournalEvent;
pub use message::{CoreMessage, CoreToolCall};
pub use node::{
    Context, ContextData, Data, Input, Kind, Meta, Node, Outcome, Step, Turn, TurnData, TurnLine,
    Usage, now_millis,
};
pub use store::Store;
pub use ulid::Ulid;
