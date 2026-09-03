//! Input：链上的一次输入，建模为节点。正文内联进 journal meta
//! （text/actor/tools 只有几百字节），没有外部正文文件。

use serde::{Deserialize, Serialize};

use crate::id::NodeId;
use crate::node::{truncate_preview, Kind, Node, now_millis};

/// Input meta = journal 平铺字段，必填。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Input {
    /// 版本边界容忍缺省：迁移直通的手坏旧行可能没有 text（正常数据恒有）。
    #[serde(default)]
    pub text: String,
    pub actor: String,
    /// 本轮的工具覆盖（展开后的显式列表，wire 层的 None 在 commit 前
    /// 就地展开）。空数组 = 未记录（旧数据）或该轮无工具。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

impl Kind for Input {
    type Data = ();
}

impl Input {
    /// 构造一个已提交形态的 Input 节点（预览截好、`data = Some(())`）。
    /// `context_refs` 实践中恒为空；需要时直接改 pub 字段。
    pub fn node(
        id: NodeId,
        parent: Option<NodeId>,
        text: impl Into<String>,
        actor: impl Into<String>,
        tools: Vec<String>,
        created_by: Option<NodeId>,
    ) -> Node<Input> {
        let text = text.into();
        let preview = truncate_preview(&text, 80);
        Node {
            id,
            parent,
            context_refs: vec![],
            created_by,
            created_at: now_millis(),
            preview,
            kind: Input {
                text,
                actor: actor.into(),
                tools,
            },
            data: Some(()),
        }
    }
}
