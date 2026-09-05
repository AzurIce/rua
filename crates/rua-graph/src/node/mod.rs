//! 节点类型层：泛型 [`Node<T>`]（纯 Ref）+ 唯一和类型 [`Meta`]（边界枚举）。
//!
//! 一个节点同时活在两个世界：结构世界（信封 + meta，journal 是唯一事实
//! 源，Graph 重放为 meta 索引）与数据世界（正文，`Data` trait 管文件格式
//! 与路径，[`crate::datastore::DataStore`] 管并发与缓存）。`Node<T>` 只承
//! 载结构存在——它不含正文，"未加载"不再是节点的状态，只是"DataStore 里
//! 还没有条目"。
//!
//! 一种节点 = 一个模块，自闭合三份知识：meta 类型（journal 平铺字段，
//! 必填，**含自己声明的边字段**）+ 正文类型（impl [`Data`]）+ impl [`Kind`]
//! 把两者绑成一对。
//!
//! 边不住信封：相继边（parent）、材料边（context_refs）、因果边
//! （created_by / distilled_from）都是各 meta 自己的字段，目标带类型。
//! 类型分歧只声明两处：kind 清单在 [`Meta`]（唯一的 enum，兼作**边的
//! 注册表**——`parent()` / `material_refs()` 把各 kind 的边字段擦成裸
//! Ulid 视图，新增 kind 或边时编译器押着补齐 match 臂），meta/正文模式在
//! [`Node<T>`]（唯一的泛型）。
//!
//! serde 约定：
//! - `Serialize for Meta` = header 形状（信封 + kind tag + meta 平铺）：
//!   journal 的 `node_committed` 行与 chain 端点的轻量记录共用；
//! - `Deserialize for Meta` = header 直读；对旧格式行容忍缺省字段
//!   （`tools`/`usage`），Context 的 `created_by`→`distilled_from`、
//!   `context_refs`→`sources` 两个别名直读；
//! - 详情端点的完整形状（`kind: {type, ...meta, ...data}`）由
//!   [`Meta::kind_value`] 现场合并，不经这里。

pub mod context;
pub mod input;
pub mod turn;

pub use context::{Context, ContextData};
pub use input::Input;
pub use turn::{Step, Turn, TurnData, TurnLine};

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde::de::Error as _;
use serde_json::{Map, Value};
use ulid::Ulid;

use crate::id::NodeId;

/// 一种节点的类型级定义，最小形态：只绑定正文类型。格式知识（路径构造、
/// 读、写）住在 [`Data`] 与各 kind 模块的固有函数里。
pub trait Kind: 'static {
    type Data: Data;
}

/// 一类正文的文件知识：路径构造、读、写。`Default` 供 DataStore 的
/// single-flight 占位（"已加载但为空"与"加载中"共用一个空形态）。
pub trait Data: Sized + Send + Sync + Default + 'static {
    /// 正文文件路径（每类不同；将来 Context 指向 vault 任意路径就覆盖这里）。
    fn path(root: &Path, id: Ulid) -> PathBuf;
    /// 从文件读出正文。缺失文件的语义各类自定：Turn 读成空（首轮前就失败
    /// 闭环），Context 报错。
    fn load(path: &Path) -> crate::error::Result<Self>;
    /// 整体写出正文（调用方负责先确认文件不存在——节点不可变）。
    fn save(&self, path: &Path) -> crate::error::Result<()>;
}

/// `Input::Data = ()`：无正文文件（正文内联在 journal header 的 text 里）。
/// 仅为完备性实现，`DataStore` 里不会有人注册 `NodeData<Input>`。
impl Data for () {
    fn path(_root: &Path, _id: Ulid) -> PathBuf {
        PathBuf::new()
    }
    fn load(_path: &Path) -> crate::error::Result<Self> {
        Ok(())
    }
    fn save(&self, _path: &Path) -> crate::error::Result<()> {
        Ok(())
    }
}

/// Normalized token usage, summed over a turn's LLM calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
}

impl Usage {
    pub fn add_assign(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
    }
}

/// Terminal state of a turn. A node existing at all means its turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Failed,
    Cancelled,
    /// The daemon died mid-turn; discovered on journal replay.
    Interrupted,
}

/// Unix epoch milliseconds.
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Header/UI 预览：截到 `max` 个字符，超出加省略号。
pub(crate) fn truncate_preview(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// 不可变已提交节点的结构存在：信封（所有 kind 共享的解释自由字段）+
/// meta（`kind: T`，进 journal，含各类自己的边字段）。正文不在这里——
/// 数据面由 `DataStore` 管。
#[derive(Debug, Clone, PartialEq)]
pub struct Node<T: Kind> {
    pub id: NodeId<T>,
    /// Unix epoch milliseconds.
    pub created_at: u64,
    /// Header/UI 预览（80 字符截断），构造时算好，chain 端点免读正文。
    pub preview: String,
    /// meta：进 journal 的 kind 专属字段，必填。
    pub kind: T,
}

/// 边界枚举：节点全集的和类型，兼作 tag 字符串与边的注册表（图上"怎么
/// 解读我"的唯一出处）。加新 kind = 新模块 + 这里一个变体 + 编译器指路的
/// 若干 match 臂。
#[derive(Debug, Clone, PartialEq)]
pub enum Meta {
    Input(Node<Input>),
    Turn(Node<Turn>),
    Context(Node<Context>),
}

impl Meta {
    // ---- 信封访问器 ----

    pub fn id(&self) -> Ulid {
        match self {
            Meta::Input(n) => n.id.raw(),
            Meta::Turn(n) => n.id.raw(),
            Meta::Context(n) => n.id.raw(),
        }
    }

    pub fn created_at(&self) -> u64 {
        match self {
            Meta::Input(n) => n.created_at,
            Meta::Turn(n) => n.created_at,
            Meta::Context(n) => n.created_at,
        }
    }

    pub fn preview(&self) -> &str {
        match self {
            Meta::Input(n) => &n.preview,
            Meta::Turn(n) => &n.preview,
            Meta::Context(n) => &n.preview,
        }
    }

    /// 覆盖 created_at（迁移/克隆路径保真用；正常提交走构造函数）。
    pub fn with_created_at(mut self, at: u64) -> Self {
        match &mut self {
            Meta::Input(n) => n.created_at = at,
            Meta::Turn(n) => n.created_at = at,
            Meta::Context(n) => n.created_at = at,
        }
        self
    }

    // ---- kind 判定 ----

    pub fn kind_tag(&self) -> &'static str {
        match self {
            Meta::Input(_) => "input",
            Meta::Turn(_) => "turn",
            Meta::Context(_) => "context",
        }
    }

    pub fn is_input(&self) -> bool {
        matches!(self, Meta::Input(_))
    }

    pub fn is_turn(&self) -> bool {
        matches!(self, Meta::Turn(_))
    }

    /// 材料节点（非结构节点）：不可作 cursor 落点、不进会话链。
    pub fn is_context(&self) -> bool {
        matches!(self, Meta::Context(_))
    }

    pub fn input(&self) -> Option<&Node<Input>> {
        match self {
            Meta::Input(n) => Some(n),
            _ => None,
        }
    }

    pub fn turn(&self) -> Option<&Node<Turn>> {
        match self {
            Meta::Turn(n) => Some(n),
            _ => None,
        }
    }

    pub fn context(&self) -> Option<&Node<Context>> {
        match self {
            Meta::Context(n) => Some(n),
            _ => None,
        }
    }

    // ---- 边注册表：图上"边怎么解释"的唯一出处 ----

    /// 相继边（结构回溯）：Input → 上一个 Turn；Turn → 它回应的 Input；
    /// Context 不上链。
    pub fn parent(&self) -> Option<Ulid> {
        match self {
            Meta::Input(n) => n.kind.parent.map(NodeId::raw),
            Meta::Turn(n) => Some(n.kind.parent.raw()),
            Meta::Context(_) => None,
        }
    }

    /// 材料引用边（前向信息流，装配时加载）：只有 Input 声明。
    /// `Context.sources` 是展示用溯源（异构、可悬空），不经这里。
    pub fn material_refs(&self) -> &[NodeId<Context>] {
        match self {
            Meta::Input(n) => &n.kind.context_refs,
            _ => &[],
        }
    }

    // ---- wire ----

    /// header 形状的 JSON（信封 + kind tag + meta 平铺）。
    /// journal 行与 chain 端点共用；序列化不会失败，失败即程序 bug。
    pub fn header_value(&self) -> Value {
        serde_json::to_value(self).expect("node header serialization is infallible")
    }

    /// 详情端点的完整 kind 形状：`{type, ...meta, ...data}`。`data` 由调用
    /// 方从 DataStore 读出后平铺进来（缺席 = 只平铺 meta）。
    pub fn kind_value(&self, data: Option<&Value>) -> Value {
        let mut obj = match self {
            Meta::Input(n) => serde_json::to_value(&n.kind),
            Meta::Turn(n) => serde_json::to_value(&n.kind),
            Meta::Context(n) => serde_json::to_value(&n.kind),
        }
        .expect("node meta serialization is infallible");
        {
            let obj = obj.as_object_mut().expect("node meta serializes to an object");
            obj.insert("type".into(), self.kind_tag().into());
            if let Some(Value::Object(data)) = data {
                obj.extend(data.clone());
            }
        }
        obj
    }
}

// ---- serde：header 形状（journal 行 / chain 端点）----

/// 信封反序列化（header 行里 kind 无关的部分；多余键被忽略）。
#[derive(Deserialize)]
struct HeaderEnvelope {
    id: Ulid,
    #[serde(default)]
    created_at: u64,
    #[serde(default)]
    preview: String,
}

/// 把信封 + kind tag + meta 平铺进一个 JSON map。meta 先单独序列化再合并：
/// T 自身的 `skip_serializing_if` 得以生效（serde 的 flatten 路径会忽略
/// 它们）。边字段（parent/context_refs/…）是 meta 的一部分，随 meta 平铺。
fn push_header<T: Kind + Serialize>(
    map: &mut Map<String, Value>,
    node: &Node<T>,
    tag: &'static str,
) -> serde_json::Result<()> {
    map.insert("id".into(), serde_json::to_value(node.id)?);
    map.insert("created_at".into(), serde_json::to_value(node.created_at)?);
    map.insert("preview".into(), serde_json::to_value(&node.preview)?);
    map.insert("kind".into(), tag.into());
    if let Value::Object(o) = serde_json::to_value(&node.kind)? {
        map.extend(o);
    }
    Ok(())
}

impl Serialize for Meta {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error as _;
        let mut map = Map::new();
        let res = match self {
            Meta::Input(n) => push_header(&mut map, n, "input"),
            Meta::Turn(n) => push_header(&mut map, n, "turn"),
            Meta::Context(n) => push_header(&mut map, n, "context"),
        };
        res.map_err(S::Error::custom)?;
        map.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Meta {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // 缓冲成 map 再按 tag 分派：各 meta 的 derive 解析自己的字段
        // （含边字段），header 里的 `kind` 键作为未知字段被它们忽略。
        let map = Map::<String, Value>::deserialize(deserializer)?;
        let tag = map
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| D::Error::custom("node record missing `kind` tag"))?;
        let envelope = HeaderEnvelope::deserialize(Value::Object(map.clone()))
            .map_err(D::Error::custom)?;
        let node = match tag {
            "input" => {
                let kind = Input::deserialize(Value::Object(map)).map_err(D::Error::custom)?;
                Meta::Input(Node {
                    id: NodeId::from_raw(envelope.id),
                    created_at: envelope.created_at,
                    preview: envelope.preview,
                    kind,
                })
            }
            "turn" => {
                let kind = Turn::deserialize(Value::Object(map)).map_err(D::Error::custom)?;
                Meta::Turn(Node {
                    id: NodeId::from_raw(envelope.id),
                    created_at: envelope.created_at,
                    preview: envelope.preview,
                    kind,
                })
            }
            "context" => {
                let kind = Context::deserialize(Value::Object(map)).map_err(D::Error::custom)?;
                Meta::Context(Node {
                    id: NodeId::from_raw(envelope.id),
                    created_at: envelope.created_at,
                    preview: envelope.preview,
                    kind,
                })
            }
            other => return Err(D::Error::custom(format!("unknown node kind: {other}"))),
        };
        Ok(node)
    }
}

/// `Node<T> → Meta`：每个 kind 一个一行实现。
impl From<Node<Input>> for Meta {
    fn from(node: Node<Input>) -> Self {
        Meta::Input(node)
    }
}

impl From<Node<Turn>> for Meta {
    fn from(node: Node<Turn>) -> Self {
        Meta::Turn(node)
    }
}

impl From<Node<Context>> for Meta {
    fn from(node: Node<Context>) -> Self {
        Meta::Context(node)
    }
}
