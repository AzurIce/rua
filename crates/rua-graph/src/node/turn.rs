//! Turn：一轮完整 agent 交互。meta 进 journal；正文是
//! `turns/<ulid>.jsonl` 轮内事件流（引擎 sink 逐行增量追加，崩溃不丢
//! 轮内进度），折叠成内存里的 [`TurnData`]。

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::id::NodeId;
use crate::message::CoreMessage;
use crate::node::{Outcome, truncate_preview, Kind, Node, Usage, now_millis};
use crate::store::Store;

/// Turn meta = journal 平铺字段，必填。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
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

impl Turn {
    /// 构造一个已提交形态的 Turn 节点：context_tokens（最后一次有产量的
    /// LlmCall 的 input tokens）与 preview（最后一条非空 response）从
    /// steps 算好。
    #[allow(clippy::too_many_arguments)]
    pub fn node(
        id: NodeId,
        parent: Option<NodeId>,
        context_refs: Vec<NodeId>,
        created_by: Option<NodeId>,
        outcome: Outcome,
        actor: impl Into<String>,
        model: impl Into<String>,
        usage: Usage,
        tools: Vec<String>,
        data: TurnData,
    ) -> Node<Turn> {
        let final_text = data
            .steps
            .iter()
            .rev()
            .find_map(|s| match s {
                Step::LlmCall {
                    response_text, ..
                } if !response_text.is_empty() => Some(response_text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let context_tokens = data.steps.iter().rev().find_map(|s| match s {
            Step::LlmCall { usage, .. } if usage.input_tokens > 0 => Some(usage.input_tokens),
            _ => None,
        });
        Node {
            id,
            parent,
            context_refs,
            created_by,
            created_at: now_millis(),
            preview: truncate_preview(&final_text, 80),
            kind: Turn {
                outcome,
                actor: actor.into(),
                model: model.into(),
                usage,
                context_tokens,
                tools,
            },
            data: Some(data),
        }
    }

    /// 读正文：jsonl 逐行折叠成 steps（Init 锚点不是 step）。
    /// 缺失文件 = 空正文；撕裂尾行容忍（崩溃在 append 中途）。
    pub fn read_data(store: &Store, id: NodeId) -> Result<TurnData> {
        let steps = store
            .read_turn_lines(id)?
            .into_iter()
            .filter_map(TurnLine::into_step)
            .collect();
        Ok(TurnData { steps })
    }

    /// commit 时的正文收尾（幂等）：sink 已增量写过（文件存在）则跳过；
    /// 直接提交路径（无 sink，如测试）把 steps 整体转出（无 Init 行）。
    pub fn write_data(store: &Store, id: NodeId, data: &TurnData) -> Result<()> {
        if store.has_turn_lines(id) {
            return Ok(());
        }
        for step in &data.steps {
            store.append_turn_line(id, &TurnLine::from(step.clone()))?;
        }
        Ok(())
    }

    /// The turn's init anchor: the first LLM call's full request snapshot
    /// (`None` when the body has no `Init` line, e.g. direct-commit paths).
    pub fn init_anchor(store: &Store, id: NodeId) -> Result<Option<Vec<CoreMessage>>> {
        Ok(store.read_turn_lines(id)?.into_iter().find_map(|line| {
            match line {
                TurnLine::Init { request } => Some(request),
                _ => None,
            }
        }))
    }
}
