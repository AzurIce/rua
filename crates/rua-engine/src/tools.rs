//! The `bash` tool, behavior aligned with pi's bash tool (pi-mono
//! `coding-agent/src/core/tools/bash.ts`):
//!
//! - stdout/stderr **实时交错合并**（像终端一样保留先后顺序）；
//! - 输出**尾部截断**：最后 2000 行 / 50KB 先到先截，截断时完整输出落盘到
//!   临时文件，通知里带路径，模型可以自己去 grep/读；
//! - **无默认超时**（`timeout` 秒数可选）；超时与会话取消都会杀掉整个
//!   **进程组**（shell 通过 `process_group(0)` 成为组长，子孙进程一并带走）。
//!
//! 错误不抛出，全部作为工具输出文本返回，让模型自己看到并恢复。

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use rig_core::completion::ToolDefinition;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

/// 尾部截断的两个独立上限（先到先截），与 pi 一致。
pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

/// shell 退出后排干管道的宽限期：detached 的子孙进程可能一直握着管道
/// 写端，不能因为等 EOF 而挂住。
const DRAIN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct BashTool {
    cwd: PathBuf,
}

#[derive(Debug, serde::Deserialize)]
struct BashArgs {
    command: String,
    /// 秒；不设 = 无超时（等命令自己结束或被取消）。
    timeout: Option<u64>,
}

impl BashTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_string(),
            description: format!(
                "Execute a bash command in the project root. Returns stdout and stderr \
                 interleaved in real time. Output is truncated to the last {MAX_LINES} lines \
                 or {}KB (whichever is hit first); when truncated, the full output is saved \
                 to a temp file whose path is included in the notice. Optionally provide a \
                 timeout in seconds (no timeout by default).",
                MAX_BYTES / 1024
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The bash command to execute"
                    },
                    "timeout": {
                        "type": "number",
                        "description": "Optional timeout in seconds (no default)"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    /// Execute one call. Errors are reported as tool output text (never
    /// thrown) so the model can see and recover from them.
    pub async fn execute(&self, args: &serde_json::Value, cancel: &CancellationToken) -> String {
        let args: BashArgs = match serde_json::from_value(args.clone()) {
            Ok(args) => args,
            Err(e) => return format!("error: invalid bash arguments: {e}"),
        };
        let mut command = tokio::process::Command::new("sh");
        // `exec 2>&1` 在 shell 内把 stderr 并进 stdout：单一管道 = 输出严格
        // 按产生顺序交错（两条管道在 OS 层面无法保证顺序，谁就绪先读谁）。
        // 模型自己的 `2>file` 等重定向写在后面，仍然生效。
        command
            .arg("-c")
            .arg(format!("exec 2>&1\n{}", args.command))
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // shell 自成进程组组长：超时/取消时整组 SIGKILL，子孙一并带走。
        #[cfg(unix)]
        command.process_group(0);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => return format!("error: failed to spawn `sh -c`: {e}"),
        };
        let pid = child.id();
        let mut stdout = child.stdout.take().expect("piped");
        let timeout = args
            .timeout
            .filter(|secs| *secs > 0)
            .map(Duration::from_secs);

        let mut buf: Vec<u8> = Vec::new();
        let mut chunk_out = [0u8; 8192];
        let mut out_open = true;
        // None = 命令正常退出（code 可取）；Some(标签) = 被中止/超时。
        let mut stop: Option<&'static str> = None;
        let mut exit_status: Option<std::process::ExitStatus> = None;
        let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
        let mut drain_deadline: Option<tokio::time::Instant> = None;

        loop {
            let exited = exit_status.is_some();
            if stop.is_some() || (exited && !out_open) {
                break;
            }
            // 退出后只再读 DRAIN_GRACE，避免被子孙进程持有的管道挂住。
            if exited && drain_deadline.is_none() {
                drain_deadline = Some(tokio::time::Instant::now() + DRAIN_GRACE);
            }
            // 注意：tokio::select! 的分支表达式是**立即求值**的，`if` 前置
            // 条件只阻止轮询——所以不能把 `deadline.unwrap()` 写进分支里，
            // 用 sleep_opt：None 时永远 pending。
            tokio::select! {
                biased;
                _ = cancel.cancelled(), if stop.is_none() => {
                    stop = Some("aborted");
                }
                _ = sleep_opt(deadline.filter(|_| stop.is_none() && exit_status.is_none())) => {
                    stop = Some("timed out");
                }
                r = stdout.read(&mut chunk_out), if out_open => {
                    match r {
                        Ok(0) | Err(_) => out_open = false,
                        Ok(n) => buf.extend_from_slice(&chunk_out[..n]),
                    }
                }
                s = child.wait(), if exit_status.is_none() => {
                    match s {
                        Ok(status) => exit_status = Some(status),
                        Err(_) => break,
                    }
                }
                _ = sleep_opt(drain_deadline.filter(|_| out_open)) => {
                    break;
                }
            }
        }

        if stop.is_some() {
            kill_group(pid);
            // 回收 shell 自身，避免僵尸进程。
            if exit_status.is_none() {
                let _ = child.wait().await;
            }
        }

        let full = String::from_utf8_lossy(&buf).into_owned();
        let (mut text, notice) = truncate_tail(&full);
        if text.trim().is_empty() {
            text = "(no output)".to_string();
        }
        if let Some(notice) = notice {
            text.push_str("\n\n");
            text.push_str(&notice);
        }
        match stop {
            Some("aborted") => text.push_str("\n\nCommand aborted"),
            Some("timed out") => text.push_str(&format!(
                "\n\nCommand timed out after {} seconds",
                timeout.map(|t| t.as_secs()).unwrap_or(0)
            )),
            Some(other) => text.push_str(&format!("\n\nCommand {other}")),
            None => match exit_status.and_then(|s| s.code()) {
                Some(0) => {}
                Some(code) => text.push_str(&format!("\n\nCommand exited with code {code}")),
                None => text.push_str("\n\nCommand terminated by signal"),
            },
        }
        text
    }
}

/// `Some` = 睡到截止时间；`None` = 永远 pending（select! 分支是立即求值
/// 的，不能用 unwrap 表达"禁用"）。
async fn sleep_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => futures::future::pending().await,
    }
}

/// 杀掉 shell 所在的整个进程组（spawn 时已 `process_group(0)`）。
#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // pgid == pid（组长），负号 = 整组。
        unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {}

/// 尾部截断：保留最后 MAX_LINES 行且不超过 MAX_BYTES。截断时把完整输出
/// 写入临时文件，返回 (尾部内容, 通知文本)。
fn truncate_tail(full: &str) -> (String, Option<String>) {
    let total_lines = full.lines().count();
    if total_lines <= MAX_LINES && full.len() <= MAX_BYTES {
        return (full.to_string(), None);
    }

    // 从尾部往前收行，先撞到哪个上限由哪个说了算。
    let mut tail: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    let mut by_lines = false;
    for line in full.lines().rev() {
        if tail.len() >= MAX_LINES {
            by_lines = true;
            break;
        }
        // +1 for the newline
        if bytes + line.len() + 1 > MAX_BYTES {
            break;
        }
        tail.push(line);
        bytes += line.len() + 1;
    }
    tail.reverse();
    let shown_lines = tail.len();
    let mut content = tail.join("\n");

    // 边界情况：最后一行本身就超过字节上限 → 只留该行的尾部字节。
    let mut last_line_partial = false;
    if shown_lines == 0 {
        let last = full.lines().next_back().unwrap_or("");
        let keep = last.len().min(MAX_BYTES);
        content = last[last.len() - keep..].to_string();
        last_line_partial = true;
    }

    let path = dump_full_output(full);
    let start_line = total_lines.saturating_sub(shown_lines) + 1;
    let notice = if last_line_partial {
        format!(
            "[Showing last {} of line {total_lines} (line exceeds the {}KB limit). Full output: {path}]",
            format_size(content.len()),
            MAX_BYTES / 1024,
        )
    } else if by_lines {
        format!("[Showing lines {start_line}-{total_lines} of {total_lines}. Full output: {path}]")
    } else {
        format!(
            "[Showing lines {start_line}-{total_lines} of {total_lines} ({}KB limit). Full output: {path}]",
            MAX_BYTES / 1024,
        )
    };
    (content, Some(notice))
}

fn dump_full_output(full: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("rua-bash-{}-{nanos}.log", std::process::id()));
    match std::fs::write(&path, full) {
        Ok(()) => path.display().to_string(),
        Err(e) => format!("<failed to save full output: {e}>"),
    }
}

fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> BashTool {
        BashTool::new(std::env::current_dir().unwrap())
    }

    fn no_cancel() -> CancellationToken {
        CancellationToken::new()
    }

    #[tokio::test]
    async fn runs_echo() {
        let out = tool()
            .execute(&serde_json::json!({"command": "echo hello"}), &no_cancel())
            .await;
        assert_eq!(out, "hello\n");
    }

    #[tokio::test]
    async fn merges_stdout_and_stderr_in_order() {
        let out = tool()
            .execute(
                &serde_json::json!({"command": "echo out1; echo err1 >&2; echo out2"}),
                &no_cancel(),
            )
            .await;
        let (o1, e1, o2) = (
            out.find("out1").unwrap(),
            out.find("err1").unwrap(),
            out.find("out2").unwrap(),
        );
        assert!(o1 < e1 && e1 < o2, "interleave broken: {out}");
    }

    #[tokio::test]
    async fn reports_nonzero_exit() {
        let out = tool()
            .execute(&serde_json::json!({"command": "echo oops >&2; exit 3"}), &no_cancel())
            .await;
        assert!(out.contains("oops"), "got: {out}");
        assert!(out.contains("Command exited with code 3"), "got: {out}");
    }

    #[tokio::test]
    async fn empty_output_is_marked() {
        let out = tool()
            .execute(&serde_json::json!({"command": "true"}), &no_cancel())
            .await;
        assert_eq!(out, "(no output)");
    }

    #[tokio::test]
    async fn enforces_timeout_and_kills_process_group() {
        let start = std::time::Instant::now();
        let out = tool()
            .execute(&serde_json::json!({"command": "sleep 30 & sleep 30", "timeout": 1}), &no_cancel())
            .await;
        assert!(out.contains("timed out after 1 seconds"), "got: {out}");
        assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
    }

    #[tokio::test]
    async fn cancellation_aborts() {
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel2.cancel();
        });
        let out = tool()
            .execute(&serde_json::json!({"command": "echo before; sleep 30"}), &cancel)
            .await;
        assert!(out.contains("before"), "got: {out}");
        assert!(out.contains("Command aborted"), "got: {out}");
    }

    #[tokio::test]
    async fn rejects_bad_args() {
        let out = tool().execute(&serde_json::json!({"nope": 1}), &no_cancel()).await;
        assert!(out.contains("invalid bash arguments"), "got: {out}");
    }

    #[tokio::test]
    async fn truncates_huge_single_line_and_dumps_full_output() {
        let out = tool()
            .execute(
                &serde_json::json!({"command": "printf 'x%.0s' {1..300000}"}),
                &no_cancel(),
            )
            .await;
        assert!(out.contains("line exceeds"), "got head: {}", &out[..out.len().min(200)]);
        let path = out
            .split("Full output: ")
            .nth(1)
            .map(|s| s.trim_end_matches(']').trim().to_string())
            .expect("notice carries path");
        let full = std::fs::read_to_string(&path).unwrap();
        assert_eq!(full.trim().len(), 300000);
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn truncates_many_lines_from_the_tail() {
        let out = tool()
            .execute(&serde_json::json!({"command": "seq 1 5000"}), &no_cancel())
            .await;
        assert!(out.contains("of 5000"), "got tail: {}", &out[out.len()-200..]);
        // 尾部保留：最后的行必须在，最早的行必须不在。
        assert!(out.contains("5000"), "got: {out}");
        assert!(!out.contains("\n1\n2\n"), "head leaked: {}", &out[..200]);
    }
}
