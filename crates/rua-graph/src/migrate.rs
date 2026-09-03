//! One-shot migration from the legacy on-disk layout to the current one.
//!
//! Legacy layout: `nodes/<ulid>.json` single files holding envelope + body
//! (`Step::LlmCall` embedding the full request message list). Current layout:
//! the journal is the only structural source of truth (Input bodies inline in
//! `NodeMeta.text`), Turn bodies are `turns/<ulid>.jsonl` event streams
//! (request dropped; the first call's request kept as the `init` anchor),
//! Context bodies are `contexts/<ulid>.md` text files.
//!
//! Runs at `Graph::open` when a non-empty `nodes/` directory exists. The old
//! `nodes/` and the old journal are moved into `.trash/migration-<millis>/`
//! before the new journal is written.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::Result;
use crate::graph::NodeMeta;
use crate::id::NodeId;
use crate::journal::JournalEvent;
use crate::message::{CoreMessage, CoreToolCall};
use crate::node::{Node, NodeKind, Outcome, TurnLine, Usage};
use crate::store::Store;

// ---- legacy serde types (mirror the old format; never written) ----

#[derive(Debug, Deserialize)]
struct LegacyNode {
    id: NodeId,
    #[serde(default)]
    parent: Option<NodeId>,
    #[serde(default)]
    context_refs: Vec<NodeId>,
    #[serde(default)]
    created_by: Option<NodeId>,
    created_at: u64,
    kind: LegacyNodeKind,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LegacyNodeKind {
    Input {
        text: String,
        actor: String,
        #[serde(default)]
        tools: Vec<String>,
    },
    Turn {
        steps: Vec<LegacyStep>,
        outcome: Outcome,
        actor: String,
        model: String,
        #[serde(default)]
        usage: Usage,
        #[serde(default)]
        tools: Vec<String>,
    },
    Context {
        body: String,
        created_by: NodeId,
        model: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LegacyStep {
    LlmCall {
        request: Vec<CoreMessage>,
        response_text: String,
        #[serde(default)]
        tool_calls: Vec<CoreToolCall>,
        #[serde(default)]
        reasoning: Option<String>,
        #[serde(default)]
        usage: Usage,
    },
    ToolExec {
        call_id: String,
        name: String,
        args: serde_json::Value,
        output: String,
        duration_ms: u64,
    },
}

/// Migrate `store`'s graph directory if it still holds a legacy `nodes/`
/// layout. Returns `true` when a migration ran.
pub(crate) fn migrate_legacy_nodes(store: &Store) -> Result<bool> {
    let nodes_dir = store.root().join("nodes");
    if !nodes_dir.is_dir() {
        return Ok(false);
    }
    let mut files: Vec<PathBuf> = fs::read_dir(&nodes_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    if files.is_empty() {
        return Ok(false);
    }
    files.sort();

    // 1. Read every legacy node; convert to the current model and write the
    //    new body files.
    let mut converted: Vec<Node> = Vec::with_capacity(files.len());
    for path in &files {
        let bytes = fs::read(path)?;
        let legacy: LegacyNode = serde_json::from_slice(&bytes)?;
        let node = convert(legacy, store)?;
        converted.push(node);
    }

    // 2. Move the legacy data aside before rewriting the journal in place.
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let trash = store.root().join(".trash").join(format!("migration-{millis}"));
    fs::create_dir_all(&trash)?;
    fs::rename(&nodes_dir, trash.join("nodes"))?;
    let journal = store.journal_path();
    let had_journal = journal.exists();
    if had_journal {
        fs::rename(&journal, trash.join("journal.jsonl"))?;
        // 3. Rewrite the journal: node metas are regenerated from the
        //    converted nodes (Input metas gain the inline `text`); cursor /
        //    turn events pass through unchanged.
        let old_events: Vec<JournalEvent> = {
            let text = fs::read_to_string(trash.join("journal.jsonl"))?;
            let mut events = Vec::new();
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                // 旧 journal 是迁移前最后写入的完整文件，不容忍坏行。
                events.push(serde_json::from_str(line)?);
            }
            events
        };
        let mut out: Vec<u8> = Vec::new();
        for ev in old_events {
            let ev = match ev {
                JournalEvent::NodeCommitted { meta } => {
                    match converted.iter().find(|n| n.id == meta.id) {
                        Some(node) => JournalEvent::NodeCommitted {
                            meta: NodeMeta::of(node),
                        },
                        // meta 没有对应正文文件（手坏的数据）：原样保留。
                        None => JournalEvent::NodeCommitted { meta },
                    }
                }
                other => other,
            };
            out.extend_from_slice(&serde_json::to_vec(&ev)?);
            out.push(b'\n');
        }
        let tmp = store.root().join(".journal.jsonl.tmp");
        fs::write(&tmp, out)?;
        fs::rename(&tmp, &journal)?;
    }

    eprintln!(
        "rua-graph: migrated {} legacy nodes in {} (backup at {})",
        converted.len(),
        store.root().display(),
        trash.display()
    );
    Ok(true)
}

/// Convert one legacy node to the current model, writing its new body file.
fn convert(legacy: LegacyNode, store: &Store) -> Result<Node> {
    let kind = match legacy.kind {
        LegacyNodeKind::Input { text, actor, tools } => NodeKind::Input { text, actor, tools },
        LegacyNodeKind::Turn {
            steps,
            outcome,
            actor,
            model,
            usage,
            tools,
        } => {
            // init 锚点：首个 LlmCall 的 request（该轮系统提示 + 初始历史）。
            let init = steps.iter().find_map(|s| match s {
                LegacyStep::LlmCall { request, .. } => Some(request.clone()),
                _ => None,
            });
            if let Some(request) = init {
                store.append_turn_line(legacy.id, &TurnLine::Init { request })?;
            }
            let mut new_steps = Vec::with_capacity(steps.len());
            for step in steps {
                let (line, new_step) = match step {
                    LegacyStep::LlmCall {
                        response_text,
                        tool_calls,
                        reasoning,
                        usage,
                        ..
                    } => {
                        let step = crate::node::Step::LlmCall {
                            response_text,
                            tool_calls,
                            reasoning,
                            usage,
                            provider_data: None,
                        };
                        (TurnLine::from(step.clone()), step)
                    }
                    LegacyStep::ToolExec {
                        call_id,
                        name,
                        args,
                        output,
                        duration_ms,
                    } => {
                        let step = crate::node::Step::ToolExec {
                            call_id,
                            name,
                            args,
                            output,
                            duration_ms,
                        };
                        (TurnLine::from(step.clone()), step)
                    }
                };
                store.append_turn_line(legacy.id, &line)?;
                new_steps.push(new_step);
            }
            NodeKind::Turn {
                steps: new_steps,
                outcome,
                actor,
                model,
                usage,
                tools,
            }
        }
        LegacyNodeKind::Context {
            body,
            created_by,
            model,
        } => {
            store.write_context(legacy.id, &body)?;
            NodeKind::Context {
                body,
                created_by,
                model,
            }
        }
    };
    Ok(Node {
        id: legacy.id,
        parent: legacy.parent,
        context_refs: legacy.context_refs,
        created_by: legacy.created_by,
        created_at: legacy.created_at,
        kind,
    })
}
