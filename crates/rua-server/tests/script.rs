//! `script` 工具（PTC）端到端：BoaScriptHost 绑定面 × 真实图 × mock engine。
//! spawn 走完整原子路径（真实开轮，mock engine 立即提交），wait 轮询图索引。

use std::future::Future;
use std::sync::Arc;

use rua_graph::graph::Graph;
use rua_graph::id::NodeId;
use rua_graph::message::{CoreMessage, CoreToolCall};
use rua_graph::node::{
    Context, ContextData, Input, Meta, Node, Outcome, Step, Turn, TurnLine, Usage,
};
use rua_graph::Ulid;
use rua_engine::TurnParams;
use rua_engine::ScriptHost;
use rua_server::engine::AgentEngine;
use rua_server::script::BoaScriptHost;
use rua_server::state::AppState;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// 立即提交一个空响应轮（与 api.rs 的 MockEngine{park:false} 同形）。
struct MockEngine;

impl AgentEngine for MockEngine {
    fn run_turn<'a>(
        &'a self,
        mut params: TurnParams,
        _events: UnboundedSender<rua_graph::TurnEvent>,
        cancel: CancellationToken,
    ) -> std::pin::Pin<Box<dyn Future<Output = rua_engine::Result<Node<Turn>>> + 'a>> {
        Box::pin(async move {
            let outcome = if cancel.is_cancelled() {
                Outcome::Cancelled
            } else {
                Outcome::Completed
            };
            let step = Step::LlmCall {
                response_text: String::new(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage::default(),
                provider_data: None,
            };
            if let Some(sink) = params.sink.as_mut() {
                sink(TurnLine::Init {
                    request: vec![
                        CoreMessage::System { content: "sys".into() },
                        CoreMessage::User { content: "hi".into() },
                    ],
                })
                .unwrap();
                sink(TurnLine::from(step.clone())).unwrap();
            }
            Ok(Turn::node(
                params.node_id,
                params.parent,
                outcome,
                params.actor,
                params.model,
                Usage::default(),
                params.tools.clone().unwrap_or_default(),
                &[step],
            ))
        })
    }

    fn summarize<'a>(
        &'a self,
        _model_ref: &'a str,
        _material: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = rua_engine::Result<String>> + Send + 'a>> {
        Box::pin(async { Ok("distilled".to_string()) })
    }
}


async fn test_state() -> (tempfile::TempDir, Arc<AppState>, NodeId<Turn>) {
    let dir = tempfile::tempdir().unwrap();
    let graphs_root = dir.path().join("graphs");
    let mut graph = Graph::open(graphs_root.join("default")).unwrap();

    // 主会话：root input + 已提交轮（= 调用方 me）。
    let i0 = Input::node(NodeId::new(), None, "root task", "human", vec![], None);
    graph.commit(i0.clone()).unwrap();
    let t0_id: NodeId<Turn> = graph.data().allocate();
    let steps = vec![Step::LlmCall {
        response_text: "done".into(),
        tool_calls: vec![],
        reasoning: None,
        usage: Usage::default(),
        provider_data: None,
    }];
    {
        let entry = graph.data().entry(t0_id).unwrap();
        for s in &steps {
            entry.append(TurnLine::from(s.clone())).unwrap();
        }
    }
    let t0 = Turn::node(
        t0_id,
        i0.id,
        Outcome::Completed,
        "human",
        "mock-model",
        Usage::default(),
        vec![],
        &steps,
    );
    graph.commit(t0).unwrap();
    let cur = graph.create_cursor("human", vec![]);
    graph.cursor_mut(cur.id).unwrap().move_to(t0_id.raw()).unwrap();

    let state = Arc::new(AppState::new(
        graph,
        Arc::new(MockEngine),
        "mock-model".into(),
        graphs_root,
        "default".into(),
        vec![("mock".to_string(), rua_engine::config::ProviderConfig::default())],
    ));
    (dir, state, t0_id)
}

#[tokio::test]
async fn script_spawn_wait_list_view_end_to_end() {
    let (_dir, state, me) = test_state().await;
    let host = BoaScriptHost::new(state.clone());

    let code = r#"
      const me = graph.me();
      const s = graph.spawn({ content: "child task" });
      const r = graph.wait(s.turn_node_id, 10);
      console.log("wait:", r.status, r.outcome);
      console.log("agents:", graph.list({ actor: "agent:" + me.slice(0, 8), kind: "input" }).length);
      console.log("view:", graph.view(s.input_node_id).text);
      console.log("me:", me);
    "#;
    let out = host.run(
        code,
        me,
        0,
        vec!["bash".to_string(), "script".to_string()],
        CancellationToken::new(),
    );
    assert!(out.contains("wait: committed completed"), "out: {out}");
    assert!(out.contains("agents: 1"), "out: {out}");
    assert!(out.contains("view: child task"), "out: {out}");
    assert!(out.contains(&format!("me: {me}")), "out: {out}");

    // 溯源与工具集继承落在图上（不只看脚本输出）。
    let g = state.graph.lock().await;
    let input = g
        .metas()
        .iter()
        .find_map(|m| match m {
            Meta::Input(n) if n.kind.created_by == Some(me) => Some(n),
            _ => None,
        })
        .expect("spawned input");
    assert_eq!(input.kind.text, "child task");
    assert_eq!(input.kind.tools, vec!["bash".to_string(), "script".to_string()]);
}

#[tokio::test]
async fn script_spawn_with_pointer_forks_and_validates() {
    let (_dir, state, me) = test_state().await;
    let host = BoaScriptHost::new(state.clone());
    let tools = vec!["bash".to_string(), "script".to_string()];

    // (a) 以 me 为 pointer fork：成功，且 wait 可读。
    let code = format!(
        r#"
      const s = graph.spawn({{ pointer: "{me}", content: "forked" }});
      const r = graph.wait(s.turn_node_id, 10);
      console.log("fork:", r.status);
    "#
    );
    let out = host.run(&code, me, 0, tools.clone(), CancellationToken::new());
    assert!(out.contains("fork: committed"), "out: {out}");

    // (b) pointer 落在 input 节点：受检恢复报错，脚本以错误文本终止。
    let input_id: Ulid = {
        let g = state.graph.lock().await;
        g.metas()
            .iter()
            .find_map(|m| match m {
                Meta::Input(n) => Some(n.id.raw()),
                _ => None,
            })
            .unwrap()
    };
    let code = format!(
        r#"
      const s = graph.spawn({{ pointer: "{input_id}", content: "bad" }});
      console.log("unreachable");
    "#
    );
    // 错误以文本形式作为工具输出返回（与 bash 同契约）。
    let out = host.run(&code, me, 0, tools, CancellationToken::new());
    assert!(out.contains("is not a turn node"), "out: {out}");
    assert!(!out.contains("unreachable"), "out: {out}");
}

/// 整节点 view + 导航/grep 惯例：view 回 header + steps/body（无便利字段）；
/// 边全在 header 上，脚本建索引即可回溯链、找分支、按正文 grep。
#[tokio::test]
async fn script_view_whole_node_navigation_and_grep() {
    let (_dir, state, me) = test_state().await;
    let host = BoaScriptHost::new(state.clone());

    // 另造历史：带 tool_exec 的轮（grep 目标，挂在 me 链上）+ 一个
    // 蒸馏出的 context（正文节点）。
    let (t1_id, ctx_id) = {
        let mut g = state.graph.lock().await;
        let i1 = Input::node(NodeId::new(), Some(me), "fix the panic", "human", vec![], None);
        let i1_id = i1.id;
        g.commit(i1).unwrap();
        let t1_id: NodeId<Turn> = g.data().allocate();
        let bash_args = serde_json::json!({"command": "make test"});
        let steps = vec![
            Step::LlmCall {
                response_text: String::new(),
                tool_calls: vec![CoreToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    args: bash_args.clone(),
                }],
                reasoning: None,
                usage: Usage::default(),
                provider_data: None,
            },
            Step::ToolExec {
                call_id: "c1".into(),
                name: "bash".into(),
                args: bash_args,
                output: "test result: FAILED. thread 'main' panicked at lib.rs:7: boom".into(),
                duration_ms: 5,
            },
            Step::LlmCall {
                response_text: "fixed".into(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage::default(),
                provider_data: None,
            },
        ];
        {
            let entry = g.data().entry(t1_id).unwrap();
            for s in &steps {
                entry.append(TurnLine::from(s.clone())).unwrap();
            }
        }
        g.commit(Turn::node(
            t1_id,
            i1_id,
            Outcome::Completed,
            "human",
            "mock-model",
            Usage::default(),
            vec![],
            &steps,
        ))
        .unwrap();

        let ctx_id = NodeId::new();
        let body = "distilled: the boom was in lib.rs";
        g.data()
            .create(ctx_id, ContextData { body: body.into() })
            .unwrap();
        g.commit(Context::node(ctx_id, vec![t1_id.raw()], Some(t1_id), "mock-model", body))
            .unwrap();
        (t1_id, ctx_id)
    };

    // id 经 prologue 注入（正文含大量花括号，不走 format!）。
    let code = format!(
        "const ME = \"{me}\";\nconst T1 = \"{t1}\";\nconst CTX = \"{ctx}\";\n{}",
        r#"
const idx = Object.fromEntries(graph.list().map((r) => [r.id, r]));

// 整节点 view：turn 带 steps，不再有 text/steps_count 便利字段。
const v = graph.view(ME);
console.log("kind:", v.kind, "steps:", v.steps.length, "hasText:", "text" in v, "hasCount:", "steps_count" in v);
console.log("resp:", v.steps.find((s) => s.type === "llm_call").response_text);

// 链回溯：turn.parent -> 发起它的 input；input.parent -> 上一 turn；根为 null。
let chain = [];
let cur = ME;
while (cur) {
  chain.push(cur);
  const input = idx[idx[cur].parent];
  cur = input ? input.parent || null : null;
}
console.log("chain:", chain.length);

// 分支：挂在 me 下的子 input（链上提交的 + spawn 出来的），spawn 溯源看 created_by。
const s = graph.spawn({ pointer: ME, content: "branch task" });
graph.wait(s.turn_node_id, 10);
const kids = graph.list({ kind: "input" }).filter((r) => r.parent === ME);
console.log("kids:", kids.length, "spawnedByMe:", kids.some((k) => k.created_by === ME));

// grep 正文：只对候选拉 steps，命中打印 id 而非整个正文。
const hits = graph.list({ kind: "turn" }).map((r) => r.id)
  .filter((id) => graph.view(id).steps.some((st) => st.type === "tool_exec" && /panic/.test(st.output)));
console.log("hits:", hits.join(","));

// context：整节点含 body，蒸馏溯源边在 header 上。
const c = graph.view(CTX);
console.log("ctx:", c.kind, c.body, c.distilled_from === T1);
"#,
        me = me,
        t1 = t1_id.raw(),
        ctx = ctx_id.raw(),
    );
    let out = host.run(&code, me, 0, vec!["bash".to_string(), "script".to_string()], CancellationToken::new());
    println!("{out}");
    assert!(out.contains("kind: turn steps: 1 hasText: false hasCount: false"), "out: {out}");
    assert!(out.contains("resp: done"), "out: {out}");
    assert!(out.contains("chain: 1"), "out: {out}");
    assert!(out.contains("kids: 2 spawnedByMe: true"), "out: {out}");
    assert!(out.contains(&format!("hits: {}", t1_id.raw())), "out: {out}");
    assert!(
        out.contains("ctx: context distilled: the boom was in lib.rs true"),
        "out: {out}"
    );
}
