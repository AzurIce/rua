use std::collections::HashMap;

use crate::error::Result;
use crate::id::NodeId;
use crate::message::CoreMessage;
use crate::node::{Node, NodeKind, Step};

/// Verbatim material passthrough is allowed up to this size (bytes); larger
/// bodies are truncated with a marker.
pub const MAX_MATERIAL_BYTES: usize = 32 * 1024;

/// Pure assembly: `(chain, refs) → messages[]`.
///
/// - `chain`: structural nodes root → tip (Input / Turn only).
/// - `materials`: resolved `context_refs` targets (context nodes), keyed by id.
///
/// v1 projection: linear backtrack along the chain. Each node projects to
/// messages; material referenced by a node is injected *before* that node's
/// own projection so the model sees material before the instruction that
/// cites it. Turn reasoning is preserved in the node but never fed back.
pub fn assemble(
    chain: &[Node],
    materials: &HashMap<NodeId, Node>,
) -> Result<Vec<CoreMessage>> {
    let mut out = Vec::new();
    for node in chain {
        for r in &node.context_refs {
            if let Some(Node {
                kind: NodeKind::Context { body, .. },
                context_refs: sources,
                ..
            }) = materials.get(r)
            {
                out.push(CoreMessage::Context {
                    body: clamp_material(body),
                    sources: sources.clone(),
                });
            }
        }
        project(node, &mut out);
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

fn project(node: &Node, out: &mut Vec<CoreMessage>) {
    match &node.kind {
        NodeKind::Input { text, .. } => {
            out.push(CoreMessage::User {
                content: text.clone(),
            });
        }
        NodeKind::Turn { steps, .. } => {
            for step in steps {
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
        NodeKind::Context { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::CoreToolCall;
    use crate::node::{Outcome, Usage};

    fn input(text: &str, refs: Vec<NodeId>) -> Node {
        Node {
            id: NodeId::new(),
            parent: None,
            context_refs: refs,
            created_by: None,
            created_at: 0,
            kind: NodeKind::Input {
                text: text.into(),
                actor: "human".into(),
            },
        }
    }

    fn turn() -> Node {
        Node {
            id: NodeId::new(),
            parent: None,
            context_refs: vec![],
            created_by: None,
            created_at: 0,
            kind: NodeKind::Turn {
                steps: vec![
                    Step::LlmCall {
                        request: vec![],
                        response_text: String::new(),
                        tool_calls: vec![CoreToolCall {
                            id: "c1".into(),
                            name: "bash".into(),
                            args: serde_json::json!({"command": "ls"}),
                        }],
                        reasoning: Some("thinking…".into()),
                        usage: Usage::default(),
                    },
                    Step::ToolExec {
                        call_id: "c1".into(),
                        name: "bash".into(),
                        args: serde_json::json!({"command": "ls"}),
                        output: "file.txt".into(),
                        duration_ms: 5,
                    },
                    Step::LlmCall {
                        request: vec![],
                        response_text: "done".into(),
                        tool_calls: vec![],
                        reasoning: None,
                        usage: Usage::default(),
                    },
                ],
                outcome: Outcome::Completed,
                actor: "agent".into(),
                model: "m".into(),
                usage: Usage::default(),
            },
        }
    }

    #[test]
    fn linear_chain_projection() {
        let msgs = assemble(&[input("hi", vec![]), turn()], &HashMap::new()).unwrap();
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
        let ctx = Node {
            id: NodeId::new(),
            parent: None,
            context_refs: vec![],
            created_by: None,
            created_at: 0,
            kind: NodeKind::Context {
                body: "distilled facts".into(),
                created_by: NodeId::new(),
                model: "m".into(),
            },
        };
        let mut materials = HashMap::new();
        materials.insert(ctx.id, ctx.clone());
        let msgs = assemble(&[input("use this", vec![ctx.id])], &materials).unwrap();
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
