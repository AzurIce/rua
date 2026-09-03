//! Context：蒸馏材料节点。永不在会话链上（不可作 cursor 落点、无 parent），
//! `context_refs` 是蒸馏来源的溯源边。正文外部化为 `contexts/<ulid>.md`
//! 纯文本文件（tmp + rename 原子写）。

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::id::NodeId;
use crate::node::{truncate_preview, Kind, Node, now_millis};
use crate::store::Store;

/// Context meta = journal 平铺字段，必填。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Context {
    /// 蒸馏来源（kind 级字段；类型层面独立，不与信封 `created_by` 的
    /// spawn 溯源混用）。旧格式 journal 行写作 `created_by`，别名直读。
    /// 缺省 nil 只服务迁移直通的手坏旧行（正常数据恒有）。
    #[serde(alias = "created_by", default = "nil_node_id")]
    pub distilled_from: NodeId,
    /// 蒸馏所用模型。缺省空串只服务迁移直通的手坏旧行。
    #[serde(default)]
    pub model: String,
}

/// 版本边界缺省：ULID 全零。
fn nil_node_id() -> NodeId {
    NodeId(ulid::Ulid::nil())
}

/// Context 正文：纯文本材料。`Serialize` 供详情端点平铺；反序列化不走
/// serde（正文从 contexts/<id>.md 整读而来）。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ContextData {
    pub body: String,
}

impl Kind for Context {
    type Data = ContextData;
}

impl Context {
    /// 构造一个已提交形态的 Context 节点（预览从正文截好）。
    pub fn node(
        id: NodeId,
        body: impl Into<String>,
        context_refs: Vec<NodeId>,
        distilled_from: NodeId,
        model: impl Into<String>,
    ) -> Node<Context> {
        let body = body.into();
        let preview = truncate_preview(&body, 80);
        Node {
            id,
            parent: None,
            context_refs,
            created_by: None,
            created_at: now_millis(),
            preview,
            kind: Context {
                distilled_from,
                model: model.into(),
            },
            data: Some(ContextData { body }),
        }
    }

    /// 读正文：`contexts/<id>.md` 整读。
    pub fn read_data(store: &Store, id: NodeId) -> Result<ContextData> {
        Ok(ContextData {
            body: store.read_context(id)?,
        })
    }

    /// 写正文：tmp + rename 原子写；已存在报错（节点不可变）。
    pub fn write_data(store: &Store, id: NodeId, data: &ContextData) -> Result<()> {
        store.write_context(id, &data.body)
    }
}
