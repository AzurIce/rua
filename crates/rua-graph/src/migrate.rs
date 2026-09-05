//! One-shot migration from the legacy on-disk layout to the current one.
//!
//! Legacy layout: `nodes/<ulid>.json` single files holding envelope + body
//! (`Step::LlmCall` embedding the full request message list). Current layout:
//! the journal is the only structural source of truth (Input bodies inline in
//! the header), Turn bodies are `turns/<ulid>.jsonl` event streams
//! (request dropped; the first call's request kept as the `init` anchor),
//! Context bodies are `contexts/<ulid>.md` text files.
//!
//! Runs at `Graph::open` when a non-empty `nodes/` directory exists. The old
//! `nodes/` and the old journal are moved into `.trash/migration-<millis>/`
//! before the new journal is written.
//!
//! 注意：legacy Turn 若缺 parent（旧模型里理论上的"根 Turn"）无法进入
//! 严格交替的新链——迁移 loud 失败，不静默丢数据。

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use ulid::Ulid;

use crate::error::{Error, Result};
use crate::id::NodeId;
use crate::message::{CoreMessage, CoreToolCall};
use crate::node::{Context, ContextData, Data, Input, Meta, Outcome, Step, Turn, TurnData, TurnLine, Usage};
use crate::store::Store;

// ---- legacy serde types (mirror the old format; never written) ----

#[derive(Debug, Deserialize)]
struct LegacyNode {
    id: Ulid,
    #[serde(default)]
    parent: Option<Ulid>,
    #[serde(default)]
    context_refs: Vec<Ulid>,
    #[serde(default)]
    created_by: Option<Ulid>,
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
        created_by: Ulid,
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
    let mut converted: Vec<Meta> = Vec::with_capacity(files.len());
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
        // 3. Rewrite the journal: node_committed lines whose meta id matches
        //    a converted node are replaced with the new header (Input headers
        //    gain the inline `text`); everything else passes through as raw
        //    JSON — 旧行不做 schema 反序列化（旧 meta 形状 ≠ 新 header 形状，
        //    迁移边界只做 Value 级替换，未知字段原样保留）。
        let text = fs::read_to_string(trash.join("journal.jsonl"))?;
        let mut out: Vec<u8> = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(line)?;
            let replaced = if v["event"] == "node_committed" {
                v["meta"]["id"]
                    .as_str()
                    .and_then(|s| s.parse::<Ulid>().ok())
                    .and_then(|id| converted.iter().find(|n| n.id() == id))
                    .map(|node| {
                        serde_json::json!({
                            "event": "node_committed",
                            "meta": node.header_value(),
                        })
                    })
            } else {
                None
            };
            out.extend_from_slice(&serde_json::to_vec(&replaced.unwrap_or(v))?);
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
fn convert(legacy: LegacyNode, store: &Store) -> Result<Meta> {
    let meta = match legacy.kind {
        LegacyNodeKind::Input { text, actor, tools } => {
            Meta::from(Input::node(
                NodeId::from_raw(legacy.id),
                legacy.parent.map(NodeId::from_raw),
                text,
                actor,
                tools,
                legacy.created_by.map(NodeId::from_raw),
            ))
        }
        LegacyNodeKind::Turn {
            steps,
            outcome,
            actor,
            model,
            usage,
            tools,
        } => {
            // 严格交替的新链不接受 parentless Turn：loud 失败，不静默丢数据。
            let parent = legacy.parent.ok_or_else(|| {
                Error::MigrationFailed(format!("legacy turn {} has no parent", legacy.id))
            })?;
            // init 锚点：首个 LlmCall 的 request（该轮系统提示 + 初始历史）。
            let path = TurnData::path(store.root(), legacy.id);
            let init = steps.iter().find_map(|s| match s {
                LegacyStep::LlmCall { request, .. } => Some(request.clone()),
                _ => None,
            });
            if let Some(request) = init {
                crate::node::turn::append_line(&path, &TurnLine::Init { request })?;
            }
            let mut new_steps = Vec::with_capacity(steps.len());
            for step in steps {
                let new_step = match step {
                    LegacyStep::LlmCall {
                        response_text,
                        tool_calls,
                        reasoning,
                        usage,
                        ..
                    } => Step::LlmCall {
                        response_text,
                        tool_calls,
                        reasoning,
                        usage,
                        provider_data: None,
                    },
                    LegacyStep::ToolExec {
                        call_id,
                        name,
                        args,
                        output,
                        duration_ms,
                    } => Step::ToolExec {
                        call_id,
                        name,
                        args,
                        output,
                        duration_ms,
                    },
                };
                crate::node::turn::append_line(&path, &TurnLine::from(new_step.clone()))?;
                new_steps.push(new_step);
            }
            // legacy Turn 信封上的 context_refs / created_by 在新模型里没有
            // 对应字段（Turn 只有相继边），随迁移丢弃。
            Meta::from(Turn::node(
                NodeId::from_raw(legacy.id),
                NodeId::from_raw(parent),
                outcome,
                actor,
                model,
                usage,
                tools,
                &new_steps,
            ))
        }
        LegacyNodeKind::Context {
            body,
            created_by,
            model,
        } => {
            let id: NodeId<Context> = NodeId::from_raw(legacy.id);
            ContextData { body: body.clone() }.save(&ContextData::path(store.root(), id.raw()))?;
            Meta::from(Context::node(
                id,
                legacy.context_refs,
                Some(NodeId::from_raw(created_by)),
                model,
                &body,
            ))
        }
    };
    Ok(meta.with_created_at(legacy.created_at))
}
