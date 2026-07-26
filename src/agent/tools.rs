use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use command_group::AsyncCommandGroup;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use super::types::{DirectorySnapshot, ToolCall, ToolDefinition};

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutcome {
    Completed { content: String },
    CompletedWithEffect { content: String, effect: ToolEffect },
    FailedKnown { message: String },
    OutcomeUnknown { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolEffect {
    ChangeDirectory {
        input: String,
        from: DirectorySnapshot,
        to: DirectorySnapshot,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionContext {
    pub directory: DirectorySnapshot,
}

pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    fn execute(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
        cancel: CancellationToken,
    ) -> ToolFuture<'_>;
}

pub trait ToolExecutor: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn definition(&self, name: &str) -> Option<ToolDefinition> {
        self.definitions()
            .into_iter()
            .find(|definition| definition.name == name)
    }
    fn execute(
        &self,
        call: ToolCall,
        context: ToolExecutionContext,
        cancel: CancellationToken,
    ) -> ToolFuture<'_>;
}

pub struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
    max_argument_bytes: usize,
}

struct RegisteredTool {
    tool: Arc<dyn Tool>,
    validator: Arc<jsonschema::Validator>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self {
            tools: HashMap::new(),
            max_argument_bytes: 256 * 1024,
        }
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_argument_bytes(mut self, max_argument_bytes: usize) -> Self {
        self.max_argument_bytes = max_argument_bytes.max(1);
        self
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
        let validator = jsonschema::validator_for(&definition.parameters).map_err(|error| {
            ToolRegistryError::InvalidSchema {
                name: definition.name.clone(),
                message: error.to_string(),
            }
        })?;
        self.tools.insert(
            definition.name,
            RegisteredTool {
                tool: Arc::new(tool),
                validator: Arc::new(validator),
            },
        );
        Ok(())
    }
}

impl ToolExecutor for ToolRegistry {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions: Vec<_> = self
            .tools
            .values()
            .map(|registered| registered.tool.definition())
            .collect();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        definitions
    }

    fn execute(
        &self,
        call: ToolCall,
        context: ToolExecutionContext,
        cancel: CancellationToken,
    ) -> ToolFuture<'_> {
        let Some(registered) = self.tools.get(&call.name) else {
            return Box::pin(async move {
                ToolOutcome::FailedKnown {
                    message: format!("unknown tool: {}", call.name),
                }
            });
        };
        let argument_bytes = serde_json::to_vec(&call.arguments)
            .map(|encoded| encoded.len())
            .unwrap_or(usize::MAX);
        if argument_bytes > self.max_argument_bytes {
            let message = format!(
                "arguments for tool {} exceed the {} byte limit",
                call.name, self.max_argument_bytes
            );
            return Box::pin(async move { ToolOutcome::FailedKnown { message } });
        }
        if let Err(error) = registered.validator.validate(&call.arguments) {
            let message = format!("invalid arguments for tool {}: {error}", call.name);
            return Box::pin(async move { ToolOutcome::FailedKnown { message } });
        }
        let tool = registered.tool.clone();
        Box::pin(async move { tool.execute(call.arguments, context, cancel).await })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolRegistryError {
    #[error("tool name cannot be empty")]
    EmptyName,
    #[error("tool name already registered: {0}")]
    DuplicateName(String),
    #[error("invalid JSON schema for tool {name}: {message}")]
    InvalidSchema { name: String, message: String },
}

#[derive(Debug, Clone)]
pub struct BashTool {
    max_output_bytes: usize,
    timeout: Duration,
}

impl BashTool {
    pub fn new(max_output_bytes: usize) -> Self {
        Self {
            max_output_bytes,
            timeout: Duration::from_secs(120),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
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
            replay_class: super::types::ReplayClass::Unknown,
        }
    }

    fn execute(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
        cancel: CancellationToken,
    ) -> ToolFuture<'_> {
        let max_output_bytes = self.max_output_bytes;
        let cwd = context.directory.path;
        let timeout = self.timeout;
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
            process.current_dir(cwd);
            process.stdout(Stdio::piped()).stderr(Stdio::piped());
            if cancel.is_cancelled() {
                return ToolOutcome::FailedKnown {
                    message: "tool execution was cancelled before the shell started".to_owned(),
                };
            }

            let mut group = process.group();
            group.kill_on_drop(true);
            let mut child = match group.spawn() {
                Ok(child) => child,
                Err(error) => {
                    return ToolOutcome::FailedKnown {
                        message: format!("failed to start shell: {error}"),
                    };
                }
            };
            let stdout = child.inner().stdout.take();
            let stderr = child.inner().stderr.take();
            let stdout_task = tokio::spawn(read_pipe(stdout, max_output_bytes));
            let stderr_task = tokio::spawn(read_pipe(stderr, max_output_bytes));

            let status: ExitStatus = tokio::select! {
                _ = cancel.cancelled() => {
                    let cleanup = terminate_group(&mut child).await;
                    return ToolOutcome::OutcomeUnknown {
                        message: append_cleanup_error(
                            "tool execution was cancelled; side effects may have occurred".to_owned(),
                            cleanup,
                        ),
                    };
                }
                _ = tokio::time::sleep(timeout) => {
                    let cleanup = terminate_group(&mut child).await;
                    return ToolOutcome::OutcomeUnknown {
                        message: append_cleanup_error(
                            format!("tool execution exceeded {:?}; side effects may have occurred", timeout),
                            cleanup,
                        ),
                    };
                }
                result = child.wait() => match result {
                    Ok(status) => status,
                    Err(error) => {
                        return ToolOutcome::OutcomeUnknown {
                            message: format!("failed while waiting for the shell process group: {error}; side effects may have occurred"),
                        };
                    }
                },
            };

            let stdout = match stdout_task.await {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => {
                    return ToolOutcome::OutcomeUnknown {
                        message: format!("failed to read shell stdout after start: {error}"),
                    };
                }
                Err(error) => {
                    return ToolOutcome::OutcomeUnknown {
                        message: format!("shell stdout reader failed after start: {error}"),
                    };
                }
            };
            let stderr = match stderr_task.await {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => {
                    return ToolOutcome::OutcomeUnknown {
                        message: format!("failed to read shell stderr after start: {error}"),
                    };
                }
                Err(error) => {
                    return ToolOutcome::OutcomeUnknown {
                        message: format!("shell stderr reader failed after start: {error}"),
                    };
                }
            };
            let mut text = String::new();
            if !stdout.bytes.is_empty() {
                text.push_str(&String::from_utf8_lossy(&stdout.bytes));
            }
            if !stderr.bytes.is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str("stderr: ");
                text.push_str(&String::from_utf8_lossy(&stderr.bytes));
            }
            if (stdout.truncated || stderr.truncated) && !text.ends_with("[output truncated]") {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str("[output truncated]");
            }
            let text = truncate_text(text.trim(), max_output_bytes);
            if status.success() {
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
                        status.code().unwrap_or(-1)
                    ),
                }
            }
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ChangeDirectoryTool;

impl Tool for ChangeDirectoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "change_directory".to_owned(),
            description:
                "Change Rua's durable working directory for subsequent model steps and tools."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path or path relative to the current working directory"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            replay_class: super::types::ReplayClass::ReadOnly,
        }
    }

    fn execute(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
        _cancel: CancellationToken,
    ) -> ToolFuture<'_> {
        Box::pin(async move {
            let Some(input) = arguments.get("path").and_then(Value::as_str) else {
                return ToolOutcome::FailedKnown {
                    message: "missing string argument: path".to_owned(),
                };
            };
            if input.trim().is_empty() {
                return ToolOutcome::FailedKnown {
                    message: "directory path cannot be empty".to_owned(),
                };
            }
            let requested = std::path::PathBuf::from(input.trim());
            let target = if requested.is_absolute() {
                requested
            } else {
                context.directory.path.join(requested)
            };
            let canonical = match target.canonicalize() {
                Ok(path) if path.is_dir() => path,
                Ok(path) => {
                    return ToolOutcome::FailedKnown {
                        message: format!("not a directory: {}", path.display()),
                    };
                }
                Err(error) => {
                    return ToolOutcome::FailedKnown {
                        message: format!("cannot open {}: {error}", target.display()),
                    };
                }
            };
            let Some(next_revision) = context.directory.revision.0.checked_add(1) else {
                return ToolOutcome::FailedKnown {
                    message: "working directory revision overflow".to_owned(),
                };
            };
            let to = DirectorySnapshot {
                path: canonical,
                revision: super::types::DirectoryRevision(next_revision),
            };
            ToolOutcome::CompletedWithEffect {
                content: format!("working directory: {}", to.path.display()),
                effect: ToolEffect::ChangeDirectory {
                    input: input.to_owned(),
                    from: context.directory,
                    to,
                },
            }
        })
    }
}

struct CapturedPipe {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_pipe(
    pipe: Option<impl tokio::io::AsyncRead + Unpin>,
    max_bytes: usize,
) -> Result<CapturedPipe, std::io::Error> {
    let mut output = Vec::with_capacity(max_bytes.min(8192));
    let mut truncated = false;
    if let Some(mut pipe) = pipe {
        let mut chunk = [0_u8; 8192];
        loop {
            let read = pipe.read(&mut chunk).await?;
            if read == 0 {
                break;
            }
            let remaining = max_bytes.saturating_sub(output.len());
            let retained = remaining.min(read);
            output.extend_from_slice(&chunk[..retained]);
            truncated |= retained < read;
        }
    }
    Ok(CapturedPipe {
        bytes: output,
        truncated,
    })
}

async fn terminate_group(child: &mut command_group::AsyncGroupChild) -> Result<(), std::io::Error> {
    child.kill().await
}

fn append_cleanup_error(message: String, cleanup: Result<(), std::io::Error>) -> String {
    match cleanup {
        Ok(()) => message,
        Err(error) => format!("{message}; process-group cleanup also failed: {error}"),
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
    use tokio::io::AsyncWriteExt;

    use super::*;

    fn execution_context() -> ToolExecutionContext {
        ToolExecutionContext {
            directory: DirectorySnapshot {
                path: std::env::current_dir().unwrap(),
                revision: super::super::types::DirectoryRevision::default(),
            },
        }
    }

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
                execution_context(),
                CancellationToken::new(),
            )
            .await;

        assert!(matches!(outcome, ToolOutcome::FailedKnown { .. }));
    }

    struct RequiresValueTool;

    impl Tool for RequiresValueTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "requires_value".to_owned(),
                description: "test schema validation".to_owned(),
                parameters: json!({
                    "type": "object",
                    "properties": { "value": { "type": "string" } },
                    "required": ["value"],
                    "additionalProperties": false
                }),
                replay_class: super::super::types::ReplayClass::ReadOnly,
            }
        }

        fn execute(
            &self,
            _arguments: Value,
            _context: ToolExecutionContext,
            _cancel: CancellationToken,
        ) -> ToolFuture<'_> {
            panic!("schema-invalid arguments reached the tool")
        }
    }

    #[tokio::test]
    async fn registry_rejects_schema_invalid_arguments_before_execution() {
        let mut registry = ToolRegistry::new();
        registry.register(RequiresValueTool).unwrap();

        let outcome = registry
            .execute(
                ToolCall {
                    id: "call-1".into(),
                    name: "requires_value".to_owned(),
                    arguments: json!({}),
                    provider_state: None,
                },
                execution_context(),
                CancellationToken::new(),
            )
            .await;

        assert!(matches!(outcome, ToolOutcome::FailedKnown { .. }));
    }

    #[tokio::test]
    async fn registry_rejects_oversized_arguments_before_execution() {
        let mut registry = ToolRegistry::new().with_max_argument_bytes(16);
        registry.register(RequiresValueTool).unwrap();

        let outcome = registry
            .execute(
                ToolCall {
                    id: "call-1".into(),
                    name: "requires_value".to_owned(),
                    arguments: json!({ "value": "this is larger than the configured limit" }),
                    provider_state: None,
                },
                execution_context(),
                CancellationToken::new(),
            )
            .await;

        assert!(matches!(outcome, ToolOutcome::FailedKnown { .. }));
    }

    #[tokio::test]
    async fn bash_uses_the_captured_working_directory() {
        let root = std::env::temp_dir().join(format!(
            "rua-bash-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let context = ToolExecutionContext {
            directory: DirectorySnapshot {
                path: root.canonicalize().unwrap(),
                revision: super::super::types::DirectoryRevision(7),
            },
        };
        #[cfg(windows)]
        let command = "Set-Content -Path cwd-probe.txt -Value ok";
        #[cfg(not(windows))]
        let command = "printf ok > cwd-probe.txt";

        let outcome = BashTool::default()
            .execute(
                json!({ "command": command }),
                context,
                CancellationToken::new(),
            )
            .await;

        assert!(matches!(outcome, ToolOutcome::Completed { .. }));
        assert!(root.join("cwd-probe.txt").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn pipe_capture_drains_input_but_retains_only_the_limit() {
        let (mut writer, reader) = tokio::io::duplex(32);
        let write = tokio::spawn(async move {
            writer.write_all(b"0123456789").await.unwrap();
        });

        let captured = read_pipe(Some(reader), 4).await.unwrap();
        write.await.unwrap();

        assert_eq!(captured.bytes, b"0123");
        assert!(captured.truncated);
    }

    #[tokio::test]
    async fn bash_cancellation_before_start_is_known() {
        let cancel = CancellationToken::new();
        cancel.cancel();

        let outcome = BashTool::default()
            .execute(
                json!({ "command": long_running_command() }),
                execution_context(),
                cancel,
            )
            .await;

        assert!(matches!(outcome, ToolOutcome::FailedKnown { .. }));
    }

    #[tokio::test]
    async fn bash_timeout_after_start_is_unknown() {
        let outcome = BashTool::default()
            .with_timeout(Duration::from_millis(10))
            .execute(
                json!({ "command": long_running_command() }),
                execution_context(),
                CancellationToken::new(),
            )
            .await;

        assert!(matches!(outcome, ToolOutcome::OutcomeUnknown { .. }));
    }

    #[cfg(windows)]
    fn long_running_command() -> &'static str {
        "Start-Sleep -Seconds 10"
    }

    #[cfg(not(windows))]
    fn long_running_command() -> &'static str {
        "sleep 10"
    }

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        assert_eq!(truncate_text("你好", 4), "你\n[output truncated]");
    }
}
