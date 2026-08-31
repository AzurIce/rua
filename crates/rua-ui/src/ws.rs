//! WebSocket client: one long-lived task with exponential-backoff reconnect.

use std::time::Duration;

use dioxus::prelude::*;
use futures::StreamExt;
use gloo_net::websocket::{Message, futures::WebSocket};
use gloo_timers::future::sleep;

use crate::api::WS_URL;
use crate::state::{AppState, ConnState, handle_event, resync};
use crate::types::WsEvent;

const BACKOFF_START_MS: u64 = 500;
const BACKOFF_MAX_MS: u64 = 10_000;

/// Spawn the WS driver. Runs forever: connect, stream events into state, and
/// on any drop reconnect with backoff, resyncing the snapshot each time.
pub fn spawn_ws_loop(mut state: AppState) {
    spawn(async move {
        let mut backoff = BACKOFF_START_MS;
        loop {
            state.conn.set(ConnState::Connecting);
            match WebSocket::open(WS_URL) {
                Ok(ws) => {
                    state.conn.set(ConnState::Connected);
                    backoff = BACKOFF_START_MS;
                    let (_write, mut read) = ws.split();
                    while let Some(msg) = read.next().await {
                        match msg {
                            Ok(Message::Text(text)) => {
                                match serde_json::from_str::<WsEvent>(&text) {
                                    Ok(ev) => handle_event(state, ev).await,
                                    Err(e) => {
                                        tracing::warn!("无法解析 WS 消息: {e}: {text}");
                                    }
                                }
                            }
                            Ok(Message::Bytes(_)) => {}
                            Err(_) => break, // socket errored/closed: reconnect
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("WS 连接失败: {e}");
                }
            }
            state.conn.set(ConnState::Disconnected);
            sleep(Duration::from_millis(backoff)).await;
            backoff = (backoff * 2).min(BACKOFF_MAX_MS);
            resync(state).await;
        }
    });
}
