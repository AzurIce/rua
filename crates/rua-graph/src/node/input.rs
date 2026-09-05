//! Input：链上的一次输入，建模为节点。正文内联进 journal meta
//! （text/actor/tools 只有几百字节），没有外部正文文件。

use serde::{Deserialize, Serialize};

use crate::id::NodeId;
use crate::node::{Context, Kind, Node, Turn, now_millis, truncate_preview};

/// Input meta = journal 平铺字段，必填。边字段是自己的：相继边 `parent`、
/// 材料边 `context_refs`、因果边 `created_by`，目标全部带类型。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Input {
    /// 相继边：上一个 Turn；根 = None。链严格交替（Input ↔ Turn）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<NodeId<Turn>>,
    /// 材料边：本输入引用的材料（装配时加载进请求）。空 = 无引用。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_refs: Vec<NodeId<Context>>,
    /// 因果边：spawn 溯源（由哪个 Turn 内的 spawn_turn 调用产生），只打在
    /// spawn 出的根 Input 上；None = 用户/直接操作。唯一含义。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<NodeId<Turn>>,
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
    /// 构造一个已提交形态的 Input 节点（预览截好）。
    pub fn node(
        id: NodeId<Input>,
        parent: Option<NodeId<Turn>>,
        text: impl Into<String>,
        actor: impl Into<String>,
        tools: Vec<String>,
        created_by: Option<NodeId<Turn>>,
    ) -> Node<Input> {
        let text = text.into();
        let preview = truncate_preview(&text, 80);
        Node {
            id,
            created_at: now_millis(),
            preview,
            kind: Input {
                parent,
                context_refs: vec![],
                created_by,
                text,
                actor: actor.into(),
                tools,
            },
        }
    }
}
