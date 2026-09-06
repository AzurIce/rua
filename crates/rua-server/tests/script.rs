//! `script` 工具（PTC）端到端：BoaScriptHost 绑定面 × 真实图 × mock engine。
//! spawn 走完整原子路径（真实开轮，mock engine 立即提交），wait 轮询图索引。

use std::future::Future;
use std::sync::Arc;

use rua_graph::graph::Graph;
use rua_graph::id::NodeId;
use rua_graph::message::CoreMessage;
use rua_graph::node::{Input, Meta, Node, Outcome, Step, Turn, TurnLine, Usage};
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
