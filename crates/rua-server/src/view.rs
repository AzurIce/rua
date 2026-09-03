//! Wire view of a node for the REST API. `Step::LlmCall` no longer stores
//! the request on disk, but the UI (inspect 侧栏的回合快照) still expects
//! it verbatim in `GET /api/nodes/{id}` responses. This view re-materializes
//! each call's request by replaying the turn: start from the `init` anchor
//! (or an empty vec) and fold — an `LlmCall` step is emitted with the
//! accumulated messages as its `request`, then its own Assistant message is
//! pushed; a `ToolExec` step pushes a ToolResult. The serde shape matches
//! rua-ui's `types.rs` exactly (`Node`/`Step`/`CoreMessageView`).
//! （chain 端点只回轻量 NodeMeta，不经此视图。）

use rua_graph::id::NodeId;
use rua_graph::message::{CoreMessage, CoreToolCall};
use rua_graph::node::{Node, NodeKind, Outcome, Step, Usage};
use serde::Serialize;

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
    pub kind: NodeKindView,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeKindView {
    Input {
        text: String,
        actor: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tools: Vec<String>,
    },
    Turn {
        steps: Vec<StepView>,
        outcome: Outcome,
        actor: String,
        model: String,
        #[serde(default)]
        usage: Usage,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tools: Vec<String>,
    },
    Context {
        body: String,
        created_by: NodeId,
        model: String,
    },
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

/// Build the wire view of a node. `init` is the turn's init anchor
/// (`Graph::turn_init`; ignored for non-turn kinds).
pub fn node_view(node: &Node, init: Option<Vec<CoreMessage>>) -> NodeView {
    let kind = match &node.kind {
        NodeKind::Input { text, actor, tools } => NodeKindView::Input {
            text: text.clone(),
            actor: actor.clone(),
            tools: tools.clone(),
        },
        NodeKind::Turn {
            steps,
            outcome,
            actor,
            model,
            usage,
            tools,
        } => NodeKindView::Turn {
            steps: step_views(steps, init.unwrap_or_default()),
            outcome: *outcome,
            actor: actor.clone(),
            model: model.clone(),
            usage: *usage,
            tools: tools.clone(),
        },
        NodeKind::Context {
            body,
            created_by,
            model,
        } => NodeKindView::Context {
            body: body.clone(),
            created_by: *created_by,
            model: model.clone(),
        },
    };
    NodeView {
        id: node.id,
        parent: node.parent,
        context_refs: node.context_refs.clone(),
        created_by: node.created_by,
        created_at: node.created_at,
        kind,
    }
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

    fn turn(steps: Vec<Step>) -> Node {
        Node {
            id: NodeId::new(),
            parent: None,
            context_refs: vec![],
            created_by: None,
            created_at: 0,
            kind: NodeKind::Turn {
                steps,
                outcome: Outcome::Completed,
                actor: "agent".into(),
                model: "m".into(),
                usage: Usage::default(),
                tools: vec![],
            },
        }
    }

    /// 重放折叠：request_0 = init 锚点，request_k 携带前序 Assistant /
    /// ToolResult；无锚点时从空 vec 起步。
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
        let NodeKindView::Turn { steps, .. } = &view.kind else {
            panic!("expected turn");
        };
        let StepView::LlmCall { request: r0, .. } = &steps[0] else {
            panic!("expected llm call");
        };
        assert_eq!(r0, &init);
        let StepView::LlmCall { request: r1, .. } = &steps[2] else {
            panic!("expected llm call");
        };
        assert_eq!(r1.len(), 4);
        assert!(matches!(&r1[2], CoreMessage::Assistant { tool_calls, .. } if tool_calls == &vec![bash_call]));
        assert!(
            matches!(&r1[3], CoreMessage::ToolResult { call_id, output, .. } if call_id == "c1" && output == "f.txt")
        );

        // 无锚点：从空 vec 起步。
        let view = node_view(&node, None);
        let NodeKindView::Turn { steps, .. } = &view.kind else {
            panic!("expected turn");
        };
        let StepView::LlmCall { request: r0, .. } = &steps[0] else {
            panic!("expected llm call");
        };
        assert!(r0.is_empty());
    }
}
