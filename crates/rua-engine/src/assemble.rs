use std::collections::HashMap;

use rua_graph::datastore::DataStore;
use rua_graph::error::Result;
use rua_graph::message::CoreMessage;
use rua_graph::node::{Meta, Step};
use rua_graph::Ulid;

/// Verbatim material passthrough is allowed up to this size (bytes); larger
/// bodies are truncated with a marker.
pub const MAX_MATERIAL_BYTES: usize = 32 * 1024;

/// Assembly: `(chain, refs) + 数据面 → messages[]`。
///
/// - `chain`: structural node metas root → tip (Input / Turn only)，来自
///   `Graph::load_chain`；
/// - `materials`: resolved `context_refs` targets' metas, keyed by id；
/// - 正文经 `store`（DataStore）按 id 读取——装配不碰文件，只走数据面。
///
/// v1 projection: linear backtrack along the chain. Each node projects to
/// messages; material referenced by a node is injected *before* that node's
/// own projection so the model sees material before the instruction that
/// cites it. Turn reasoning is preserved in the node but never fed back.
pub fn assemble(
    chain: &[Meta],
    materials: &HashMap<Ulid, Meta>,
    store: &DataStore,
) -> Result<Vec<CoreMessage>> {
    let mut out = Vec::new();
    for node in chain {
        for r in node.material_refs() {
            if let Some(Meta::Context(ctx)) = materials.get(&r.raw()) {
                let data = store.entry(ctx.id)?.cloned()?;
                out.push(CoreMessage::Context {
                    body: clamp_material(&data.body),
                    sources: ctx.kind.sources.clone(),
                });
            }
        }
        project(node, store, &mut out)?;
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

fn project(node: &Meta, store: &DataStore, out: &mut Vec<CoreMessage>) -> Result<()> {
    match node {
        Meta::Input(n) => {
            out.push(CoreMessage::User {
                content: n.kind.text.clone(),
            });
        }
        Meta::Turn(n) => {
            let data = store.entry(n.id)?.cloned()?;
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
        Meta::Context(_) => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rua_graph::node::{Context, ContextData, Input, Node, Outcome, Turn, TurnData, Usage};
    use rua_graph::NodeId;

    /// 数据面最小环境（turns/ 与 contexts/ 目录由调用方建好——测试里直接
    /// 建；正常路径由 `Store::new` 建）。
    fn test_store(root: &std::path::Path) -> DataStore {
        std::fs::create_dir_all(root.join("turns")).unwrap();
        std::fs::create_dir_all(root.join("contexts")).unwrap();
        DataStore::new(root.to_path_buf())
    }

    fn sample_steps() -> Vec<Step> {
        vec![
            Step::LlmCall {
                response_text: String::new(),
                tool_calls: vec![rua_graph::CoreToolCall {
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
        ]
    }

    /// 造一个 Turn：正文进数据面（create），返回结构 meta。
    fn make_turn(store: &DataStore, parent: NodeId<Input>, steps: Vec<Step>) -> Meta {
        let id: NodeId<Turn> = store.allocate();
        store.create(id, TurnData { steps: steps.clone() }).unwrap();
        Meta::from(Turn::node(
            id,
            parent,
            Outcome::Completed,
            "agent",
            "m",
            Usage::default(),
            vec![],
            &steps,
        ))
    }

    #[test]
    fn linear_chain_projection() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let input: Node<Input> = Input::node(NodeId::new(), None, "hi", "human", vec![], None);
        let turn = make_turn(&store, input.id, sample_steps());
        let msgs = assemble(&[Meta::from(input), turn], &HashMap::new(), &store).unwrap();
        assert_eq!(
            msgs,
            vec![
                CoreMessage::User { content: "hi".into() },
                CoreMessage::Assistant {
                    content: String::new(),
                    tool_calls: vec![rua_graph::CoreToolCall {
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
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let ctx_id: rua_graph::NodeId<Context> = store.allocate();
        store
            .create(ctx_id, ContextData { body: "distilled facts".into() })
            .unwrap();
        let ctx: Node<Context> = Context::node(ctx_id, vec![], Some(NodeId::new()), "m", "distilled facts");
        let ctx_meta = Meta::from(ctx);
        let mut materials = HashMap::new();
        materials.insert(ctx_id.raw(), ctx_meta);
        let mut input_node = Input::node(NodeId::new(), None, "use this", "human", vec![], None);
        input_node.kind.context_refs = vec![ctx_id];
        let msgs = assemble(&[Meta::from(input_node)], &materials, &store).unwrap();
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
