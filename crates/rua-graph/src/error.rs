use crate::id::CursorId;
use ulid::Ulid;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("node not found: {0}")]
    NodeNotFound(Ulid),

    #[error("node already committed (immutable): {0}")]
    NodeAlreadyCommitted(Ulid),

    #[error("parent not committed: {0}")]
    ParentNotCommitted(Ulid),

    /// 链严格交替：Input 的父必须是 Turn，Turn 的父必须是 Input。
    #[error("parent kind mismatch: node {node} declared parent {parent} of the wrong kind")]
    ParentKindMismatch { node: Ulid, parent: Ulid },

    /// 期望的节点 kind 与实际不符（受检类型恢复）。
    #[error("node {id} is not a {expected} node")]
    WrongKind { id: Ulid, expected: &'static str },

    #[error("context ref not committed: {0}")]
    ContextRefNotCommitted(Ulid),

    #[error("structural node (input/turn) may only reference context nodes, got: {0}")]
    ContextRefNotContextNode(Ulid),

    #[error("cursor cannot land on a context node: {0}")]
    CursorOnContextNode(Ulid),

    #[error("cursor not found: {0}")]
    CursorNotFound(CursorId),

    #[error("cursor already has an in-flight turn: {0}")]
    CursorBusy(CursorId),

    #[error("cursor has no in-flight turn: {0}")]
    CursorIdle(CursorId),

    /// commit 一个有正文的 kind（Turn/Context）时，数据面里没有它的条目
    ///（open_turn 注册的空条目 / DataStore::create 写入的条目）。
    #[error("node body missing in the data store: {0}")]
    DataNotLoaded(Ulid),

    /// DataStore 条目加载正文失败（条目中毒，可由下一次 entry() 重试）。
    #[error("node body load failed: {id}: {reason}")]
    BodyLoadFailed { id: Ulid, reason: String },

    #[error("journal corrupted at line {line}: {reason}")]
    JournalCorrupted { line: usize, reason: String },

    /// 旧布局迁移失败（如 parentless 的 legacy Turn 无法进入严格交替的链）。
    #[error("legacy migration failed: {0}")]
    MigrationFailed(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("config error: {0}")]
    Config(String),
}
