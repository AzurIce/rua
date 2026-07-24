use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::types::{ToolCall, ToolDefinition};

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutcome {
    Completed { content: String },
    FailedKnown { message: String },
    OutcomeUnknown { message: String },
}

pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    fn execute(&self, arguments: Value, cancel: CancellationToken) -> ToolFuture<'_>;
}

pub trait ToolExecutor: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn execute(&self, call: ToolCall, cancel: CancellationToken) -> ToolFuture<'_>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T>(&mut self, tool: T) -> Result<(), ToolRegistryError>
    where
        T: Tool + 'static,
    {
        let definition = tool.definition();
        if definition.name.is_empty() {
            return Err(ToolRegistryError::EmptyName);
        }
        if self.tools.contains_key(&definition.name) {
            return Err(ToolRegistryError::DuplicateName(definition.name));
        }
        self.tools.insert(definition.name, Arc::new(tool));
        Ok(())
    }
}

impl ToolExecutor for ToolRegistry {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions: Vec<_> = self.tools.values().map(|tool| tool.definition()).collect();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        definitions
    }

    fn execute(&self, call: ToolCall, cancel: CancellationToken) -> ToolFuture<'_> {
        let Some(tool) = self.tools.get(&call.name).cloned() else {
            return Box::pin(async move {
                ToolOutcome::FailedKnown {
                    message: format!("unknown tool: {}", call.name),
                }
            });
        };
        Box::pin(async move { tool.execute(call.arguments, cancel).await })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolRegistryError {
    #[error("tool name cannot be empty")]
    EmptyName,
    #[error("tool name already registered: {0}")]
    DuplicateName(String),
}

#[derive(Debug, Clone)]
pub struct BashTool {
    max_output_bytes: usize,
}

impl BashTool {
    pub fn new(max_output_bytes: usize) -> Self {
        Self { max_output_bytes }
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new(64 * 1024)
    }
}

impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_owned(),
            description: "Execute a shell command in the current working directory.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, arguments: Value, cancel: CancellationToken) -> ToolFuture<'_> {
        let max_output_bytes = self.max_output_bytes;
        Box::pin(async move {
            let Some(arguments) = arguments.as_object() else {
                return ToolOutcome::FailedKnown {
                    message: "bash arguments must be a JSON object".to_owned(),
                };
            };
            let Some(command) = arguments.get("command").and_then(Value::as_str) else {
                return ToolOutcome::FailedKnown {
                    message: "missing string argument: command".to_owned(),
                };
            };

            let mut process = platform_shell(command);
            process.kill_on_drop(true);
            let output = tokio::select! {
                _ = cancel.cancelled() => {
                    return ToolOutcome::OutcomeUnknown {
                        message: "tool execution was cancelled; side effects may have occurred".to_owned(),
                    };
                }
                result = process.output() => result,
            };

            let output = match output {
                Ok(output) => output,
                Err(error) => {
                    return ToolOutcome::FailedKnown {
                        message: format!("failed to start shell: {error}"),
                    };
                }
            };
            let mut text = String::new();
            if !output.stdout.is_empty() {
                text.push_str(&String::from_utf8_lossy(&output.stdout));
            }
            if !output.stderr.is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str("stderr: ");
                text.push_str(&String::from_utf8_lossy(&output.stderr));
            }
            let text = truncate_text(text.trim(), max_output_bytes);
            if output.status.success() {
                ToolOutcome::Completed {
                    content: if text.is_empty() {
                        "(no output)".to_owned()
                    } else {
                        text
                    },
                }
            } else {
                ToolOutcome::FailedKnown {
                    message: format!(
                        "{}{}exit code: {}",
                        text,
                        if text.is_empty() { "" } else { "\n" },
                        output.status.code().unwrap_or(-1)
                    ),
                }
            }
        })
    }
}

#[cfg(windows)]
fn platform_shell(command: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new("powershell");
    process.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        command,
    ]);
    process
}

#[cfg(not(windows))]
fn platform_shell(command: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new("sh");
    process.args(["-c", command]);
    process
}

fn truncate_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[output truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_tool_is_a_known_failure() {
        let registry = ToolRegistry::new();
        let outcome = registry
            .execute(
                ToolCall {
                    id: "call-1".into(),
                    name: "missing".to_owned(),
                    arguments: json!({}),
                    provider_state: None,
                },
                CancellationToken::new(),
            )
            .await;

        assert!(matches!(outcome, ToolOutcome::FailedKnown { .. }));
    }

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        assert_eq!(truncate_text("你好", 4), "你\n[output truncated]");
    }
}
