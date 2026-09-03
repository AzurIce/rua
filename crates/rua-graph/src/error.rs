use crate::id::{CursorId, NodeId};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("node not found: {0}")]
    NodeNotFound(NodeId),

    #[error("node already committed (immutable): {0}")]
    NodeAlreadyCommitted(NodeId),

    #[error("parent not committed: {0}")]
    ParentNotCommitted(NodeId),

    #[error("context ref not committed: {0}")]
    ContextRefNotCommitted(NodeId),

    #[error("structural node (input/turn) may only reference context nodes, got: {0}")]
    ContextRefNotContextNode(NodeId),

    #[error("context nodes cannot have a parent: {0}")]
    ContextNodeHasParent(NodeId),

    #[error("cursor cannot land on a context node: {0}")]
    CursorOnContextNode(NodeId),

    #[error("cursor not found: {0}")]
    CursorNotFound(CursorId),

    #[error("cursor already has an in-flight turn: {0}")]
    CursorBusy(CursorId),

    #[error("cursor has no in-flight turn: {0}")]
    CursorIdle(CursorId),

    #[error("node body not loaded (data: None): {0}")]
    DataNotLoaded(NodeId),

    #[error("journal corrupted at line {line}: {reason}")]
    JournalCorrupted { line: usize, reason: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("config error: {0}")]
    Config(String),
}
