//! 节点类型层：泛型 [`Node<T>`] + 唯一的和类型 [`AnyNode`]。
//!
//! 一种节点 = 一个 meta 类型（journal 平铺字段，必填）+ 一个正文类型
//! （[`Kind::Data`]，可选的外部正文）。`data: None` 的语义是**未加载**，
//! 不是「为空」；空正文是 `Some`（如零 LLM 调用失败轮的
//! `TurnData { steps: [] }`）。Input 无正文概念（`Data = ()`，恒 `Some(())`）。
//!
//! 类型分歧只声明两处：kind 清单在 [`AnyNode`]（唯一的 enum），
//! meta/正文模式在 [`Node<T>`]（唯一的泛型）。其余一切——journal/chain 的
//! header 形状、详情端点的完整形状、懒加载与正文收尾——都是引用这两个
//! 声明的 match 臂或泛型函数，编译器穷尽检查押着加新 kind 时逐处补齐。
//!
//! serde 约定：
//! - `Serialize for AnyNode` = header 形状（信封 + kind tag + meta 平铺，
//!   **永远跳过 data**）：journal 的 `node_committed` 行与 chain 端点的
//!   轻量记录共用；
//! - `Deserialize for AnyNode` = header 形状 → `data: None`（journal 重放，
//!   正文懒加载）；对旧格式行容忍缺省字段（`tools`/`usage`）与
//!   Context 的 `created_by` 别名；
//! - 详情端点的完整形状（`kind: {type, ...meta, ...data}`）由
//!   [`AnyNode::kind_json`] 现场生成，不经这里。

pub mod context;
pub mod input;
pub mod turn;

pub use context::{Context, ContextData};
pub use input::Input;
pub use turn::{Step, Turn, TurnData, TurnLine};

use serde::{Deserialize, Serialize};
use serde::de::Error as _;
use serde_json::{Map, Value};

use crate::id::NodeId;
use crate::store::Store;

/// 一种节点的正文类型。管道 trait：只有关联类型，没有方法、没有常量——
/// 格式知识（构造 / `read_data` / `write_data`）住在各 kind 模块的固有函数里。
pub trait Kind {
    type Data;
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

/// 不可变的已提交节点：信封（结构字段，所有 kind 共享）+ meta（`kind: T`，
/// 进 journal 的部分）+ 可选正文（`data`，按 kind 外部化）。
#[derive(Debug, Clone, PartialEq)]
pub struct Node<T: Kind> {
    pub id: NodeId,
    /// 唯一结构边（会话链）。
    pub parent: Option<NodeId>,
    /// 材料引用边（指向已提交节点）。
    pub context_refs: Vec<NodeId>,
    /// 创建者（provenance）：由哪个 turn 内部的 spawn_turn 工具调用产生。
    /// 只打在 spawn 出的根 Input 上；None = 用户/直接操作。唯一含义。
    pub created_by: Option<NodeId>,
    /// Unix epoch milliseconds.
    pub created_at: u64,
    /// Header/UI 预览（80 字符截断），构造时算好，chain 端点免读正文。
    pub preview: String,
    /// meta：进 journal 的 kind 专属字段，必填。
    pub kind: T,
    /// 正文：`None` = 未加载；`Some` = 已加载（可能内容为空）。
    pub data: Option<T::Data>,
}

/// 节点全集。加新 kind = 新模块 + 这里一个变体 + 编译器指路的若干 match 臂。
#[derive(Debug, Clone, PartialEq)]
pub enum AnyNode {
    Input(Node<Input>),
    Turn(Node<Turn>),
    Context(Node<Context>),
}

impl<T: Kind> Node<T> {
    /// 未加载的节点（journal 重放路径）：`data: None`。
    pub fn header(
        id: NodeId,
        parent: Option<NodeId>,
        context_refs: Vec<NodeId>,
        created_by: Option<NodeId>,
        created_at: u64,
        preview: String,
        kind: T,
    ) -> Self {
        Self {
            id,
            parent,
            context_refs,
            created_by,
            created_at,
            preview,
            kind,
            data: None,
        }
    }
}

impl AnyNode {
    // ---- 信封访问器（enum 上的统一入口；每个 kind 的绑定类型不同，
    //      所以是显式三臂而不是 or-pattern）----

    pub fn id(&self) -> NodeId {
        match self {
            AnyNode::Input(n) => n.id,
            AnyNode::Turn(n) => n.id,
            AnyNode::Context(n) => n.id,
        }
    }

    pub fn parent(&self) -> Option<NodeId> {
        match self {
            AnyNode::Input(n) => n.parent,
            AnyNode::Turn(n) => n.parent,
            AnyNode::Context(n) => n.parent,
        }
    }

    pub fn context_refs(&self) -> &[NodeId] {
        match self {
            AnyNode::Input(n) => &n.context_refs,
            AnyNode::Turn(n) => &n.context_refs,
            AnyNode::Context(n) => &n.context_refs,
        }
    }

    pub fn created_by(&self) -> Option<NodeId> {
        match self {
            AnyNode::Input(n) => n.created_by,
            AnyNode::Turn(n) => n.created_by,
            AnyNode::Context(n) => n.created_by,
        }
    }

    pub fn created_at(&self) -> u64 {
        match self {
            AnyNode::Input(n) => n.created_at,
            AnyNode::Turn(n) => n.created_at,
            AnyNode::Context(n) => n.created_at,
        }
    }

    pub fn preview(&self) -> &str {
        match self {
            AnyNode::Input(n) => &n.preview,
            AnyNode::Turn(n) => &n.preview,
            AnyNode::Context(n) => &n.preview,
        }
    }

    /// 覆盖 created_at（迁移路径保真用；正常提交走构造函数）。
    pub fn with_created_at(mut self, at: u64) -> Self {
        match &mut self {
            AnyNode::Input(n) => n.created_at = at,
            AnyNode::Turn(n) => n.created_at = at,
            AnyNode::Context(n) => n.created_at = at,
        }
        self
    }

    // ---- kind 访问 ----

    pub fn kind_tag(&self) -> &'static str {
        match self {
            AnyNode::Input(_) => "input",
            AnyNode::Turn(_) => "turn",
            AnyNode::Context(_) => "context",
        }
    }

    pub fn is_input(&self) -> bool {
        matches!(self, AnyNode::Input(_))
    }

    pub fn is_turn(&self) -> bool {
        matches!(self, AnyNode::Turn(_))
    }

    /// 材料节点（非结构节点）：不可作 cursor 落点、不进会话链。
    pub fn is_context(&self) -> bool {
        matches!(self, AnyNode::Context(_))
    }

    /// 结构节点（Input/Turn）vs 材料节点（Context）。cursor 落点、parent 边、
    /// context_refs 边界规则都建立在这个区分上。
    pub(crate) fn is_structural(&self) -> bool {
        !self.is_context()
    }

    pub fn input(&self) -> Option<&Node<Input>> {
        match self {
            AnyNode::Input(n) => Some(n),
            _ => None,
        }
    }

    pub fn turn(&self) -> Option<&Node<Turn>> {
        match self {
            AnyNode::Turn(n) => Some(n),
            _ => None,
        }
    }

    pub fn context(&self) -> Option<&Node<Context>> {
        match self {
            AnyNode::Context(n) => Some(n),
            _ => None,
        }
    }

    /// 已加载的 Turn 正文；未加载或非 Turn 报错（装配路径只接受
    /// `load_chain` 产出的节点，`None` 是 bug 不是正常分支）。
    pub fn turn_data(&self) -> crate::error::Result<&TurnData> {
        match self {
            AnyNode::Turn(n) => {
                n.data.as_ref().ok_or(crate::error::Error::DataNotLoaded(n.id))
            }
            _ => Err(crate::error::Error::DataNotLoaded(self.id())),
        }
    }

    pub fn context_data(&self) -> crate::error::Result<&ContextData> {
        match self {
            AnyNode::Context(n) => {
                n.data.as_ref().ok_or(crate::error::Error::DataNotLoaded(n.id))
            }
            _ => Err(crate::error::Error::DataNotLoaded(self.id())),
        }
    }

    /// 正文是否已在内存中（Input 恒已加载）。
    pub(crate) fn loaded(&self) -> bool {
        match self {
            AnyNode::Input(_) => true,
            AnyNode::Turn(n) => n.data.is_some(),
            AnyNode::Context(n) => n.data.is_some(),
        }
    }

    /// commit 的正文收尾（幂等）：Input no-op；Context tmp+rename 原子写
    /// （已存在报错——不可变）；Turn 已有正文（sink 已增量写过）则跳过，
    /// 否则把 steps 整体转写为 jsonl（无 Init 锚点的直接提交路径）。
    pub(crate) fn write_data(&self, store: &Store) -> crate::error::Result<()> {
        match self {
            AnyNode::Input(_) => Ok(()),
            AnyNode::Turn(n) => {
                let data = n
                    .data
                    .as_ref()
                    .ok_or(crate::error::Error::DataNotLoaded(n.id))?;
                Turn::write_data(store, n.id, data)
            }
            AnyNode::Context(n) => {
                let data = n
                    .data
                    .as_ref()
                    .ok_or(crate::error::Error::DataNotLoaded(n.id))?;
                Context::write_data(store, n.id, data)
            }
        }
    }

    /// 懒加载：按 kind 读正文填进 `data`（索引条目即缓存）。
    pub(crate) fn read_data(&mut self, store: &Store) -> crate::error::Result<()> {
        match self {
            AnyNode::Input(n) => n.data = Some(()),
            AnyNode::Turn(n) => {
                if n.data.is_none() {
                    n.data = Some(Turn::read_data(store, n.id)?);
                }
            }
            AnyNode::Context(n) => {
                if n.data.is_none() {
                    n.data = Some(Context::read_data(store, n.id)?);
                }
            }
        }
        Ok(())
    }

    // ---- wire ----

    /// header 形状的 JSON（信封 + kind tag + meta 平铺，无 data）。
    /// journal 行与 chain 端点共用；序列化不会失败，失败即程序 bug。
    pub fn header_value(&self) -> Value {
        serde_json::to_value(self).expect("node header serialization is infallible")
    }

    /// 详情端点的完整 kind 形状：`{type, ...meta, ...data}`。data 缺席时
    /// 只平铺 meta（调用方负责先懒加载）。
    pub fn kind_json(&self) -> Value {
        fn merged<T: Kind + Serialize>(
            node: &Node<T>,
            data: Option<&Value>,
        ) -> Map<String, Value> {
            let mut obj = match serde_json::to_value(&node.kind)
                .expect("node meta serialization is infallible")
            {
                Value::Object(o) => o,
                _ => unreachable!("node meta serializes to an object"),
            };
            if let Some(Value::Object(o)) = data {
                obj.extend(o.clone());
            }
            obj
        }
        let (tag, obj) = match self {
            AnyNode::Input(n) => ("input", merged(n, None)),
            // TurnData/ContextData 平铺进同一层；Input 的 data 是 ()，跳过。
            AnyNode::Turn(n) => (
                "turn",
                merged(
                    n,
                    n.data
                        .as_ref()
                        .map(|d| serde_json::to_value(d).expect("turn data serialization is infallible"))
                        .as_ref(),
                ),
            ),
            AnyNode::Context(n) => (
                "context",
                merged(
                    n,
                    n.data
                        .as_ref()
                        .map(|d| serde_json::to_value(d).expect("context data serialization is infallible"))
                        .as_ref(),
                ),
            ),
        };
        let mut obj = obj;
        obj.insert("type".into(), tag.into());
        Value::Object(obj)
    }
}

// ---- serde：header 形状（journal 行 / chain 端点）----

/// 信封反序列化（header 行里 kind 无关的部分；多余键被忽略）。
#[derive(Deserialize)]
struct HeaderEnvelope {
    id: NodeId,
    #[serde(default)]
    parent: Option<NodeId>,
    #[serde(default)]
    context_refs: Vec<NodeId>,
    #[serde(default)]
    created_by: Option<NodeId>,
    #[serde(default)]
    created_at: u64,
    #[serde(default)]
    preview: String,
}

/// 把信封 + kind tag + meta 平铺进一个 JSON map（data 永不落盘）。
/// meta 先单独序列化再合并：T 自身的 `skip_serializing_if` 得以生效
/// （serde 的 flatten 路径会忽略它们）。
fn push_header<T: Kind + Serialize>(
    map: &mut Map<String, Value>,
    node: &Node<T>,
    tag: &'static str,
) -> serde_json::Result<()> {
    map.insert("id".into(), serde_json::to_value(&node.id)?);
    if let Some(p) = node.parent {
        map.insert("parent".into(), serde_json::to_value(p)?);
    }
    if !node.context_refs.is_empty() {
        map.insert("context_refs".into(), serde_json::to_value(&node.context_refs)?);
    }
    if let Some(c) = node.created_by {
        map.insert("created_by".into(), serde_json::to_value(c)?);
    }
    map.insert("created_at".into(), serde_json::to_value(node.created_at)?);
    map.insert("preview".into(), serde_json::to_value(&node.preview)?);
    map.insert("kind".into(), tag.into());
    if let Value::Object(o) = serde_json::to_value(&node.kind)? {
        map.extend(o);
    }
    Ok(())
}

impl Serialize for AnyNode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error as _;
        let mut map = Map::new();
        let res = match self {
            AnyNode::Input(n) => push_header(&mut map, n, "input"),
            AnyNode::Turn(n) => push_header(&mut map, n, "turn"),
            AnyNode::Context(n) => push_header(&mut map, n, "context"),
        };
        res.map_err(S::Error::custom)?;
        map.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AnyNode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // 缓冲成 map 再按 tag 分派：各 meta 的 derive 解析自己的字段，
        // header 里的 `kind` 键作为未知字段被它们忽略。
        let map = Map::<String, Value>::deserialize(deserializer)?;
        let tag = map
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| D::Error::custom("node record missing `kind` tag"))?;
        let envelope = HeaderEnvelope::deserialize(Value::Object(map.clone()))
            .map_err(D::Error::custom)?;
        let parse = |map: Map<String, Value>| Value::Object(map);
        let node = match tag {
            "input" => {
                let kind = Input::deserialize(parse(map)).map_err(D::Error::custom)?;
                AnyNode::Input(Node {
                    id: envelope.id,
                    parent: envelope.parent,
                    context_refs: envelope.context_refs,
                    created_by: envelope.created_by,
                    created_at: envelope.created_at,
                    preview: envelope.preview,
                    kind,
                    data: Some(()),
                })
            }
            "turn" => {
                let kind = Turn::deserialize(parse(map)).map_err(D::Error::custom)?;
                AnyNode::Turn(Node {
                    id: envelope.id,
                    parent: envelope.parent,
                    context_refs: envelope.context_refs,
                    created_by: envelope.created_by,
                    created_at: envelope.created_at,
                    preview: envelope.preview,
                    kind,
                    data: None,
                })
            }
            "context" => {
                let kind = Context::deserialize(parse(map)).map_err(D::Error::custom)?;
                AnyNode::Context(Node {
                    id: envelope.id,
                    parent: envelope.parent,
                    context_refs: envelope.context_refs,
                    // 旧格式 Context 行的 `created_by` 是 kind 级蒸馏来源
                    //（由上面 alias 读进 distilled_from），信封 created_by
                    // 对 Context 恒 None。
                    created_by: None,
                    created_at: envelope.created_at,
                    preview: envelope.preview,
                    kind,
                    data: None,
                })
            }
            other => return Err(D::Error::custom(format!("unknown node kind: {other}"))),
        };
        Ok(node)
    }
}

/// `Node<T> → AnyNode`：每个 kind 一个一行实现。
impl From<Node<Input>> for AnyNode {
    fn from(node: Node<Input>) -> Self {
        AnyNode::Input(node)
    }
}

impl From<Node<Turn>> for AnyNode {
    fn from(node: Node<Turn>) -> Self {
        AnyNode::Turn(node)
    }
}

impl From<Node<Context>> for AnyNode {
    fn from(node: Node<Context>) -> Self {
        AnyNode::Context(node)
    }
}
