//! The `script` tool: programmatic access to the conversation graph (PTC).
//!
//! 模型写一段 JS，经 `graph` 绑定面对图做查询与生长（`list`/`view`/`wait`/
//! `spawn` + `me`），只有 `console.log` 输出进上下文——解析、过滤、聚合发生
//! 在脚本里。engine 自身不解释 JS：[`ScriptHost`] 由 server 注入（那边用
//! boa 承载），沿用原 `TurnSpawner` 的注入模式；graph-free 纪律不破。

use rig_core::completion::ToolDefinition;
use rua_graph::id::NodeId;
use rua_graph::node::Turn;
use tokio_util::sync::CancellationToken;

/// 递归上限：spawn 出的子轮 depth+1，到顶后 script 工具不再注册（子轮因此
/// 无法再生长图——能力沿 spawn 边单调衰减）。
pub const MAX_SPAWN_DEPTH: usize = 4;

/// 脚本执行宿主：server 侧用 boa 实现。同步阻塞——engine 的轮本就跑在专用
/// 线程上，脚本期间该线程被占用与 bash 一致；`cancel` 由绑定面在可等待处
/// 检查（CPU 循环由解释器预算兜底）。返回 console 输出文本（含错误文本，
/// 错误不抛出）。
pub trait ScriptHost: Send + Sync {
    fn run(
        &self,
        code: &str,
        me: NodeId<Turn>,
        depth: usize,
        // 调用方 turn 的有效工具集（规范序）：spawn 的子代默认继承它。
        tools: Vec<String>,
        cancel: CancellationToken,
    ) -> String;
}

pub fn script_definition() -> ToolDefinition {
    ToolDefinition {
        name: "script".to_string(),
        // description 以 supervisor 工作流开头 + 权威示范脚本：工具 description
        // 是选择时刻注意力最集中的位置，模型只模仿见过的形状，不从 API 参考
        // 发明用法。绑定清单随后，末尾给出错自修复承诺（消除盲写顾虑）。
        description: "Act on the conversation graph by writing ONE JavaScript program. The \
             program runs to completion in a single tool call and only `console.log` output \
             enters your context — this is how you supervise multi-part work: one round-trip \
             instead of dozens.\n\
             For a task with independent branches, the default is ONE script that spawns one \
             session per branch, waits for each, and prints the distilled results:\n\
             ```js\n\
             const branches = [\"v0.1 architecture\", \"v0.2 architecture\", \"v0.3 architecture\"];\n\
             const ids = branches.map((b) => graph.spawn({ content: `Investigate ${b} of the \
             repo at <path>. Read the relevant sources and end with a <=200-word distilled \
             summary.` }).turn_node_id);\n\
             for (const id of ids) {\n\
               const r = graph.wait(id, 600);\n\
               console.log(id, r.outcome, r.text);\n\
             }\n\
             ```\n\
             Bindings on the `graph` object:\n\
             - `graph.me()` -> your turn's node id\n\
             - `graph.list({kind, actor, outcome, limit})` -> node headers (kind: \
             \"input\"|\"turn\"|\"context\"; ordered by creation)\n\
             - `graph.view(id)` -> one node's header (+ text/steps_count for turns)\n\
             - `graph.spawn({pointer?, content})` -> fork a session (under a committed turn, \
             or a fresh root); returns {cursor_id, input_node_id, turn_node_id}; runs in the \
             background; spawned sessions inherit your tool set\n\
             - `graph.wait(id, timeout_secs?)` -> blocks until the node commits, returns \
             {status, outcome?, text?}; status is \"running\" on timeout\n\
             Only `console.log` output is returned (tail-truncated); compute is capped by an \
             instruction budget. If the script throws, the JS error (with line numbers) comes \
             back as text — fix and rerun."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "The JavaScript program to run"
                }
            },
            "required": ["code"]
        }),
    }
}
