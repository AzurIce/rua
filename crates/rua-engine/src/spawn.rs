//! The graph-growing tools: `spawn_turn` and `inspect`.
//!
//! The agent can fork the conversation graph itself: `spawn_turn` atomically
//! creates a fresh cursor + input node (child of `pointer`, or a new root)
//! and starts its turn — returning immediately so the agent can fan out
//! parallel branches. `inspect` waits for a turn node to commit and returns
//! its result. Together they cover "wait for one result", "collect many",
//! and "run in the background while I keep working".
//!
//! The engine has no view of the graph/runtime: both tools go through the
//! [`TurnSpawner`] trait, implemented by rua-server and injected into the
//! [`crate::Engine`].

use std::sync::Arc;
use std::time::Duration;

use rig_core::completion::ToolDefinition;
use rua_graph::Ulid;
use rua_graph::id::NodeId;
use rua_graph::node::{Turn, Usage};
use tokio_util::sync::CancellationToken;

/// 递归上限：子 turn 也带这套工具，深度到顶后不再注册 spawn/inspect。
pub const MAX_SPAWN_DEPTH: usize = 4;

/// Result of a successful `spawn_turn` call.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SpawnedTurn {
    pub cursor_id: String,
    pub input_node_id: String,
    pub turn_node_id: String,
}

/// What `inspect` reports for a node.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InspectOutcome {
    /// The node is committed; carries its content projection.
    Committed {
        outcome: Option<String>,
        text: String,
        usage: Option<Usage>,
    },
    /// Still running (or unknown) when the wait elapsed.
    Running,
}

/// The runtime services the agent's graph-growing tools need.
pub trait TurnSpawner: Send + Sync {
    /// Atomically create a cursor + input node + started turn.
    /// `parent`: Some = must be a committed turn node; None = fresh root.
    /// `created_by`: the calling turn's node id, stamped on the spawned root
    /// input as its provenance. Returns immediately (async fan-out); the
    /// turn's node id is pre-allocated so the caller can `inspect` it later.
    /// `tools`: 子代的工具覆盖（engine 已解析好的显式列表）：默认 = 父
    /// turn 的有效集（沿 spawn 边继承），显式指定 = 校验后的子集（能力沿
    /// spawn 边单调衰减，永不放大）。
    fn spawn_turn(
        &self,
        parent: Option<Ulid>,
        text: String,
        actor: String,
        created_by: NodeId<Turn>,
        depth: usize,
        tools: Vec<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<SpawnedTurn, String>> + Send>>;

    /// Wait (up to `wait`; None = indefinitely) for the node to be
    /// committed, then return its content. Cancellation of the *parent*
    /// turn is handled engine-side and never touches the child.
    fn inspect(
        &self,
        node: Ulid,
        wait: Option<Duration>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<InspectOutcome, String>> + Send>>;
}

pub fn spawn_turn_definition(depth: usize) -> ToolDefinition {
    ToolDefinition {
        name: "spawn_turn".to_string(),
        description: format!(
            "Spawn a new agent turn on the conversation graph: creates a fresh session whose \
             first message is `content`, attached under `pointer` (a committed turn node) or as \
             a new root when `pointer` is omitted. Returns immediately with the new turn's node \
             id — the turn runs in the background; use `inspect` on that id to wait for and read \
             its result. Spawn multiple turns before inspecting to run branches in parallel. \
             The spawned turn has the same tools as you (spawn depth {} of {MAX_SPAWN_DEPTH}).",
            depth + 1
        ),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "pointer": {
                    "type": ["string", "null"],
                    "description": "Node id of a committed turn to fork from; omit/null for a fresh root"
                },
                "content": {
                    "type": "string",
                    "description": "The first message of the spawned session (its task)"
                },
                "tools": {
                    "type": ["array", "null"],
                    "items": {"type": "string"},
                    "description": "Subset of your current tools for the spawned session; omit = inherit your full current set"
                }
            },
            "required": ["content"]
        }),
    }
}

pub fn inspect_definition() -> ToolDefinition {
    ToolDefinition {
        name: "inspect".to_string(),
        description: "Read the result of a graph node (usually a turn spawned via spawn_turn). \
            Blocks until the node commits — every turn ends in a commit, so this always \
            terminates — or until `wait_secs` elapses, in which case status is \"running\". \
            Returns the outcome, final response text and token usage."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "pointer": {
                    "type": "string",
                    "description": "Node id to inspect"
                },
                "wait_secs": {
                    "type": "number",
                    "description": "Max seconds to wait for the turn to commit (default: wait indefinitely)"
                }
            },
            "required": ["pointer"]
        }),
    }
}

/// Execute `spawn_turn`. Output is a JSON [`SpawnedTurn`] or an error string.
/// `parent_tools` 是父 turn 的有效工具集：子代默认继承它；`args.tools`
/// 显式指定时必须是它的子集（含空集），否则报错让模型修正重试。
pub async fn execute_spawn(
    spawner: &Arc<dyn TurnSpawner>,
    args: &serde_json::Value,
    parent_turn: NodeId<Turn>,
    depth: usize,
    parent_tools: &crate::prompt::EffectiveTools,
) -> String {
    #[derive(serde::Deserialize)]
    struct Args {
        pointer: Option<String>,
        content: String,
        tools: Option<Vec<String>>,
    }
    let args: Args = match serde_json::from_value(args.clone()) {
        Ok(a) => a,
        Err(e) => return format!("error: invalid spawn_turn arguments: {e}"),
    };
    if args.content.trim().is_empty() {
        return "error: content must be non-empty".to_string();
    }
    // 工具集解析：省略 = 继承父有效集；显式 = 严格校验后的子集（空数组合法）。
    let tools = match args.tools {
        None => parent_tools.names(),
        Some(list) => {
            let bad: Vec<&String> = list.iter().filter(|t| !parent_tools.allowed(t)).collect();
            if !bad.is_empty() {
                return format!(
                    "error: invalid tools: {} (your current tools: {})",
                    bad.iter()
                        .map(|t| t.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    parent_tools.names().join(", ")
                );
            }
            list
        }
    };
    let parent = match args.pointer.as_deref().map(str::parse::<Ulid>).transpose() {
        Ok(p) => p,
        Err(_) => return format!("error: invalid node id: {:?}", args.pointer),
    };
    // 溯源：actor 记为 agent:<父turn短id>，图上一眼看出是谁生的。
    let parent_id = parent_turn.to_string();
    let actor = format!("agent:{}", &parent_id[..8.min(parent_id.len())]);
    match spawner
        .spawn_turn(parent, args.content, actor, parent_turn, depth + 1, tools)
        .await
    {
        Ok(spawned) => serde_json::to_string_pretty(&spawned).unwrap_or_default(),
        Err(e) => format!("error: spawn_turn failed: {e}"),
    }
}

/// Execute `inspect`. Blocks (up to `wait_secs`) for the node to commit;
/// parent-turn cancellation only aborts the wait, never the child turn.
pub async fn execute_inspect(
    spawner: &Arc<dyn TurnSpawner>,
    args: &serde_json::Value,
    cancel: &CancellationToken,
) -> String {
    #[derive(serde::Deserialize)]
    struct Args {
        pointer: String,
        wait_secs: Option<u64>,
    }
    let args: Args = match serde_json::from_value(args.clone()) {
        Ok(a) => a,
        Err(e) => return format!("error: invalid inspect arguments: {e}"),
    };
    let node: Ulid = match args.pointer.parse() {
        Ok(id) => id,
        Err(_) => return format!("error: invalid node id: {:?}", args.pointer),
    };
    let wait = args.wait_secs.filter(|s| *s > 0).map(Duration::from_secs);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => "error: inspect aborted (parent turn cancelled)".to_string(),
        result = spawner.inspect(node, wait) => match result {
            Ok(outcome) => serde_json::to_string_pretty(&outcome).unwrap_or_default(),
            Err(e) => format!("error: inspect failed: {e}"),
        },
    }
}
