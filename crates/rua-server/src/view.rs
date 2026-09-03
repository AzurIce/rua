//! Wire view of a node for the REST API. `Step::LlmCall` no longer stores
//! the request on disk, but the UI (inspect 侧栏的回合快照) still expects
//! it verbatim in `GET /api/nodes/{id}` responses. This view re-materializes
//! each call's request by replaying the turn: start from the `init` anchor
//! (or an empty vec) and fold — an `LlmCall` step is emitted with the
//! accumulated messages as its `request`, then its own Assistant message is
//! pushed; a `ToolExec` step pushes a ToolResult. The serde shape matches
//! rua-ui's `types.rs` exactly (`Node`/`Step`/`CoreMessageView`).
//! （chain 端点只回轻量 header，不经此视图。）

use rua_graph::id::NodeId;
use rua_graph::message::{CoreMessage, CoreToolCall};
use rua_graph::node::{AnyNode, Step, Usage};
use serde::Serialize;

/// 详情端点的节点视图：信封 + 完整 kind 形状（`kind: {type, ...meta,
/// ...data}`；data 的 steps 由 [`step_views`] 重放出逐字 request）。
/// kind 内容来自 `AnyNode::kind_json`，没有第二个和类型声明。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeView {
    pub id: NodeId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<NodeId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_refs: Vec<NodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<NodeId>,
    pub created_at: u64,
    pub kind: serde_json::Value,
}

/// Build the wire view of a node. `init` is the turn's init anchor
/// (`Graph::turn_init`; ignored for non-turn kinds). 调用方先经
/// `Graph::node` 懒加载，正文必已就位；缺席时按空正文处理。
pub fn node_view(node: &AnyNode, init: Option<Vec<CoreMessage>>) -> NodeView {
    // Turn：重放 steps，把重建的 request 逐条塞回 wire step。
    if let AnyNode::Turn(n) = node {
        let steps: Vec<Step> = n
            .data
            .as_ref()
            .map(|d| d.steps.clone())
            .unwrap_or_default();
        let mut kind = node.kind_json();
        if let Some(obj) = kind.as_object_mut() {
            obj.insert(
                "steps".into(),
                serde_json::to_value(step_views(&steps, init.unwrap_or_default()))
                    .expect("step view serialization is infallible"),
            );
        }
        return NodeView {
            id: node.id(),
            parent: node.parent(),
            context_refs: node.context_refs().to_vec(),
            created_by: node.created_by(),
            created_at: node.created_at(),
            kind,
        };
    }
    NodeView {
        id: node.id(),
        parent: node.parent(),
        context_refs: node.context_refs().to_vec(),
        created_by: node.created_by(),
        created_at: node.created_at(),
        kind: node.kind_json(),
    }
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
    use rua_graph::node::{Outcome, Turn, TurnData};

    fn turn(steps: Vec<Step>) -> AnyNode {
        AnyNode::Turn(Turn::node(
            NodeId::new(),
            None,
            vec![],
            None,
            Outcome::Completed,
            "agent",
            "m",
            Usage::default(),
            vec![],
            TurnData { steps },
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
        let node = turn(vec![
            call("", vec![bash_call.clone()]),
            Step::ToolExec {
                call_id: "c1".into(),
                name: "bash".into(),
                args: serde_json::json!({"command": "ls"}),
                output: "f.txt".into(),
                duration_ms: 3,
            },
            call("done", vec![]),
        ]);
        let init = vec![
            CoreMessage::System {
                content: "sys".into(),
            },
            CoreMessage::User {
                content: "hi".into(),
            },
        ];
        let view = node_view(&node, Some(init.clone()));
        assert_eq!(view.kind["type"], "turn");
        assert_eq!(view.kind["outcome"], "completed");
        let steps = view.kind["steps"].as_array().unwrap();
        let r0 = &steps[0]["request"];
        assert_eq!(r0, &serde_json::to_value(&init).unwrap());
        let r1 = &steps[2]["request"];
        assert_eq!(r1.as_array().unwrap().len(), 4);
        assert_eq!(steps[2]["response_text"], "done");
        assert_eq!(steps[1]["type"], "tool_exec");

        // 无锚点：从空 vec 起步。
        let view = node_view(&node, None);
        let steps = view.kind["steps"].as_array().unwrap();
        assert!(steps[0]["request"].as_array().unwrap().is_empty());
    }
}
