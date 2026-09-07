//! The `script` tool's host: a boa (pure-Rust JS) interpreter whose `graph`
//! bindings talk to the live graph and runtime. PTC 风格——模型写 JS，解析/
//! 过滤/聚合发生在脚本里，只有 `console.log` 输出进上下文。
//!
//! 执行模型：每次调用在一个专用 OS 线程上跑解释器（CPU 循环由指令预算兜
//! 底）；绑定是同步函数，内部 `block_on` 进程级共享 runtime（`runtime.rs`）
//! 驱动异步图操作（graph 锁、spawn 的完整开轮路径）。外层 turn 线程在
//! channel 上等结果——与 bash 一样占用轮线程，顺序语义一致。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boa_engine::context::ContextBuilder;
use boa_engine::native_function::NativeFunction;
use boa_engine::{Context, JsNativeError, JsResult, JsString, JsValue, Source};
use rua_engine::script::ScriptHost;
use rua_graph::node::{Meta, Step, Turn};
use rua_graph::{NodeId, Ulid};
use tokio_util::sync::CancellationToken;

use crate::spawn::ServerSpawner;
use crate::state::SharedState;

/// 指令预算：CPU 工作量上限（绑定内阻塞等待不消耗）。死循环脚本的兜底。
const INSTRUCTION_BUDGET: usize = 100_000_000;

/// wait 绑定的轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub struct BoaScriptHost {
    state: SharedState,
}

impl BoaScriptHost {
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }
}

impl ScriptHost for BoaScriptHost {
    fn run(
        &self,
        code: &str,
        me: NodeId<Turn>,
        depth: usize,
        tools: Vec<String>,
        cancel: CancellationToken,
    ) -> String {
        if code.trim().is_empty() {
            return "error: script: code is required".to_string();
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let code = code.to_string();
        let state = self.state.clone();
        std::thread::spawn(move || {
            let result = evaluate(&state, &code, me, depth, tools, cancel);
            let _ = tx.send(result);
        });
        match rx.recv() {
            Ok((out, Ok(()))) if out.is_empty() => "(no output)".to_string(),
            Ok((out, Ok(()))) => out,
            Ok((_, Err(e))) => e,
            Err(_) => "error: script worker died".to_string(),
        }
    }
}

// ---- 绑定面（list/view/wait/spawn/me） ----

#[derive(Clone)]
struct Bindings {
    state: SharedState,
    me: NodeId<Turn>,
    depth: usize,
    tools: Vec<String>,
    cancel: CancellationToken,
    out: Arc<Mutex<String>>,
}

impl Bindings {
    /// meta 索引上的过滤：kind/actor/outcome/limit，created_at 升序。
    /// （kind/actor/outcome 是精确匹配；这是承诺面，改动要过版本。）
    fn list(&self, filter: serde_json::Value) -> Result<Vec<serde_json::Value>, String> {
        crate::runtime::shared_runtime().block_on(async {
            let g = self.state.graph.lock().await;
            let kind = filter.get("kind").and_then(|v| v.as_str());
            let actor = filter.get("actor").and_then(|v| v.as_str());
            let outcome = filter.get("outcome").and_then(|v| v.as_str());
            let limit = filter.get("limit").and_then(|v| v.as_u64());
            let mut rows = Vec::new();
            for m in g.metas() {
                let row = m.header_value();
                if let Some(kind) = kind {
                    if row.get("kind").and_then(|v| v.as_str()) != Some(kind) {
                        continue;
                    }
                }
                if let Some(actor) = actor {
                    if row.get("actor").and_then(|v| v.as_str()) != Some(actor) {
                        continue;
                    }
                }
                if let Some(outcome) = outcome {
                    if row.get("outcome").and_then(|v| v.as_str()) != Some(outcome) {
                        continue;
                    }
                }
                rows.push(row);
                if limit.is_some_and(|l| rows.len() as u64 >= l) {
                    break;
                }
            }
            Ok(rows)
        })
    }

    /// 整节点投影：header（信封 + kind tag + meta 平铺，全部边字段都在）
    /// + 正文平铺（turn = `steps`，context = `body`；input 的 `text` 本就
    /// 在 meta 上）。与详情端点同一数据面来源，但不做 request 重放（init
    /// 锚点不外露——脚本能沿链重放，不必逐字复制一份）。
    fn view(&self, id: &str) -> Result<serde_json::Value, String> {
        let ulid = Ulid::from_string(id).map_err(|e| format!("invalid node id: {e}"))?;
        crate::runtime::shared_runtime().block_on(async {
            let g = self.state.graph.lock().await;
            let m = g
                .meta(ulid)
                .ok_or_else(|| format!("node not found: {id}"))?;
            let mut row = m.header_value();
            let body: Option<serde_json::Value> = match m {
                Meta::Turn(t) => g
                    .data()
                    .entry(t.id)
                    .and_then(|e| e.cloned())
                    .ok()
                    .map(|d| serde_json::to_value(&d).expect("turn data serialization")),
                Meta::Context(c) => g
                    .data()
                    .entry(c.id)
                    .and_then(|e| e.cloned())
                    .ok()
                    .map(|d| serde_json::to_value(&d).expect("context data serialization")),
                Meta::Input(_) => None,
            };
            if let Some(serde_json::Value::Object(o)) = body {
                row.as_object_mut()
                    .expect("header serializes to an object")
                    .extend(o);
            }
            Ok(row)
        })
    }

    /// 同步阻塞等 commit。`timeout_secs` None = 一直等到（cancel 可中断，
    /// 与被移除的 inspect 工具同语义）。
    fn wait(&self, id: &str, timeout_secs: Option<u64>) -> Result<serde_json::Value, String> {
        let ulid = Ulid::from_string(id).map_err(|e| format!("invalid node id: {e}"))?;
        let deadline = timeout_secs.map(|s| Instant::now() + Duration::from_secs(s));
        loop {
            if self.cancel.is_cancelled() {
                return Err("script aborted (turn cancelled)".to_string());
            }
            {
                let g = crate::runtime::shared_runtime().block_on(async {
                    let g = self.state.graph.lock().await;
                    match g.meta(ulid) {
                        None => None,
                        Some(m) => project(m, g.data()).ok(),
                    }
                });
                if let Some(v) = g {
                    return Ok(v);
                }
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(serde_json::json!({ "status": "running" }));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// fork 一个会话：走 `ServerSpawner::spawn_turn` 的完整原子路径（cursor +
    /// input + 真实开轮，事件进 WS），返回 {cursor_id, input_node_id,
    /// turn_node_id}。子代继承调用方有效工具集；provenance（created_by）由
    /// 绑定面盖章，模型无需（也无法）自己传入。
    fn spawn(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        if self.cancel.is_cancelled() {
            return Err("script aborted (turn cancelled)".to_string());
        }
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or("spawn: content is required")?
            .to_string();
        let parent = match args.get("pointer").and_then(|v| v.as_str()) {
            Some(p) => {
                let ulid = Ulid::from_string(p).map_err(|e| format!("invalid pointer: {e}"))?;
                let typed = crate::runtime::shared_runtime()
                    .block_on(async { self.state.graph.lock().await.expect_turn(ulid) })
                    .map_err(|e| e.to_string())?;
                Some(typed.raw())
            }
            None => None,
        };
        let me = self.me.raw().to_string();
        let actor = format!("agent:{}", &me[..8.min(me.len())]);
        let spawned = crate::runtime::shared_runtime().block_on(self.spawner().spawn_turn(
            parent,
            content,
            actor,
            self.me,
            self.depth + 1,
            self.tools.clone(),
        ))?;
        Ok(serde_json::json!({
            "cursor_id": spawned.cursor_id,
            "input_node_id": spawned.input_node_id,
            "turn_node_id": spawned.turn_node_id,
        }))
    }
}

/// Project a committed node to the wait/view payload（正文走数据面）。
/// Turn = outcome + final response text + usage; Input/Context 回正文原文。
fn project(meta: &Meta, data: &rua_graph::DataStore) -> rua_graph::Result<serde_json::Value> {
    let mut row = meta.header_value();
    match meta {
        Meta::Turn(n) => {
            let data = data.entry(n.id).and_then(|e| e.cloned())?;
            let text = last_text(&data.steps);
            row["status"] = "committed".into();
            row["outcome"] = format!("{:?}", n.kind.outcome).to_lowercase().into();
            row["text"] = text.into();
            row["usage"] = serde_json::to_value(n.kind.usage)?;
        }
        Meta::Input(n) => {
            row["status"] = "committed".into();
            row["text"] = n.kind.text.clone().into();
        }
        Meta::Context(n) => {
            row["status"] = "committed".into();
            let data = data.entry(n.id).and_then(|e| e.cloned())?;
            row["text"] = data.body.into();
        }
    }
    Ok(row)
}

fn last_text(steps: &[Step]) -> String {
    steps
        .iter()
        .rev()
        .find_map(|s| match s {
            Step::LlmCall {
                response_text, ..
            } if !response_text.is_empty() => Some(response_text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

impl Bindings {
    fn spawner(&self) -> ServerSpawner {
        ServerSpawner::new(self.state.clone())
    }
}

// ---- boa runner ----

const PRELUDE: &str = r#"
const graph = {
  me: () => __me(),
  list: (f) => __list(f === undefined ? {} : f),
  view: (id) => __view(id),
  wait: (id, t) => __wait(id, t === undefined ? null : t),
  spawn: (a) => __spawn(a === undefined ? {} : a),
};
const console = {
  log: (...xs) => __log(xs),
};
"#;

fn arg_json(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<Option<serde_json::Value>> {
    match args.get(i) {
        Some(v) if !v.is_undefined() && !v.is_null() => v.to_json(ctx),
        _ => Ok(None),
    }
}

fn json_arg_or_null(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<serde_json::Value> {
    Ok(arg_json(args, i, ctx)?.unwrap_or(serde_json::Value::Null))
}

fn native_me(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let _ = args;
    let b: Bindings = ctx.get_data::<Bindings>().expect("bindings").clone();
    JsValue::from_json(&serde_json::Value::String(b.me.raw().to_string()), ctx)
}

fn native_list(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let filter = json_arg_or_null(args, 0, ctx)?;
    let b: Bindings = ctx.get_data::<Bindings>().expect("bindings").clone();
    match b.list(filter) {
        Ok(rows) => JsValue::from_json(&serde_json::Value::Array(rows), ctx),
        Err(e) => Err(JsNativeError::error().with_message(e).into()),
    }
}

fn native_view(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = json_arg_or_null(args, 0, ctx)?;
    let b: Bindings = ctx.get_data::<Bindings>().expect("bindings").clone();
    let res = match id.as_str() {
        Some(s) => b.view(s),
        None => Err("view: id must be a string".to_string()),
    };
    match res {
        Ok(v) => JsValue::from_json(&v, ctx),
        Err(e) => Err(JsNativeError::error().with_message(e).into()),
    }
}

fn native_wait(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = json_arg_or_null(args, 0, ctx)?;
    let timeout = json_arg_or_null(args, 1, ctx)?.as_u64();
    let b: Bindings = ctx.get_data::<Bindings>().expect("bindings").clone();
    let res = match id.as_str() {
        Some(s) => b.wait(s, timeout),
        None => Err("wait: id must be a string".to_string()),
    };
    match res {
        Ok(v) => JsValue::from_json(&v, ctx),
        Err(e) => Err(JsNativeError::error().with_message(e).into()),
    }
}

fn native_spawn(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let a = json_arg_or_null(args, 0, ctx)?;
    let b: Bindings = ctx.get_data::<Bindings>().expect("bindings").clone();
    match b.spawn(a) {
        Ok(v) => JsValue::from_json(&v, ctx),
        Err(e) => Err(JsNativeError::error().with_message(e).into()),
    }
}

fn native_log(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let xs = arg_json(args, 0, ctx)?.unwrap_or(serde_json::Value::Null);
    let parts: Vec<String> = match xs {
        serde_json::Value::Array(xs) => xs
            .into_iter()
            .map(|x| match x {
                serde_json::Value::String(s) => s,
                other => serde_json::to_string(&other).unwrap_or_default(),
            })
            .collect(),
        other => vec![serde_json::to_string(&other).unwrap_or_default()],
    };
    let b: Bindings = ctx.get_data::<Bindings>().expect("bindings").clone();
    let mut out = b.out.lock().unwrap();
    out.push_str(&parts.join(" "));
    out.push('\n');
    Ok(JsValue::undefined())
}

/// 跑一个脚本：返回 (console 输出, Err = 引擎错误文本，含 JS 栈与行列号)。
fn evaluate(
    state: &SharedState,
    code: &str,
    me: NodeId<Turn>,
    depth: usize,
    tools: Vec<String>,
    cancel: CancellationToken,
) -> (String, Result<(), String>) {
    let out = Arc::new(Mutex::new(String::new()));
    let mut ctx = ContextBuilder::new()
        .instructions_remaining(INSTRUCTION_BUDGET)
        .build()
        .expect("boa context");
    ctx.insert_data(Bindings {
        state: state.clone(),
        me,
        depth,
        tools,
        cancel,
        out: out.clone(),
    });
    let natives: [(&str, NativeFunction); 6] = [
        ("__me", NativeFunction::from_fn_ptr(native_me)),
        ("__list", NativeFunction::from_fn_ptr(native_list)),
        ("__view", NativeFunction::from_fn_ptr(native_view)),
        ("__wait", NativeFunction::from_fn_ptr(native_wait)),
        ("__spawn", NativeFunction::from_fn_ptr(native_spawn)),
        ("__log", NativeFunction::from_fn_ptr(native_log)),
    ];
    for (name, f) in natives {
        ctx.register_global_callable(JsString::from(name), 0, f)
            .expect("register native");
    }
    ctx.eval(Source::from_bytes(PRELUDE)).expect("prelude eval");
    let result = ctx.eval(Source::from_bytes(code));
    let text = out.lock().unwrap().clone();
    (text, result.map(|_| ()).map_err(|e| format!("{e}")))
}
