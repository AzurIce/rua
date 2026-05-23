use futures::StreamExt;
use tokio::sync::mpsc;

use crate::app::event::UiEvent;
use crate::deepseek::{DeepSeekClient, Message, StreamEvent};

/// Manages the lifecycle of a chat turn (request → stream → completion).
///
/// Encapsulates the background task spawning so that `main.rs` only
/// needs to route UI events.
#[derive(Clone)]
pub struct Session {
    client: DeepSeekClient,
    tx: mpsc::UnboundedSender<UiEvent>,
}

impl Session {
    pub fn new(client: DeepSeekClient, tx: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self { client, tx }
    }

    /// Send a chat request and stream the response back as `UiEvent`s.
    ///
    /// Returns the spawned task handle so the caller can abort it on quit.
    pub fn send_message(
        &self,
        messages: Vec<Message>,
    ) -> tokio::task::JoinHandle<()> {
        let client = self.client.clone();
        let tx = self.tx.clone();

        tokio::spawn(async move {
            match client.chat_stream(messages).await {
                Ok(mut stream) => {
                    while let Some(event) = stream.next().await {
                        let ui_event = match event {
                            StreamEvent::TextDelta(delta) => UiEvent::StreamDelta(delta),
                            StreamEvent::Done => UiEvent::StreamDone,
                            StreamEvent::Error(e) => UiEvent::StreamError(e),
                        };
                        let _ = tx.send(ui_event);
                    }
                }
                Err(e) => {
                    let _ = tx.send(UiEvent::StreamError(format!(
                        "Request failed: {}",
                        e
                    )));
                }
            }
        })
    }
}
