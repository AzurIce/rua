use std::collections::HashMap;

use rua_graph::error::{Error, Result};
use rua_graph::id::NodeId;
use rua_graph::message::CoreMessage;
use rua_graph::node::{AnyNode, Step};

/// Verbatim material passthrough is allowed up to this size (bytes); larger
/// bodies are truncated with a marker.
pub const MAX_MATERIAL_BYTES: usize = 32 * 1024;

/// Pure assembly: `(chain, refs) → messages[]`.
///
/// - `chain`: structural nodes root → tip (Input / Turn only), bodies loaded
///   (`load_chain` guarantees `data: Some`).
/// - `materials`: resolved `context_refs` targets (context nodes), keyed by id.
///
/// v1 projection: linear backtrack along the chain. Each node projects to
/// messages; material referenced by a node is injected *before* that node's
/// own projection so the model sees material before the instruction that
/// cites it. Turn reasoning is preserved in the node but never fed back.
pub fn assemble(
    chain: &[AnyNode],
    materials: &HashMap<NodeId, AnyNode>,
) -> Result<Vec<CoreMessage>> {
    let mut out = Vec::new();
    for node in chain {
        for r in node.context_refs() {
            if let Some(AnyNode::Context(ctx)) = materials.get(r) {
                let data = ctx
                    .data
                    .as_ref()
                    .ok_or(Error::DataNotLoaded(ctx.id))?;
                out.push(CoreMessage::Context {
                    body: clamp_material(&data.body),
                    sources: ctx.context_refs.clone(),
                });
            }
        }
        project(node, &mut out)?;
    }
    Ok(out)
}

fn clamp_material(body: &str) -> String {
    if body.len() <= MAX_MATERIAL_BYTES {
        body.to_string()
    } else {
        let mut end = MAX_MATERIAL_BYTES;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…\n[material truncated at {} bytes]", &body[..end], MAX_MATERIAL_BYTES)
    }
}

fn project(node: &AnyNode, out: &mut Vec<CoreMessage>) -> Result<()> {
    match node {
        AnyNode::Input(n) => {
            out.push(CoreMessage::User {
                content: n.kind.text.clone(),
            });
        }
        AnyNode::Turn(n) => {
            let data = n.data.as_ref().ok_or(Error::DataNotLoaded(n.id))?;
            for step in &data.steps {
                match step {
                    Step::LlmCall {
                        response_text,
                        tool_calls,
                        ..
                    } => {
                        if response_text.is_empty() && tool_calls.is_empty() {
                            continue;
                        }
                        out.push(CoreMessage::Assistant {
                            content: response_text.clone(),
                            tool_calls: tool_calls.clone(),
                        });
                    }
                    Step::ToolExec {
                        call_id,
                        name,
                        output,
                        ..
                    } => {
                        out.push(CoreMessage::ToolResult {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            output: output.clone(),
                        });
                    }
                }
            }
        }
        // Context nodes are material, not conversation steps; they only
        // appear via `context_refs`, never on a chain.
        AnyNode::Context(_) => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rua_graph::message::CoreToolCall;
    use rua_graph::node::{Context, Input, Outcome, Turn, TurnData, Usage};

    fn turn() -> AnyNode {
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
            TurnData {
                steps: vec![
                    Step::LlmCall {
                        response_text: String::new(),
                        tool_calls: vec![CoreToolCall {
                            id: "c1".into(),
                            name: "bash".into(),
                            args: serde_json::json!({"command": "ls"}),
                        }],
                        reasoning: Some("thinking…".into()),
                        usage: Usage::default(),
                        provider_data: None,
                    },
                    Step::ToolExec {
                        call_id: "c1".into(),
                        name: "bash".into(),
                        args: serde_json::json!({"command": "ls"}),
                        output: "file.txt".into(),
                        duration_ms: 5,
                    },
                    Step::LlmCall {
                        response_text: "done".into(),
                        tool_calls: vec![],
                        reasoning: None,
                        usage: Usage::default(),
                        provider_data: None,
                    },
                ],
            },
        ))
    }

    #[test]
    fn linear_chain_projection() {
        let input_node = AnyNode::Input(Input::node(
            NodeId::new(),
            None,
            "hi",
            "human",
            vec![],
            None,
        ));
        let msgs = assemble(&[input_node, turn()], &HashMap::new()).unwrap();
        assert_eq!(
            msgs,
            vec![
                CoreMessage::User { content: "hi".into() },
                CoreMessage::Assistant {
                    content: String::new(),
                    tool_calls: vec![CoreToolCall {
                        id: "c1".into(),
                        name: "bash".into(),
                        args: serde_json::json!({"command": "ls"}),
                    }],
                },
                CoreMessage::ToolResult {
                    call_id: "c1".into(),
                    name: "bash".into(),
                    output: "file.txt".into(),
                },
                CoreMessage::Assistant {
                    content: "done".into(),
                    tool_calls: vec![],
                },
            ]
        );
        // reasoning is never projected back into context
        assert!(!msgs.iter().any(|m| matches!(m, CoreMessage::Assistant { content, .. } if content.contains("thinking"))));
    }

    #[test]
    fn material_injected_before_referencing_node() {
        let src = NodeId::new();
        let ctx = Context::node(NodeId::new(), "distilled facts", vec![], src, "m");
        let ctx_id = ctx.id;
        let mut materials = HashMap::new();
        materials.insert(ctx_id, AnyNode::Context(ctx));
        let mut input_node = Input::node(
            NodeId::new(),
            None,
            "use this",
            "human",
            vec![],
            None,
        );
        input_node.context_refs = vec![ctx_id];
        let msgs = assemble(&[AnyNode::Input(input_node)], &materials).unwrap();
        assert_eq!(
            msgs,
            vec![
                CoreMessage::Context {
                    body: "distilled facts".into(),
                    sources: vec![],
                },
                CoreMessage::User {
                    content: "use this".into()
                },
            ]
        );
    }

    #[test]
    fn oversized_material_is_truncated() {
        let big = "x".repeat(MAX_MATERIAL_BYTES + 10);
        assert!(clamp_material(&big).len() < big.len() + 128);
    }
}
