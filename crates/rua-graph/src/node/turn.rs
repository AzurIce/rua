//! Turn：一轮完整 agent 交互。meta 进 journal；正文是
//! `turns/<ulid>.jsonl` 轮内事件流（engine sink 经 `Entry<Turn>::append`
//! 逐行增量追加，崩溃不丢轮内进度），折叠成内存里的 [`TurnData`]。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::error::{Error, Result};
use crate::id::NodeId;
use crate::message::CoreMessage;
use crate::node::{Data, Input, Kind, Node, Outcome, Usage, now_millis, truncate_preview};

/// Turn meta = journal 平铺字段，必填。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    /// 相继边：一轮必然回应一个 Input（不可选；链严格交替）。
    pub parent: NodeId<Input>,
    pub outcome: Outcome,
    pub actor: String,
    pub model: String,
    #[serde(default)]
    pub usage: Usage,
    /// 该回合最后一次 LLM 调用实际吃掉的上下文量（input tokens，含缓存；
    /// 构造时从 steps 算好，chain 端点免读正文）。无 LLM 调用（纯失败
    /// 回合）为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// 该轮实际生效的工具集（规范序记录，非配置）。空 = 未记录（旧数据）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

/// One step inside a turn. The full interior of a turn is preserved;
/// operations on the graph only ever address whole turns.
///
/// `LlmCall` deliberately does not store the request messages: inside a turn
/// the history is append-only, so request_k ≡ init anchor + replay of prior
/// steps (see docs/graph.md). The init anchor lives in the turn's jsonl body
/// (`TurnLine::Init`); the wire view re-materializes `request` server-side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Step {
    /// One LLM call: what came back (the request is replayable, not stored).
    LlmCall {
        /// Final assistant text (may be empty when the call only requested tools).
        response_text: String,
        /// Tool calls requested by this response.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<crate::message::CoreToolCall>,
        /// Reasoning trace, kept for audit but never fed back into context.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default)]
        usage: Usage,
        /// Provider-specific round-trip data (reasoning signatures, provider
        /// call ids, …), stored verbatim, never interpreted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_data: Option<serde_json::Value>,
    },
    /// One tool execution (incl. distill calls, per the design memo).
    ToolExec {
        call_id: String,
        name: String,
        args: serde_json::Value,
        output: String,
        duration_ms: u64,
    },
}

/// One line of a turn's jsonl body (`turns/<ulid>.jsonl`): the incremental,
/// as-it-happens record appended by the engine's sink during the turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnLine {
    /// Full request snapshot of the first LLM call (system prompt + initial
    /// history). At most one per turn; the anchor for request replay.
    /// Not a step: `into_step` maps it to `None`.
    Init { request: Vec<CoreMessage> },
    /// Same fields as `Step::LlmCall`.
    LlmCall {
        response_text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<crate::message::CoreToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default)]
        usage: Usage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_data: Option<serde_json::Value>,
    },
    /// Same fields as `Step::ToolExec`.
    ToolExec {
        call_id: String,
        name: String,
        args: serde_json::Value,
        output: String,
        duration_ms: u64,
    },
}

impl From<Step> for TurnLine {
    fn from(step: Step) -> Self {
        match step {
            Step::LlmCall {
                response_text,
                tool_calls,
                reasoning,
                usage,
                provider_data,
            } => TurnLine::LlmCall {
                response_text,
                tool_calls,
                reasoning,
                usage,
                provider_data,
            },
            Step::ToolExec {
                call_id,
                name,
                args,
                output,
                duration_ms,
            } => TurnLine::ToolExec {
                call_id,
                name,
                args,
                output,
                duration_ms,
            },
        }
    }
}

impl TurnLine {
    /// Fold a line back into a step; `Init` is an anchor, not a step.
    pub fn into_step(self) -> Option<Step> {
        match self {
            TurnLine::Init { .. } => None,
            TurnLine::LlmCall {
                response_text,
                tool_calls,
                reasoning,
                usage,
                provider_data,
            } => Some(Step::LlmCall {
                response_text,
                tool_calls,
                reasoning,
                usage,
                provider_data,
            }),
            TurnLine::ToolExec {
                call_id,
                name,
                args,
                output,
                duration_ms,
            } => Some(Step::ToolExec {
                call_id,
                name,
                args,
                output,
                duration_ms,
            }),
        }
    }
}

/// Turn 正文：折叠后的 steps（Init 锚点不是 step，不在这里）。
/// `Serialize` 供详情端点的 `kind: {type, ...meta, ...data}` 平铺；
/// 反序列化不走 serde（正文从 jsonl 折叠而来）。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TurnData {
    pub steps: Vec<Step>,
}

impl Kind for Turn {
    type Data = TurnData;
}

impl Data for TurnData {
    fn path(root: &Path, id: Ulid) -> PathBuf {
        root.join("turns").join(format!("{id}.jsonl"))
    }

    /// 读正文：jsonl 逐行折叠成 steps（Init 锚点不是 step）。
    /// 缺失文件 = 空正文（首轮前就失败的闭环）；撕裂尾行容忍（崩溃在
    /// append 中途），中间坏行报错。
    fn load(path: &Path) -> Result<Self> {
        let steps = read_lines(path)?
            .into_iter()
            .filter_map(TurnLine::into_step)
            .collect();
        Ok(TurnData { steps })
    }

    /// 整体写出正文（无 Init 行）：迁移/克隆路径用。调用方（
    /// `DataStore::create`）已确认文件不存在。
    fn save(&self, path: &Path) -> Result<()> {
        let mut bytes = Vec::new();
        for step in &self.steps {
            bytes.extend_from_slice(&serde_json::to_vec(&TurnLine::from(step.clone()))?);
            bytes.push(b'\n');
        }
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

/// 追加一行到正文文件（首次追加时创建），耐久性与 journal 同级
/// （append + `sync_data`）。`Entry<Turn>::append` 与迁移路径共用。
pub(crate) fn append_line(path: &Path, line: &TurnLine) -> Result<()> {
    use std::io::Write;
    let mut line_bytes = serde_json::to_vec(line)?;
    line_bytes.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(&line_bytes)?;
    file.sync_data()?;
    Ok(())
}

/// Read all lines of a turn body, tolerating a truncated/corrupt final
/// line (the daemon may have died mid-append; same policy as the journal).
/// A missing file reads as an empty body.
fn read_lines(path: &Path) -> Result<Vec<TurnLine>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    let mut lines = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(l) => lines.push(l),
            Err(e) => {
                // Only the last line may be incomplete (crash mid-append).
                if i + 1 == text.lines().count() {
                    break;
                }
                return Err(Error::JournalCorrupted {
                    line: i + 1,
                    reason: e.to_string(),
                });
            }
        }
    }
    Ok(lines)
}

/// The turn's init anchor: the first LLM call's full request snapshot
/// (`None` when the body has no `Init` line, e.g. direct-commit paths).
pub fn init_anchor(root: &Path, id: Ulid) -> Result<Option<Vec<CoreMessage>>> {
    Ok(read_lines(&TurnData::path(root, id))?
        .into_iter()
        .find_map(|line| match line {
            TurnLine::Init { request } => Some(request),
            _ => None,
        }))
}

impl Turn {
    /// 构造一个已提交形态的 Turn 节点：context_tokens（最后一次有产量的
    /// LlmCall 的 input tokens）与 preview（最后一条非空 response）从
    /// steps 算好。steps 只用于推导派生字段——正文归 DataStore，节点
    /// 本身不携带它。
    #[allow(clippy::too_many_arguments)]
    pub fn node(
        id: NodeId<Turn>,
        parent: NodeId<Input>,
        outcome: Outcome,
        actor: impl Into<String>,
        model: impl Into<String>,
        usage: Usage,
        tools: Vec<String>,
        steps: &[Step],
    ) -> Node<Turn> {
        let final_text = steps
            .iter()
            .rev()
            .find_map(|s| match s {
                Step::LlmCall {
                    response_text, ..
                } if !response_text.is_empty() => Some(response_text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let context_tokens = steps.iter().rev().find_map(|s| match s {
            Step::LlmCall { usage, .. } if usage.input_tokens > 0 => Some(usage.input_tokens),
            _ => None,
        });
        Node {
            id,
            created_at: now_millis(),
            preview: truncate_preview(&final_text, 80),
            kind: Turn {
                parent,
                outcome,
                actor: actor.into(),
                model: model.into(),
                usage,
                context_tokens,
                tools,
            },
        }
    }
}
