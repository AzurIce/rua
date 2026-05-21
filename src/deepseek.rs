use std::pin::Pin;

use color_eyre::{eyre::Context, Result};
use futures::{Stream, StreamExt};
use reqwest::header::{self, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::config::DeepSeekConfig;

/// A single chat message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

/// Request body for DeepSeek Chat Completions API
#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
}

/// Delta from a streaming chunk
#[derive(Debug, Deserialize, Default, Clone)]
pub struct ChoiceDelta {
    pub content: Option<String>,
    pub role: Option<String>,
}

/// A single choice in a streaming chunk
#[derive(Debug, Deserialize, Clone)]
pub struct StreamChoice {
    pub delta: ChoiceDelta,
    pub index: Option<u32>,
    pub finish_reason: Option<String>,
}

/// One SSE chunk from DeepSeek
#[derive(Debug, Deserialize, Clone)]
pub struct StreamChunk {
    pub id: Option<String>,
    pub object: Option<String>,
    pub created: Option<u64>,
    pub model: Option<String>,
    pub choices: Option<Vec<StreamChoice>>,
}

/// Events emitted from the DeepSeek stream
#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    Done,
    Error(String),
}

#[derive(Clone)]
pub struct DeepSeekClient {
    client: reqwest::Client,
    config: DeepSeekConfig,
}

impl DeepSeekClient {
    pub fn new(config: DeepSeekConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", config.api_key))
                .context("invalid api_key")?,
        );

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self { client, config })
    }

    /// Send a chat request and return a stream of events.
    ///
    /// Uses line-buffered SSE parsing so that JSON objects split across
    /// multiple HTTP chunks are reassembled before parsing.
    pub async fn chat_stream(
        &self,
        messages: Vec<Message>,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>> {
        let body = ChatRequest {
            model: self.config.model.clone(),
            messages,
            stream: true,
        };

        let url = format!("{}/chat/completions", self.config.base_url);
        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to POST to {}", url))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            color_eyre::eyre::bail!("DeepSeek API error {}: {}", status, text);
        }

        let mut byte_stream = response.bytes_stream();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();

        tokio::spawn(async move {
            let mut buffer = String::new();

            while let Some(result) = byte_stream.next().await {
                let chunk = match result {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Error(format!("stream error: {}", e)));
                        break;
                    }
                };

                // Append raw bytes to buffer (lossy is fine for JSON-over-SSE)
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                // Extract complete lines from the buffer
                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].trim_end_matches('\r').to_string();
                    buffer = buffer[pos + 1..].to_string();

                    if !line.starts_with("data: ") {
                        continue;
                    }
                    let data = &line[6..];
                    if data == "[DONE]" {
                        let _ = tx.send(StreamEvent::Done);
                        continue;
                    }

                    match serde_json::from_str::<StreamChunk>(data) {
                        Ok(chunk) => {
                            if let Some(choices) = chunk.choices {
                                for choice in choices {
                                    if let Some(content) = choice.delta.content {
                                        if !content.is_empty() {
                                            let _ = tx.send(StreamEvent::TextDelta(content));
                                        }
                                    }
                                    if choice.finish_reason.is_some() {
                                        let _ = tx.send(StreamEvent::Done);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(StreamEvent::Error(format!(
                                "JSON parse error: {} (data: {})",
                                e,
                                &data[..data.len().min(80)]
                            )));
                        }
                    }
                }
            }
        });

        Ok(Box::pin(UnboundedReceiverStream::new(rx)))
    }
}
