use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use super::tools::{Tool, ToolFuture, ToolOutcome, ToolRegistry, ToolRegistryError};
use super::types::{ReplayClass, ToolDefinition};

const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_SEARCH_RESULTS: usize = 200;

pub fn register_coding_tools(
    registry: &mut ToolRegistry,
    root: impl AsRef<Path>,
) -> Result<(), CodingToolError> {
    let workspace = Arc::new(Workspace::new(root.as_ref())?);
    registry.register(ReadFileTool(workspace.clone()))?;
    registry.register(WriteFileTool(workspace.clone()))?;
    registry.register(EditFileTool(workspace.clone()))?;
    registry.register(GlobTool(workspace.clone()))?;
    registry.register(GrepTool(workspace))?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum CodingToolError {
    #[error("failed to open workspace: {0}")]
    Workspace(#[from] std::io::Error),
    #[error("failed to register coding tool: {0}")]
    Registry(#[from] ToolRegistryError),
}

#[derive(Debug)]
struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(root: &Path) -> Result<Self, std::io::Error> {
        Ok(Self {
            root: root.canonicalize()?,
        })
    }

    fn existing(&self, value: &str) -> Result<PathBuf, String> {
        let relative = safe_relative(value)?;
        let path = self
            .root
            .join(relative)
            .canonicalize()
            .map_err(|error| format!("cannot open {value}: {error}"))?;
        self.ensure_inside(path, value)
    }

    fn writable(&self, value: &str) -> Result<PathBuf, String> {
        let relative = safe_relative(value)?;
        let path = self.root.join(relative);
        if path.exists() {
            return self
                .ensure_inside(
                    path.canonicalize()
                        .map_err(|error| format!("cannot resolve {value}: {error}"))?,
                    value,
                )
                .map(|_| path);
        }
        let mut ancestor = path.parent();
        while let Some(candidate) = ancestor {
            if candidate.exists() {
                let canonical = candidate
                    .canonicalize()
                    .map_err(|error| format!("cannot resolve parent of {value}: {error}"))?;
                self.ensure_inside(canonical, value)?;
                return Ok(path);
            }
            ancestor = candidate.parent();
        }
        Err(format!("cannot resolve a workspace parent for {value}"))
    }

    fn ensure_inside(&self, path: PathBuf, display: &str) -> Result<PathBuf, String> {
        if path.starts_with(&self.root) {
            Ok(path)
        } else {
            Err(format!("path escapes the workspace: {display}"))
        }
    }

    fn relative_display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

fn safe_relative(value: &str) -> Result<&Path, String> {
    let path = Path::new(value);
    if value.is_empty() || path.is_absolute() {
        return Err("path must be a non-empty workspace-relative path".to_owned());
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(format!("path traversal is not allowed: {value}"));
    }
    Ok(path)
}

struct ReadFileTool(Arc<Workspace>);

impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".to_owned(),
            description:
                "Read a UTF-8 workspace file, optionally selecting an inclusive line range."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer", "minimum": 1 },
                    "end_line": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            replay_class: ReplayClass::ReadOnly,
        }
    }

    fn execute(&self, arguments: Value, _cancel: CancellationToken) -> ToolFuture<'_> {
        let workspace = self.0.clone();
        Box::pin(async move {
            let path = match string_argument(&arguments, "path")
                .and_then(|path| workspace.existing(path))
            {
                Ok(path) => path,
                Err(message) => return ToolOutcome::FailedKnown { message },
            };
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(error) => {
                    return ToolOutcome::FailedKnown {
                        message: format!("failed to read {}: {error}", path.display()),
                    };
                }
            };
            let start = usize_argument(&arguments, "start_line").unwrap_or(1);
            let end = usize_argument(&arguments, "end_line").unwrap_or(usize::MAX);
            if start == 0 || end < start {
                return ToolOutcome::FailedKnown {
                    message: "line range must satisfy 1 <= start_line <= end_line".to_owned(),
                };
            }
            let selected = content
                .lines()
                .enumerate()
                .filter(|(index, _)| {
                    let line = index + 1;
                    line >= start && line <= end
                })
                .map(|(index, line)| format!("{}: {line}", index + 1))
                .collect::<Vec<_>>()
                .join("\n");
            ToolOutcome::Completed {
                content: bounded(if selected.is_empty() {
                    "(no matching lines)".to_owned()
                } else {
                    selected
                }),
            }
        })
    }
}

struct WriteFileTool(Arc<Workspace>);

impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".to_owned(),
            description: "Create or replace a UTF-8 file inside the workspace.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
            replay_class: ReplayClass::Effectful,
        }
    }

    fn execute(&self, arguments: Value, _cancel: CancellationToken) -> ToolFuture<'_> {
        let workspace = self.0.clone();
        Box::pin(async move {
            let path = match string_argument(&arguments, "path")
                .and_then(|path| workspace.writable(path))
            {
                Ok(path) => path,
                Err(message) => return ToolOutcome::FailedKnown { message },
            };
            let content = match string_argument(&arguments, "content") {
                Ok(content) => content,
                Err(message) => return ToolOutcome::FailedKnown { message },
            };
            if let Some(parent) = path.parent()
                && let Err(error) = std::fs::create_dir_all(parent)
            {
                return ToolOutcome::FailedKnown {
                    message: format!("failed to create {}: {error}", parent.display()),
                };
            }
            match std::fs::write(&path, content) {
                Ok(()) => ToolOutcome::Completed {
                    content: format!(
                        "wrote {} bytes to {}",
                        content.len(),
                        workspace.relative_display(&path)
                    ),
                },
                Err(error) => ToolOutcome::FailedKnown {
                    message: format!("failed to write {}: {error}", path.display()),
                },
            }
        })
    }
}

struct EditFileTool(Arc<Workspace>);

impl Tool for EditFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit_file".to_owned(),
            description: "Replace an exact string in a UTF-8 workspace file.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string" },
                    "new_text": { "type": "string" },
                    "replace_all": { "type": "boolean" }
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
            replay_class: ReplayClass::Effectful,
        }
    }

    fn execute(&self, arguments: Value, _cancel: CancellationToken) -> ToolFuture<'_> {
        let workspace = self.0.clone();
        Box::pin(async move {
            let path = match string_argument(&arguments, "path")
                .and_then(|path| workspace.existing(path))
            {
                Ok(path) => path,
                Err(message) => return ToolOutcome::FailedKnown { message },
            };
            let old_text = match string_argument(&arguments, "old_text") {
                Ok(value) if !value.is_empty() => value,
                Ok(_) => {
                    return ToolOutcome::FailedKnown {
                        message: "old_text cannot be empty".to_owned(),
                    };
                }
                Err(message) => return ToolOutcome::FailedKnown { message },
            };
            let new_text = match string_argument(&arguments, "new_text") {
                Ok(value) => value,
                Err(message) => return ToolOutcome::FailedKnown { message },
            };
            let replace_all = bool_argument(&arguments, "replace_all").unwrap_or(false);
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(error) => {
                    return ToolOutcome::FailedKnown {
                        message: format!("failed to read {}: {error}", path.display()),
                    };
                }
            };
            let matches = content.matches(old_text).count();
            if matches == 0 {
                return ToolOutcome::FailedKnown {
                    message: "old_text was not found".to_owned(),
                };
            }
            if matches > 1 && !replace_all {
                return ToolOutcome::FailedKnown {
                    message: format!(
                        "old_text matched {matches} times; set replace_all=true to replace all"
                    ),
                };
            }
            let updated = if replace_all {
                content.replace(old_text, new_text)
            } else {
                content.replacen(old_text, new_text, 1)
            };
            match std::fs::write(&path, updated) {
                Ok(()) => ToolOutcome::Completed {
                    content: format!(
                        "replaced {} occurrence(s) in {}",
                        if replace_all { matches } else { 1 },
                        workspace.relative_display(&path)
                    ),
                },
                Err(error) => ToolOutcome::FailedKnown {
                    message: format!("failed to write {}: {error}", path.display()),
                },
            }
        })
    }
}

struct GlobTool(Arc<Workspace>);

impl Tool for GlobTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "glob".to_owned(),
            description: "List workspace files matching a glob pattern.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": { "pattern": { "type": "string" } },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            replay_class: ReplayClass::ReadOnly,
        }
    }

    fn execute(&self, arguments: Value, cancel: CancellationToken) -> ToolFuture<'_> {
        let workspace = self.0.clone();
        Box::pin(async move {
            let pattern = match string_argument(&arguments, "pattern") {
                Ok(pattern) => pattern,
                Err(message) => return ToolOutcome::FailedKnown { message },
            };
            let matcher = match glob_matcher(pattern) {
                Ok(matcher) => matcher,
                Err(message) => return ToolOutcome::FailedKnown { message },
            };
            let mut results = Vec::new();
            for entry in WalkDir::new(&workspace.root).follow_links(false) {
                if cancel.is_cancelled() {
                    return ToolOutcome::FailedKnown {
                        message: "glob search cancelled before producing side effects".to_owned(),
                    };
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => continue,
                };
                if entry.file_type().is_file() {
                    let relative = workspace.relative_display(entry.path());
                    if matcher.is_match(&relative) {
                        results.push(relative);
                        if results.len() == MAX_SEARCH_RESULTS {
                            break;
                        }
                    }
                }
            }
            results.sort();
            ToolOutcome::Completed {
                content: if results.is_empty() {
                    "(no matches)".to_owned()
                } else {
                    results.join("\n")
                },
            }
        })
    }
}

struct GrepTool(Arc<Workspace>);

impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".to_owned(),
            description: "Search UTF-8 workspace files with a regular expression.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "glob": { "type": "string" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            replay_class: ReplayClass::ReadOnly,
        }
    }

    fn execute(&self, arguments: Value, cancel: CancellationToken) -> ToolFuture<'_> {
        let workspace = self.0.clone();
        Box::pin(async move {
            let pattern = match string_argument(&arguments, "pattern") {
                Ok(pattern) => pattern,
                Err(message) => return ToolOutcome::FailedKnown { message },
            };
            let regex = match Regex::new(pattern) {
                Ok(regex) => regex,
                Err(error) => {
                    return ToolOutcome::FailedKnown {
                        message: format!("invalid regular expression: {error}"),
                    };
                }
            };
            let matcher = match optional_string_argument(&arguments, "glob") {
                Some(pattern) => match glob_matcher(pattern) {
                    Ok(matcher) => Some(matcher),
                    Err(message) => return ToolOutcome::FailedKnown { message },
                },
                None => None,
            };
            let mut results = Vec::new();
            for entry in WalkDir::new(&workspace.root).follow_links(false) {
                if cancel.is_cancelled() {
                    return ToolOutcome::FailedKnown {
                        message: "grep search cancelled before producing side effects".to_owned(),
                    };
                }
                let entry = match entry {
                    Ok(entry) if entry.file_type().is_file() => entry,
                    _ => continue,
                };
                let relative = workspace.relative_display(entry.path());
                if matcher
                    .as_ref()
                    .is_some_and(|matcher| !matcher.is_match(&relative))
                {
                    continue;
                }
                let content = match std::fs::read_to_string(entry.path()) {
                    Ok(content) => content,
                    Err(_) => continue,
                };
                for (index, line) in content.lines().enumerate() {
                    if regex.is_match(line) {
                        results.push(format!("{relative}:{}:{line}", index + 1));
                        if results.len() == MAX_SEARCH_RESULTS {
                            break;
                        }
                    }
                }
                if results.len() == MAX_SEARCH_RESULTS {
                    break;
                }
            }
            ToolOutcome::Completed {
                content: bounded(if results.is_empty() {
                    "(no matches)".to_owned()
                } else {
                    results.join("\n")
                }),
            }
        })
    }
}

fn string_argument<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments
        .as_object()
        .ok_or_else(|| "tool arguments must be a JSON object".to_owned())?
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string argument: {name}"))
}

fn optional_string_argument<'a>(arguments: &'a Value, name: &str) -> Option<&'a str> {
    arguments.as_object()?.get(name)?.as_str()
}

fn usize_argument(arguments: &Value, name: &str) -> Option<usize> {
    arguments
        .as_object()?
        .get(name)?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
}

fn bool_argument(arguments: &Value, name: &str) -> Option<bool> {
    arguments.as_object()?.get(name)?.as_bool()
}

fn glob_matcher(pattern: &str) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    builder.add(Glob::new(pattern).map_err(|error| format!("invalid glob: {error}"))?);
    builder
        .build()
        .map_err(|error| format!("invalid glob: {error}"))
}

fn bounded(mut text: String) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text;
    }
    let mut end = MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n[output truncated]");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ToolExecutor;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rua-coding-tools-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    async fn execute(registry: &ToolRegistry, name: &str, arguments: Value) -> ToolOutcome {
        registry
            .execute(
                crate::agent::ToolCall {
                    id: "call-1".into(),
                    name: name.to_owned(),
                    arguments,
                    provider_state: None,
                },
                CancellationToken::new(),
            )
            .await
    }

    #[tokio::test]
    async fn reads_and_edits_files_without_leaving_the_workspace() {
        let root = temp_root("edit");
        std::fs::write(root.join("sample.txt"), "one\ntwo\nthree\n").unwrap();
        let mut registry = ToolRegistry::new();
        register_coding_tools(&mut registry, &root).unwrap();

        assert_eq!(
            execute(
                &registry,
                "read_file",
                json!({"path":"sample.txt", "start_line":2, "end_line":2}),
            )
            .await,
            ToolOutcome::Completed {
                content: "2: two".to_owned()
            }
        );
        assert!(matches!(
            execute(
                &registry,
                "edit_file",
                json!({"path":"sample.txt", "old_text":"two", "new_text":"second"}),
            )
            .await,
            ToolOutcome::Completed { .. }
        ));
        assert_eq!(
            std::fs::read_to_string(root.join("sample.txt")).unwrap(),
            "one\nsecond\nthree\n"
        );
        assert!(matches!(
            execute(&registry, "read_file", json!({"path":"../outside.txt"})).await,
            ToolOutcome::FailedKnown { .. }
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn glob_and_grep_return_bounded_workspace_results() {
        let root = temp_root("search");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn durable_runtime() {}\n").unwrap();
        std::fs::write(root.join("README.md"), "runtime\n").unwrap();
        let mut registry = ToolRegistry::new();
        register_coding_tools(&mut registry, &root).unwrap();

        assert_eq!(
            execute(&registry, "glob", json!({"pattern":"src/*.rs"})).await,
            ToolOutcome::Completed {
                content: "src/lib.rs".to_owned()
            }
        );
        assert_eq!(
            execute(
                &registry,
                "grep",
                json!({"pattern":"durable_.*", "glob":"**/*.rs"}),
            )
            .await,
            ToolOutcome::Completed {
                content: "src/lib.rs:1:fn durable_runtime() {}".to_owned()
            }
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
