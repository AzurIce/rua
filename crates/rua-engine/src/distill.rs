//! Distillation: one non-streaming completion that condenses material into a
//! reusable summary. The server wraps the returned body in a Context node.

use rig_core::completion::CompletionModel;
use rig_core::completion::message::{AssistantContent, Message};

use crate::client::Engine;
use crate::error::{Error, Result};

const DISTILL_INSTRUCTION: &str = "Distill the following material into a concise, reusable \
    summary. Preserve facts, decisions, file paths, and identifiers exactly; drop chatter and \
    redundancy. Reply with the summary text only.";

impl Engine {
    /// One-shot summarize: material in, summary body text out.
    pub async fn summarize(&self, material: &str) -> Result<String> {
        let prompt = format!("{DISTILL_INSTRUCTION}\n\n<material>\n{material}\n</material>");
        let mut builder = self.model.completion_request(Message::user(prompt));
        if let Some(additional_params) = &self.additional_params {
            builder = builder.additional_params(additional_params.clone());
        }
        let response = builder.send().await?;
        let body = response
            .choice
            .iter()
            .filter_map(|item| match item {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        if body.is_empty() {
            return Err(Error::EmptyDistill);
        }
        Ok(body)
    }
}
