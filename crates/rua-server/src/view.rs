//! Wire view of a node for the REST API. `Step::LlmCall` no longer stores
//! the request on disk, but the UI (inspect 侧栏的回合快照) still expects
//! it verbatim in `GET /api/nodes/{id}` responses. This view re-materializes
//! each call's request by replaying the turn: start from the `init` anchor
//! (or an empty vec) and fold — an `LlmCall` step is emitted with the
//! accumulated messages as its `request`, then its own Assistant message is
//! pushed; a `ToolExec` step pushes a ToolResult. The serde shape matches
//! rua-ui's `types.rs` exactly (`Node`/`Step`/`CoreMessageView`).
//! （chain 端点只回轻量 header，不经此视图。）

use rua_graph::message::{CoreMessage, CoreToolCall};
use rua_graph::node::{Meta, Step, Usage};
use rua_graph::Ulid;
use serde::Serialize;

/// 详情端点的节点视图：信封 + 完整 kind 形状（`kind: {type, ...meta,
/// ...data}`；Turn 的 steps 由 [`turn_steps_value`] 重放出逐字 request）。
/// 信封字段从各 kind 的 meta 边字段擦回平铺（wire 形状不变）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeView {
    pub id: Ulid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Ulid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_refs: Vec<Ulid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Ulid>,
    pub created_at: u64,
    pub kind: serde_json::Value,
}

/// Build the wire view of a node. `data` 是详情端点 kind 形状里的
/// `...data` 部分（Turn 用 [`turn_steps_value`] 造，Context 是
/// `{"body": ...}`，Input 为 None）。
pub fn node_view(meta: &Meta, data: Option<serde_json::Value>) -> NodeView {
    // 边字段擦回信封平铺（wire 兼容：UI 的 Node 形状不变）。
    let (parent, context_refs, created_by) = match meta {
        Meta::Input(n) => (
            n.kind.parent.map(|p| p.raw()),
            n.kind.context_refs.iter().map(|r| r.raw()).collect(),
            n.kind.created_by.map(|c| c.raw()),
        ),
        Meta::Turn(n) => (Some(n.kind.parent.raw()), Vec::new(), None),
        // Context 的 sources 在 wire 上沿用 context_refs 槽位（展示用溯源）。
        Meta::Context(n) => (None, n.kind.sources.clone(), None),
    };
    NodeView {
        id: meta.id(),
        parent,
        context_refs,
        created_by,
        created_at: meta.created_at(),
        kind: meta.kind_value(data.as_ref()),
    }
}

/// Turn 详情的 data 部分：`{"steps": [...]}`，每个 LlmCall step 带重放
/// 重建的逐字 request（init 锚点 + 轮内重放；首条 System = 当时生效的
/// 系统提示词；无锚点时从空 vec 起步）。
pub fn turn_steps_value(steps: &[Step], init: Option<Vec<CoreMessage>>) -> serde_json::Value {
    serde_json::json!({
        "steps": step_views(steps, init.unwrap_or_default()),
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepView {
    LlmCall {
        /// 重建的逐字请求快照（init 锚点 + 轮内重放；首条 System = 当时
        /// 生效的系统提示词）。
        request: Vec<CoreMessage>,
        response_text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<CoreToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default)]
        usage: Usage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_data: Option<serde_json::Value>,
    },
    ToolExec {
        call_id: String,
        name: String,
        args: serde_json::Value,
        output: String,
        duration_ms: u64,
    },
}

/// Replay a turn's steps into wire steps, re-materializing each LlmCall's
/// `request`. The fold mirrors the engine's in-turn history mutation exactly
/// (every call pushes its Assistant message, every exec its ToolResult).
fn step_views(steps: &[Step], init: Vec<CoreMessage>) -> Vec<StepView> {
    let mut acc = init;
    let mut out = Vec::with_capacity(steps.len());
    for step in steps {
        match step {
            Step::LlmCall {
                response_text,
                tool_calls,
                reasoning,
                usage,
                provider_data,
            } => {
                out.push(StepView::LlmCall {
                    request: acc.clone(),
                    response_text: response_text.clone(),
                    tool_calls: tool_calls.clone(),
                    reasoning: reasoning.clone(),
                    usage: *usage,
                    provider_data: provider_data.clone(),
                });
                acc.push(CoreMessage::Assistant {
                    content: response_text.clone(),
                    tool_calls: tool_calls.clone(),
                });
            }
            Step::ToolExec {
                call_id,
                name,
                args,
                output,
                duration_ms,
            } => {
                out.push(StepView::ToolExec {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                    output: output.clone(),
                    duration_ms: *duration_ms,
                });
                acc.push(CoreMessage::ToolResult {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    output: output.clone(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rua_graph::node::{Input, Node, Outcome, Turn};
    use rua_graph::NodeId;

    fn turn(parent: NodeId<Input>, steps: &[Step]) -> Meta {
        Meta::from(Turn::node(
            NodeId::new(),
            parent,
            Outcome::Completed,
            "agent",
            "m",
            Usage::default(),
            vec![],
            steps,
        ))
    }

    /// 重放折叠：request_0 = init 锚点，request_k 携带前序 Assistant /
    /// ToolResult；无锚点时从空 vec 起步。wire 的 kind 形状 = {type, ...meta,
    /// ...data}（steps 带 request），与 UI 镜像一致。
    #[test]
    fn replays_requests_from_init_anchor() {
        let call = |text: &str, tool_calls: Vec<CoreToolCall>| Step::LlmCall {
            response_text: text.into(),
            tool_calls,
            reasoning: None,
            usage: Usage::default(),
            provider_data: None,
        };
        let bash_call = CoreToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "ls"}),
        };
        let steps = vec![
            call("", vec![bash_call.clone()]),
            Step::ToolExec {
                call_id: "c1".into(),
                name: "bash".into(),
                args: serde_json::json!({"command": "ls"}),
                output: "f.txt".into(),
                duration_ms: 3,
            },
            call("done", vec![]),
        ];
        let input: Node<Input> = Input::node(NodeId::new(), None, "hi", "human", vec![], None);
        let meta = turn(input.id, &steps);
        let init = vec![
            CoreMessage::System {
                content: "sys".into(),
            },
            CoreMessage::User {
                content: "hi".into(),
            },
        ];
        let view = node_view(&meta, Some(turn_steps_value(&steps, Some(init.clone()))));
        assert_eq!(view.kind["type"], "turn");
        assert_eq!(view.kind["outcome"], "completed");
        // 相继边擦回信封平铺。
        assert_eq!(view.parent, Some(input.id.raw()));
        let wire_steps = view.kind["steps"].as_array().unwrap();
        let r0 = &wire_steps[0]["request"];
        assert_eq!(r0, &serde_json::to_value(&init).unwrap());
        let r1 = &wire_steps[2]["request"];
        assert_eq!(r1.as_array().unwrap().len(), 4);
        assert_eq!(wire_steps[2]["response_text"], "done");
        assert_eq!(wire_steps[1]["type"], "tool_exec");

        // 无锚点：从空 vec 起步。
        let view = node_view(&meta, Some(turn_steps_value(&steps, None)));
        let wire_steps = view.kind["steps"].as_array().unwrap();
        assert!(wire_steps[0]["request"].as_array().unwrap().is_empty());
    }
}
