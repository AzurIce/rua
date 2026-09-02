//! REST + WS handlers. JSON shapes are the rua-core types verbatim; errors
//! are `{"error": "..."}` with 404 / 409 / 400 as appropriate.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rua_core::cursor::Cursor;
use rua_core::graph::NodeMeta;
use rua_core::id::{CursorId, NodeId};
use rua_core::node::{Node, NodeKind, Step};
use rua_core::Error as CoreError;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use rua_engine::TurnParams;

use crate::events::ServerEvent;
use crate::state::SharedState;
use crate::turn::spawn_turn;

pub fn api_router(state: SharedState) -> Router {
    Router::new()
        .route("/graph", get(get_graph))
        .route("/nodes/{id}", get(get_node))
        .route("/cursors", get(list_cursors).post(create_cursor))
        .route("/inputs", post(post_root_input))
        .route("/cursors/{id}/chain", get(get_chain))
        .route("/cursors/{id}/context_preview", get(get_context_preview))
        .route("/cursors/{id}/input", post(post_input))
        .route("/cursors/{id}/move", post(post_move))
        .route("/cursors/{id}/detach", post(post_detach))
        .route("/cursors/{id}/cancel", post(post_cancel))
        .route("/summarize", post(post_summarize))
        .route("/models", get(list_models))
        .route(
            "/graphs",
            get(list_graphs_handler).post(create_graph_handler),
        )
        .route("/graphs/{name}/activate", post(activate_graph_handler))
        .route("/graphs/{name}/rename", post(rename_graph_handler))
        .route("/graphs/{name}/duplicate", post(duplicate_graph_handler))
        .route("/clone", post(clone_subgraph_handler))
        .route("/graphs/{name}", axum::routing::delete(delete_graph_handler))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

// ---- errors ----

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<CoreError> for ApiError {
    fn from(e: CoreError) -> Self {
        let status = match &e {
            CoreError::NodeNotFound(_) | CoreError::CursorNotFound(_) => StatusCode::NOT_FOUND,
            CoreError::CursorBusy(_)
            | CoreError::CursorIdle(_)
            | CoreError::NodeAlreadyCommitted(_) => StatusCode::CONFLICT,
            CoreError::CursorOnContextNode(_)
            | CoreError::ContextNodeHasParent(_)
            | CoreError::ContextRefNotContextNode(_)
            | CoreError::ContextRefNotCommitted(_)
            | CoreError::ParentNotCommitted(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, e.to_string())
    }
}

fn parse_node_id(raw: &str) -> Result<NodeId, ApiError> {
    raw.parse()
        .map_err(|_| ApiError::bad_request(format!("invalid node id: {raw:?}")))
}

fn parse_cursor_id(raw: &str) -> Result<CursorId, ApiError> {
    raw.parse()
        .map_err(|_| ApiError::bad_request(format!("invalid cursor id: {raw:?}")))
}

// ---- graph / nodes ----

async fn get_graph(State(state): State<SharedState>) -> Json<Value> {
    let graph = state.graph.lock().await;
    Json(json!({
        "nodes": graph.metas(),
        "cursors": graph.cursors.list(),
        "interrupted": graph.interrupted(),
        // Currently running turns, so late-joining/reconnecting clients can
        // render their loading placeholders too.
        "in_flight": graph.cursors.in_flight_all().cloned().collect::<Vec<_>>(),
    }))
}

async fn get_node(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<Node>, ApiError> {
    let id = parse_node_id(&id)?;
    let mut graph = state.graph.lock().await;
    Ok(Json(graph.node(id)?.clone()))
}

// ---- cursors ----

async fn list_cursors(State(state): State<SharedState>) -> Json<Vec<Cursor>> {
    let graph = state.graph.lock().await;
    Json(graph.cursors.list().into_iter().cloned().collect())
}

#[derive(Deserialize)]
struct CreateCursorBody {
    actor: String,
    #[serde(default)]
    capabilities: Vec<String>,
}

async fn create_cursor(
    State(state): State<SharedState>,
    Json(body): Json<CreateCursorBody>,
) -> (StatusCode, Json<Cursor>) {
    let cursor = state
        .graph
        .lock()
        .await
        .create_cursor(body.actor, body.capabilities);
    state.broadcast(ServerEvent::CursorCreated {
        cursor: cursor.clone(),
    });
    (StatusCode::CREATED, Json(cursor))
}

async fn get_chain(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<Node>>, ApiError> {
    let cursor_id = parse_cursor_id(&id)?;
    let mut graph = state.graph.lock().await;
    let tip = graph.cursors.get(cursor_id)?.node;
    let Some(tip) = tip else {
        return Ok(Json(Vec::new()));
    };
    let ids = graph.chain_to_root(tip)?;
    let mut nodes = Vec::with_capacity(ids.len());
    for id in ids {
        nodes.push(graph.node(id)?.clone());
    }
    Ok(Json(nodes))
}

#[derive(Deserialize)]
struct ContextPreviewQuery {
    /// 工具覆盖（逗号分隔的显式列表）；缺省 = 全量。
    tools: Option<String>,
}

/// 下一轮请求的实时装配预览：当前 tip 的链消息 + 按工具覆盖动态组装的
/// 系统提示词与有效工具集。token 数只有真实发送后 provider 才返回，这里
/// 不给估计——UI 用字符数 + 上一轮的 context_tokens 做参照。
async fn get_context_preview(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<ContextPreviewQuery>,
) -> Result<Json<Value>, ApiError> {
    let cursor_id = parse_cursor_id(&id)?;
    let tools_override: Option<Vec<String>> = query.tools.map(|raw| {
        raw.split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    });
    let mut graph = state.graph.lock().await;
    // 无 tip（detached 指针）= 空链；draft 会话 UI 侧不调用本端点。
    let messages = match graph.cursors.get(cursor_id)?.node {
        Some(tip) => graph.assemble_chain(tip)?,
        None => Vec::new(),
    };
    // 预览恒按用户发送的语义计算：depth=0 且 spawner 在位（同 post_input）。
    let effective =
        rua_engine::prompt::EffectiveTools::compute(tools_override.as_deref(), true, 0);
    Ok(Json(json!({
        "system_prompt": rua_engine::prompt::build_system_prompt(&effective),
        "tools": effective.names(),
        "messages": messages,
    })))
}

// ---- turns ----

#[derive(Deserialize)]
struct InputBody {
    text: String,
    /// 本次发送的模型覆盖（None = daemon 默认模型）。
    #[serde(default)]
    model: Option<String>,
    /// 本次发送的工具列表覆盖（None = 全部工具）。
    #[serde(default)]
    tools: Option<Vec<String>>,
}

async fn post_input(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<InputBody>,
) -> Result<Json<Value>, ApiError> {
    let cursor_id = parse_cursor_id(&id)?;

    let mut graph = state.graph.lock().await;
    let cursor = graph.cursors.get(cursor_id)?.clone();
    // Reject early: don't commit the input node when the cursor is busy.
    if graph.cursors.in_flight(cursor_id).is_some() {
        return Err(CoreError::CursorBusy(cursor_id).into());
    }

    // wire 层的 None（UI 全勾）就地展开为显式列表再落图：图数据里 tools
    // 就是 Vec，没有 None。用户发送恒 depth=0 且 spawner 在位。
    let tools = rua_engine::prompt::EffectiveTools::compute(body.tools.as_deref(), true, 0).names();
    let input = Node {
        id: NodeId::new(),
        parent: cursor.node,
        context_refs: vec![],
        created_by: None,
        created_at: Node::now_millis(),
        kind: NodeKind::Input {
            text: body.text,
            actor: cursor.actor.clone(),
            tools: tools.clone(),
        },
    };
    let input_id = input.id;
    graph.commit_node(input)?;
    let input_meta = graph.meta(input_id).expect("just committed").clone();
    state.broadcast(ServerEvent::NodeCommitted {
        meta: input_meta.clone(),
    });
    graph.move_cursor(cursor_id, input_id)?;
    state.broadcast(ServerEvent::CursorMoved {
        cursor_id,
        node: Some(input_id),
    });
    let handle = graph.begin_turn(cursor_id)?;
    let history = graph.assemble_chain(input_id)?;
    let model = body.model.clone().unwrap_or_else(|| state.default_model.clone());
    drop(graph);

    let cancel = CancellationToken::new();
    state.cancels.lock().await.insert(cursor_id, cancel.clone());
    spawn_turn(
        &state,
        TurnParams {
            cursor_id,
            node_id: handle.node_id,
            parent: Some(input_id),
            context_refs: vec![],
            actor: cursor.actor,
            model,
            history,
            system_prompt: None,
            depth: 0,
            tools: Some(tools),
        },
        cancel,
    );

    Ok(Json(json!({
        "input_node": input_meta,
        "turn_node_id": handle.node_id,
    })))
}

#[derive(Deserialize)]
struct RootInputBody {
    text: String,
    /// Attach point for the new session's first input: must be a turn node.
    /// `None` = the input becomes a fresh root (detached conversation tree).
    #[serde(default)]
    parent: Option<NodeId>,
    #[serde(default = "default_actor")]
    actor: String,
    /// 本次发送的模型覆盖（None = daemon 默认模型）。
    #[serde(default)]
    model: Option<String>,
    /// 本次发送的工具列表覆盖（None = 全部工具）。
    #[serde(default)]
    tools: Option<Vec<String>>,
}

fn default_actor() -> String {
    "human".to_string()
}

/// Lazy session creation: the cursor only comes into existence together with
/// its first input. Used by draft sessions in the UI, so clicking "new
/// conversation" never leaves an orphaned empty cursor behind.
async fn post_root_input(
    State(state): State<SharedState>,
    Json(body): Json<RootInputBody>,
) -> Result<Json<Value>, ApiError> {
    let mut graph = state.graph.lock().await;
    if let Some(parent) = body.parent {
        match graph.meta(parent) {
            None => return Err(CoreError::NodeNotFound(parent).into()),
            Some(meta) if meta.kind != rua_core::node::NodeKindTag::Turn => {
                return Err(ApiError::bad_request(
                    "attach point must be a turn node",
                ));
            }
            _ => {}
        }
    }

    let cursor = graph.create_cursor(body.actor, vec![]);
    let actor = cursor.actor.clone();
    state.broadcast(ServerEvent::CursorCreated {
        cursor: cursor.clone(),
    });
    // wire 层的 None（UI 全勾）就地展开为显式列表再落图（同 post_input）。
    let tools = rua_engine::prompt::EffectiveTools::compute(body.tools.as_deref(), true, 0).names();
    let input = Node {
        id: NodeId::new(),
        parent: body.parent,
        context_refs: vec![],
        created_by: None,
        created_at: Node::now_millis(),
        kind: NodeKind::Input {
            text: body.text,
            actor: actor.clone(),
            tools: tools.clone(),
        },
    };
    let input_id = input.id;
    graph.commit_node(input)?;
    let input_meta = graph.meta(input_id).expect("just committed").clone();
    state.broadcast(ServerEvent::NodeCommitted {
        meta: input_meta.clone(),
    });
    graph.move_cursor(cursor.id, input_id)?;
    state.broadcast(ServerEvent::CursorMoved {
        cursor_id: cursor.id,
        node: Some(input_id),
    });
    let handle = graph.begin_turn(cursor.id)?;
    let history = graph.assemble_chain(input_id)?;
    let cursor_id = cursor.id;
    let model = body.model.clone().unwrap_or_else(|| state.default_model.clone());
    drop(graph);

    let cancel = CancellationToken::new();
    state.cancels.lock().await.insert(cursor_id, cancel.clone());
    spawn_turn(
        &state,
        TurnParams {
            cursor_id,
            node_id: handle.node_id,
            parent: Some(input_id),
            context_refs: vec![],
            actor,
            model,
            history,
            system_prompt: None,
            depth: 0,
            tools: Some(tools),
        },
        cancel,
    );

    Ok(Json(json!({
        "cursor": cursor,
        "input_node": input_meta,
        "turn_node_id": handle.node_id,
    })))
}

#[derive(Deserialize)]
struct MoveBody {
    node_id: NodeId,
}

async fn post_move(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<MoveBody>,
) -> Result<Json<Cursor>, ApiError> {
    let cursor_id = parse_cursor_id(&id)?;
    let mut graph = state.graph.lock().await;
    // A running turn owns its cursor: it will move the cursor to the
    // committed turn node, silently clobbering any concurrent attach.
    if graph.cursors.in_flight(cursor_id).is_some() {
        return Err(CoreError::CursorBusy(cursor_id).into());
    }
    // Fork/attach targets are turn nodes only: inputs are edge content, not
    // footholds (re-answering an input would need an explicit retry op), and
    // context nodes are material (rejected inside the core).
    match graph.meta(body.node_id) {
        Some(meta) if meta.kind == rua_core::node::NodeKindTag::Input => {
            return Err(ApiError::bad_request(
                "cannot attach to an input node; attach to a turn node",
            ));
        }
        _ => {}
    }
    graph.move_cursor(cursor_id, body.node_id)?;
    let cursor = graph.cursors.get(cursor_id)?.clone();
    state.broadcast(ServerEvent::CursorMoved {
        cursor_id,
        node: Some(body.node_id),
    });
    Ok(Json(cursor))
}

/// Detach the cursor from the graph: its next input starts a fresh root
/// (a new, disconnected conversation tree).
async fn post_detach(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<Cursor>, ApiError> {
    let cursor_id = parse_cursor_id(&id)?;
    let mut graph = state.graph.lock().await;
    // Same ownership rule as move: a running turn owns its cursor.
    if graph.cursors.in_flight(cursor_id).is_some() {
        return Err(CoreError::CursorBusy(cursor_id).into());
    }
    graph.detach_cursor(cursor_id)?;
    let cursor = graph.cursors.get(cursor_id)?.clone();
    state.broadcast(ServerEvent::CursorMoved {
        cursor_id,
        node: None,
    });
    Ok(Json(cursor))
}

async fn post_cancel(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let cursor_id = parse_cursor_id(&id)?;
    state.graph.lock().await.cursors.get(cursor_id)?;
    let token = state.cancels.lock().await.get(&cursor_id).cloned();
    match token {
        Some(token) => {
            token.cancel();
            Ok(StatusCode::NO_CONTENT)
        }
        None => Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("cursor has no in-flight turn: {cursor_id}"),
        )),
    }
}

// ---- summarize ----

#[derive(Deserialize)]
struct SummarizeBody {
    sources: Vec<NodeId>,
}

/// Project a node to plain text as distillation material.
fn project_node(node: &Node) -> String {
    match &node.kind {
        NodeKind::Input { text, actor, .. } => format!("[input by {actor}]\n{text}"),
        NodeKind::Context { body, .. } => body.clone(),
        NodeKind::Turn { steps, actor, .. } => {
            let mut out = format!("[turn by {actor}]");
            for step in steps {
                match step {
                    Step::LlmCall { response_text, .. } if !response_text.is_empty() => {
                        out.push('\n');
                        out.push_str(response_text);
                    }
                    Step::ToolExec {
                        name,
                        args,
                        output,
                        ..
                    } => {
                        let preview: String = output.chars().take(200).collect();
                        out.push_str(&format!("\n[tool {name}] args={args}\n{preview}"));
                    }
                    _ => {}
                }
            }
            out
        }
    }
}

async fn post_summarize(
    State(state): State<SharedState>,
    Json(body): Json<SummarizeBody>,
) -> Result<Json<NodeMeta>, ApiError> {
    if body.sources.is_empty() {
        return Err(ApiError::bad_request("sources must be non-empty"));
    }

    let material = {
        let mut graph = state.graph.lock().await;
        let mut parts = Vec::with_capacity(body.sources.len());
        for id in &body.sources {
            parts.push(project_node(graph.node(*id)?));
        }
        parts.join("\n\n---\n\n")
    };

    let summary = state
        .engine
        .summarize(&state.default_model, &material)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("summarize failed: {e}")))?;

    let node = Node {
        id: NodeId::new(),
        parent: None,
        context_refs: body.sources.clone(),
        created_by: None,
        created_at: Node::now_millis(),
        kind: NodeKind::Context {
            body: summary,
            created_by: body.sources[0],
            model: state.default_model.clone(),
        },
    };
    let node_id = node.id;
    let mut graph = state.graph.lock().await;
    graph.commit_node(node)?;
    let meta = graph.meta(node_id).expect("just committed").clone();
    drop(graph);
    state.broadcast(ServerEvent::NodeCommitted { meta: meta.clone() });
    Ok(Json(meta))
}

// ---- models ----

/// Aggregate every registered provider's model list (OpenAI-compatible
/// `GET {base_url}/models` per provider, fetched in parallel). Each provider
/// falls back to its configured default model when unreachable.
///
/// 每个 provider 独立 2s 超时 + 60s 缓存：provider 不在时（比如 oMLX 没开）
/// 代理请求会挂到连接超时，UI 每次刷新都调这个端点，不能每次都卡几秒。
/// 返回的条目带 model ref（默认 provider 用裸模型名，具名 provider 用
/// `"provider/model"`），UI 直接把它作为发送时的模型覆盖值。
async fn list_models(State(state): State<SharedState>) -> Json<Value> {
    const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .unwrap_or_default();
    let fetches: Vec<_> = state
        .providers
        .iter()
        .map(|(name, cfg)| {
            let client = client.clone();
            let state = state.clone();
            let name = name.clone();
            let cfg = cfg.clone();
            async move { (name.clone(), fetch_provider_models(&state, &client, &name, &cfg).await) }
        })
        .collect();
    let mut models = Vec::new();
    for (name, list) in futures::future::join_all(fetches).await {
        for m in list {
            let id = if name == rua_core::config::DEFAULT_PROVIDER {
                m.clone()
            } else {
                format!("{name}/{m}")
            };
            models.push(json!({ "id": id, "provider": name, "model": m }));
        }
    }
    Json(json!({ "models": models, "default": state.default_model }))
}

/// 单个 provider 的模型列表：60s 缓存 → 2s 超时拉取 → 失败回退陈旧缓存
/// → 兜底为配置的默认模型。`cfg.model` 保证在列表里。
async fn fetch_provider_models(
    state: &SharedState,
    client: &reqwest::Client,
    name: &str,
    cfg: &rua_core::config::ProviderConfig,
) -> Vec<String> {
    const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

    let cached = state.models_cache.lock().await.get(name).cloned();
    if let Some((at, models)) = &cached
        && at.elapsed() < CACHE_TTL
    {
        return models.clone();
    }

    let url = format!("{}/models", cfg.base_url.trim_end_matches('/'));
    let resp = client.get(&url).bearer_auth(&cfg.api_key).send().await;
    let fetched: Option<Vec<String>> = match resp {
        Ok(r) => r.json::<Value>().await.ok().and_then(|v| {
            v["data"].as_array().map(|arr| {
                arr.iter()
                    .filter_map(|m| m["id"].as_str().map(str::to_string))
                    .collect::<Vec<String>>()
            })
        }),
        Err(_) => None,
    };
    let mut models = fetched
        .or_else(|| cached.map(|(_, m)| m))
        .unwrap_or_default();
    if !models.contains(&cfg.model) {
        models.insert(0, cfg.model.clone());
    }
    state
        .models_cache
        .lock()
        .await
        .insert(name.to_string(), (std::time::Instant::now(), models.clone()));
    models
}

// ---- graphs (user-side management) ----

impl From<crate::graphs::GraphOpError> for ApiError {
    fn from(e: crate::graphs::GraphOpError) -> Self {
        let status = match &e {
            crate::graphs::GraphOpError::InvalidName(_) => StatusCode::BAD_REQUEST,
            crate::graphs::GraphOpError::NotFound(_) => StatusCode::NOT_FOUND,
            crate::graphs::GraphOpError::AlreadyExists(_) | crate::graphs::GraphOpError::Busy => {
                StatusCode::CONFLICT
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, e.to_string())
    }
}

async fn list_graphs_handler(State(state): State<SharedState>) -> Result<Json<Value>, ApiError> {
    let graphs = crate::graphs::list_graphs(&state.graphs_root)?;
    let current = state.current_graph.lock().await.clone();
    Ok(Json(json!({ "graphs": graphs, "current": current })))
}

#[derive(Deserialize)]
struct GraphNameBody {
    name: String,
}

/// Create a new (empty) graph and switch to it.
async fn create_graph_handler(
    State(state): State<SharedState>,
    Json(body): Json<GraphNameBody>,
) -> Result<StatusCode, ApiError> {
    crate::graphs::create_graph(&state, &body.name).await?;
    Ok(StatusCode::CREATED)
}

async fn activate_graph_handler(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    crate::graphs::activate_graph(&state, &name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn rename_graph_handler(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(body): Json<GraphNameBody>,
) -> Result<StatusCode, ApiError> {
    crate::graphs::rename_graph(&state, &name, &body.name).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Duplicate a graph (deep copy); does not switch the active graph.
async fn duplicate_graph_handler(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(body): Json<GraphNameBody>,
) -> Result<StatusCode, ApiError> {
    crate::graphs::duplicate_graph(&state, &name, &body.name)?;
    Ok(StatusCode::CREATED)
}

/// Delete = move into `.rua/graphs/.trash/` (recoverable by hand).
async fn delete_graph_handler(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    crate::graphs::delete_graph(&state, &name).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct CloneBody {
    /// 源图名（从哪张图复制）。
    from_graph: String,
    /// 框选选中的节点 id。
    nodes: Vec<NodeId>,
}

/// 跨图克隆子树：把 from_graph 里选中的节点（含后继子树与引用的材料）
/// 以新 id 复制进当前活跃图。返回克隆的节点数。
async fn clone_subgraph_handler(
    State(state): State<SharedState>,
    Json(body): Json<CloneBody>,
) -> Result<Json<Value>, ApiError> {
    let n = crate::graphs::clone_subgraph(&state, &body.from_graph, body.nodes).await?;
    Ok(Json(json!({ "cloned": n })))
}

// ---- websocket ----

async fn ws_handler(State(state): State<SharedState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    let rx = state.events.subscribe();
    ws.on_upgrade(move |socket| ws_loop(socket, rx))
}

async fn ws_loop(mut socket: WebSocket, mut rx: broadcast::Receiver<String>) {
    loop {
        tokio::select! {
            biased;
            // Drain incoming frames only to notice the client going away.
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            event = rx.recv() => match event {
                Ok(json) => {
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                // Slow client: skip missed events, stay connected.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
}
