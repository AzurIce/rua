//! Context：蒸馏材料节点。永不在会话链上（不可作 cursor 落点、无 parent）。
//! 正文外部化为 `contexts/<ulid>.md` 纯文本文件（tmp + rename 原子写）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::error::Result;
use crate::id::NodeId;
use crate::node::{Data, Kind, Node, Turn, now_millis, truncate_preview};

/// Context meta = journal 平铺字段，必填。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Context {
    /// 溯源边（展示用）：蒸馏来源节点，异构目标，不经装配加载、commit 不
    /// 校验存在性（允许悬空）。落盘/wire 键名沿用 `context_refs`（UI 图
    /// 视图与旧 journal 都用它），`sources` 作为读入别名。
    #[serde(rename = "context_refs", alias = "sources", default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<Ulid>,
    /// 因果边：由哪个 Turn 的蒸馏动作产生（kind 级字段；类型层面独立，
    /// 不与 Input 的 `created_by` spawn 溯源混用）。`None` = 非轮内产生
    /// （如端点直接创建）——没有产生者就是没有，不说谎。旧格式 journal 行
    /// 写作 `created_by`，别名直读。
    #[serde(alias = "created_by", default, skip_serializing_if = "Option::is_none")]
    pub distilled_from: Option<NodeId<Turn>>,
    /// 蒸馏所用模型。缺省空串只服务迁移直通的手坏旧行。
    #[serde(default)]
    pub model: String,
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

impl Data for ContextData {
    fn path(root: &Path, id: Ulid) -> PathBuf {
        root.join("contexts").join(format!("{id}.md"))
    }

    /// 读正文：整读。journal 声称一个 Context 已提交而文件缺席 = 数据
    /// 损坏，报错而非静默读成空（`DataStore::entry` 把 NotFound 翻译成
    /// `NodeNotFound`）。
    fn load(path: &Path) -> Result<Self> {
        Ok(ContextData {
            body: std::fs::read_to_string(path)?,
        })
    }

    /// 写正文：tmp + rename 原子写（调用方已确认文件不存在——节点不可变）。
    fn save(&self, path: &Path) -> Result<()> {
        let tmp = path
            .parent()
            .expect("context path has a parent dir")
            .join(format!(".{}.tmp", path.file_name().expect("context file name").to_string_lossy()));
        std::fs::write(&tmp, &self.body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

impl Context {
    /// 构造一个已提交形态的 Context 节点（预览从正文截好）。`body` 只用于
    /// 推导 preview——正文本身归 DataStore（先 `DataStore::create` 写入，
    /// 再 commit 这里的节点）。
    pub fn node(
        id: NodeId<Context>,
        sources: Vec<Ulid>,
        distilled_from: Option<NodeId<Turn>>,
        model: impl Into<String>,
        body: &str,
    ) -> Node<Context> {
        Node {
            id,
            created_at: now_millis(),
            preview: truncate_preview(body, 80),
            kind: Context {
                sources,
                distilled_from,
                model: model.into(),
            },
        }
    }
}
