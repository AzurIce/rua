//! 标识：类型化节点 id + cursor id。
//!
//! 节点 id 分两种领土：类型静态已知处用 [`NodeId<T>`]（ULID + 幽灵类型）——
//! 各类型的边字段、API 分发边界走这里；盲走处（journal 行、链回溯、
//! cursor tip、children 索引）用裸 [`ulid::Ulid`]。不存在 `NodeId<Any>`
//! 之类的中间态：`T` 必须实现 [`Kind`]，而和类型没有 `Data`。

use std::marker::PhantomData;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::node::Kind;

/// 类型化节点 id。序列化形态就是 ULID 字符串：类型不进存储（盘上的 id 是
/// 不透明字符串），类型知识只活在静态代码里。
pub struct NodeId<T: Kind> {
    raw: ulid::Ulid,
    _t: PhantomData<fn() -> T>,
}

impl<T: Kind> NodeId<T> {
    /// 铸一个新 id（纯计算，不碰文件系统）。
    pub fn new() -> Self {
        Self {
            raw: ulid::Ulid::new(),
            _t: PhantomData,
        }
    }

    /// 从裸 Ulid 恢复类型化 id。**只在 crate 内部可用**：类型恢复只允许发生
    /// 在拥有索引/标签的地方（journal 重放按 tag 分派、`Meta` 变体解析、
    /// `Graph` 的受检查询）。crate 之外想拿 typed id 只有三条路：
    /// `DataStore::allocate`（新铸）、`Meta` 变体访问、`Graph::expect_turn`。
    pub(crate) fn from_raw(raw: ulid::Ulid) -> Self {
        Self {
            raw,
            _t: PhantomData,
        }
    }

    /// 擦成裸 Ulid（进 journal、盲走索引用）。
    pub fn raw(self) -> ulid::Ulid {
        self.raw
    }
}

// 以下 impl 全部手写：derive 会为 T 附加多余的 trait bound，而 id 的行为
// 本应与 T 无关。

impl<T: Kind> Copy for NodeId<T> {}

impl<T: Kind> Clone for NodeId<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Kind> Default for NodeId<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Kind> std::fmt::Debug for NodeId<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NodeId({})", self.raw)
    }
}

impl<T: Kind> std::fmt::Display for NodeId<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.raw)
    }
}

impl<T: Kind> PartialEq for NodeId<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<T: Kind> Eq for NodeId<T> {}

impl<T: Kind> PartialOrd for NodeId<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: Kind> Ord for NodeId<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.raw.cmp(&other.raw)
    }
}

impl<T: Kind> std::hash::Hash for NodeId<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<T: Kind> Serialize for NodeId<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.raw.serialize(serializer)
    }
}

impl<'de, T: Kind> Deserialize<'de> for NodeId<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_raw(ulid::Ulid::deserialize(deserializer)?))
    }
}

macro_rules! id_newtype {
    ($name:ident) => {
        /// ULID-backed identifier. Time-ordered; safe to pre-allocate.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub ulid::Ulid);

        impl $name {
            pub fn new() -> Self {
                Self(ulid::Ulid::new())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = ulid::DecodeError;
            fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
                Ok(Self(s.parse()?))
            }
        }
    };
}

id_newtype!(CursorId);
