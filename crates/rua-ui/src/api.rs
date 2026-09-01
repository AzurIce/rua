//! REST client for the rua-server contract (base `http://127.0.0.1:3080`).

use gloo_net::http::{Request, Response};
use serde::Serialize;

use crate::types::*;

pub const API_BASE: &str = "http://127.0.0.1:3080";
pub const WS_URL: &str = "ws://127.0.0.1:3080/api/ws";

async fn unwrap<T: serde::de::DeserializeOwned>(resp: Response) -> Result<T, String> {
    if resp.ok() {
        resp.json::<T>()
            .await
            .map_err(|e| format!("解析响应失败: {e}"))
    } else {
        Err(status_error(resp).await)
    }
}

async fn status_error(resp: Response) -> String {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let body = body.trim();
    if body.is_empty() {
        format!("请求失败 (HTTP {status})")
    } else {
        format!("请求失败 (HTTP {status}): {body}")
    }
}

pub async fn get_graph() -> Result<GraphResponse, String> {
    let resp = Request::get(&format!("{API_BASE}/api/graph"))
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    unwrap(resp).await
}

pub async fn get_cursors() -> Result<Vec<Cursor>, String> {
    let resp = Request::get(&format!("{API_BASE}/api/cursors"))
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    unwrap(resp).await
}

pub async fn get_chain(cursor_id: &str) -> Result<Vec<Node>, String> {
    let resp = Request::get(&format!("{API_BASE}/api/cursors/{cursor_id}/chain"))
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    unwrap(resp).await
}

pub async fn get_node(id: &str) -> Result<Node, String> {
    let resp = Request::get(&format!("{API_BASE}/api/nodes/{id}"))
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    unwrap(resp).await
}

pub async fn send_input(
    cursor_id: &str,
    text: &str,
    model: Option<&str>,
    tools: Option<&[String]>,
) -> Result<InputResponse, String> {
    #[derive(Serialize)]
    struct InputBody<'a> {
        text: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tools: Option<&'a [String]>,
    }
    let resp = Request::post(&format!("{API_BASE}/api/cursors/{cursor_id}/input"))
        .json(&InputBody { text, model, tools })
        .map_err(|e| format!("序列化失败: {e}"))?
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    if resp.status() == 409 {
        return Err("当前会话正忙，请等待当前轮次结束".to_string());
    }
    unwrap(resp).await
}

/// `POST /api/inputs`: atomically create a cursor + root input + started
/// turn. Used by the draft state's first send; `parent` is the pending attach
/// node (a Turn), `None` starts a fresh root tree.
pub async fn post_root_input(
    text: &str,
    parent: Option<String>,
    model: Option<&str>,
    tools: Option<&[String]>,
) -> Result<RootInputResponse, String> {
    #[derive(Serialize)]
    struct RootInputBody<'a> {
        text: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tools: Option<&'a [String]>,
    }
    let body = RootInputBody {
        text,
        parent: parent.as_deref(),
        model,
        tools,
    };
    let resp = Request::post(&format!("{API_BASE}/api/inputs"))
        .json(&body)
        .map_err(|e| format!("序列化失败: {e}"))?
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    unwrap(resp).await
}

pub async fn get_models() -> Result<ModelsResponse, String> {
    let resp = Request::get(&format!("{API_BASE}/api/models"))
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    unwrap(resp).await
}

pub async fn move_cursor(cursor_id: &str, node_id: &str) -> Result<Cursor, String> {
    #[derive(Serialize)]
    struct MoveBody<'a> {
        node_id: &'a str,
    }
    let resp = Request::post(&format!("{API_BASE}/api/cursors/{cursor_id}/move"))
        .json(&MoveBody { node_id })
        .map_err(|e| format!("序列化失败: {e}"))?
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    if resp.status() == 400 {
        return Err("非法落点：该节点不能作为游标位置".to_string());
    }
    if resp.status() == 409 {
        return Err("当前会话有进行中的回合，结束后才能移动指针".to_string());
    }
    unwrap(resp).await
}

pub async fn detach_cursor(cursor_id: &str) -> Result<Cursor, String> {
    let resp = Request::post(&format!("{API_BASE}/api/cursors/{cursor_id}/detach"))
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    if resp.status() == 409 {
        return Err("当前会话有进行中的回合，结束后才能 detach".to_string());
    }
    unwrap(resp).await
}

pub async fn cancel_turn(cursor_id: &str) -> Result<(), String> {
    let resp = Request::post(&format!("{API_BASE}/api/cursors/{cursor_id}/cancel"))
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    if resp.status() == 204 {
        Ok(())
    } else if resp.status() == 409 {
        Err("没有在飞的轮次".to_string())
    } else {
        Err(status_error(resp).await)
    }
}

// ---- graph management (user-side) ----

pub async fn get_graphs() -> Result<GraphsResponse, String> {
    let resp = Request::get(&format!("{API_BASE}/api/graphs"))
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    unwrap(resp).await
}

async fn unit_ok(resp: Response, action: &str) -> Result<(), String> {
    if resp.ok() {
        Ok(())
    } else {
        Err(format!("{action}失败: {}", status_error(resp).await))
    }
}

/// 新建空图并切换过去。
pub async fn create_graph(name: &str) -> Result<(), String> {
    #[derive(Serialize)]
    struct Body<'a> {
        name: &'a str,
    }
    let resp = Request::post(&format!("{API_BASE}/api/graphs"))
        .json(&Body { name })
        .map_err(|e| format!("序列化失败: {e}"))?
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    unit_ok(resp, "新建图").await
}

pub async fn activate_graph(name: &str) -> Result<(), String> {
    let resp = Request::post(&format!("{API_BASE}/api/graphs/{name}/activate"))
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    unit_ok(resp, "切换图").await
}

pub async fn rename_graph(from: &str, to: &str) -> Result<(), String> {
    #[derive(Serialize)]
    struct Body<'a> {
        name: &'a str,
    }
    let resp = Request::post(&format!("{API_BASE}/api/graphs/{from}/rename"))
        .json(&Body { name: to })
        .map_err(|e| format!("序列化失败: {e}"))?
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    unit_ok(resp, "重命名图").await
}

/// 删除 = 服务端移入回收站（.rua/graphs/.trash/），可手工恢复。
pub async fn delete_graph(name: &str) -> Result<(), String> {
    let resp = Request::delete(&format!("{API_BASE}/api/graphs/{name}"))
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    unit_ok(resp, "删除图").await
}

/// 复制当前图为一个新图（深拷贝，不切换）。
pub async fn duplicate_graph(from: &str, to: &str) -> Result<(), String> {
    #[derive(Serialize)]
    struct Body<'a> {
        name: &'a str,
    }
    let resp = Request::post(&format!("{API_BASE}/api/graphs/{from}/duplicate"))
        .json(&Body { name: to })
        .map_err(|e| format!("序列化失败: {e}"))?
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    unit_ok(resp, "复制图").await
}

/// `POST /api/clone`：把 from_graph 里选中的节点（含后继子树与引用的
/// 材料）以新 id 克隆进当前活跃图。返回克隆的节点数。
pub async fn clone_subgraph(from_graph: &str, nodes: &[String]) -> Result<usize, String> {
    #[derive(Serialize)]
    struct Body<'a> {
        from_graph: &'a str,
        nodes: &'a [String],
    }
    let resp = Request::post(&format!("{API_BASE}/api/clone"))
        .json(&Body { from_graph, nodes })
        .map_err(|e| format!("序列化失败: {e}"))?
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    let v: serde_json::Value = unwrap(resp).await?;
    Ok(v["cloned"].as_u64().unwrap_or(0) as usize)
}
