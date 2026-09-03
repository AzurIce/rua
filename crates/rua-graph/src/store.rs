use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::id::NodeId;
use crate::journal::JournalEvent;
use crate::node::TurnLine;

/// On-disk layout for one project's graph:
///
/// ```text
/// <root>/
/// ├── journal.jsonl       结构唯一事实源：node meta + cursor/turn 事件
/// ├── turns/<ulid>.jsonl  Turn 正文：轮内事件流（init/llm_call/tool_exec 逐行追加）
/// └── contexts/<ulid>.md  Context 正文：文本材料（tmp + rename 原子写）
/// ```
///
/// Input 正文内联在 journal header（`Input.text`），不落正文文件。
#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// `root` is the graph directory (e.g. `<project>/.rua/graphs/<name>`).
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("turns"))?;
        fs::create_dir_all(root.join("contexts"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // ---- turn bodies (jsonl event streams) ----

    fn turn_path(&self, id: NodeId) -> PathBuf {
        self.root.join("turns").join(format!("{id}.jsonl"))
    }

    /// Whether the turn body file exists (a sink already appended to it).
    pub fn has_turn_lines(&self, id: NodeId) -> bool {
        self.turn_path(id).exists()
    }

    /// Append one line to `turns/<id>.jsonl` (created on first append),
    /// durability on par with the journal (append + `sync_data`).
    pub fn append_turn_line(&self, id: NodeId, line: &TurnLine) -> Result<()> {
        let mut line_bytes = serde_json::to_vec(line)?;
        line_bytes.push(b'\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.turn_path(id))?;
        file.write_all(&line_bytes)?;
        file.sync_data()?;
        Ok(())
    }

    /// Read all lines of a turn body, tolerating a truncated/corrupt final
    /// line (the daemon may have died mid-append; same policy as the journal).
    /// A missing file reads as an empty body.
    pub fn read_turn_lines(&self, id: NodeId) -> Result<Vec<TurnLine>> {
        let path = self.turn_path(id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = fs::read_to_string(&path)?;
        let mut lines = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(l) => lines.push(l),
                Err(e) => {
                    // Only the last line may be incomplete (crash mid-append).
                    if i + 1 == text.lines().count() {
                        break;
                    }
                    return Err(crate::error::Error::JournalCorrupted {
                        line: i + 1,
                        reason: e.to_string(),
                    });
                }
            }
        }
        Ok(lines)
    }

    // ---- context bodies (text files) ----

    fn context_path(&self, id: NodeId) -> PathBuf {
        self.root.join("contexts").join(format!("{id}.md"))
    }

    /// Persist a context body (tmp + rename atomic write).
    pub fn write_context(&self, id: NodeId, body: &str) -> Result<()> {
        let path = self.context_path(id);
        if path.exists() {
            return Err(crate::error::Error::NodeAlreadyCommitted(id));
        }
        let tmp = self.root.join("contexts").join(format!(".{id}.tmp"));
        fs::write(&tmp, body)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn read_context(&self, id: NodeId) -> Result<String> {
        let path = self.context_path(id);
        if !path.exists() {
            return Err(crate::error::Error::NodeNotFound(id));
        }
        Ok(fs::read_to_string(&path)?)
    }

    // ---- journal ----

    pub fn journal_path(&self) -> PathBuf {
        self.root.join("journal.jsonl")
    }

    /// Append one event line to the journal.
    pub fn append_journal(&self, event: &JournalEvent) -> Result<()> {
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        self.append_journal_bytes(&line)
    }

    /// Append a pre-encoded journal line (commit 路径用：header 直接从
    /// 节点序列化，避免为落盘深拷贝正文)。
    pub fn append_journal_value(&self, value: &serde_json::Value) -> Result<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        self.append_journal_bytes(&line)
    }

    fn append_journal_bytes(&self, line: &[u8]) -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.journal_path())?;
        file.write_all(line)?;
        file.sync_data()?;
        Ok(())
    }

    /// Read all journal events, tolerating a truncated/corrupt final line
    /// (the daemon may have died mid-append).
    pub fn read_journal(&self) -> Result<Vec<JournalEvent>> {
        let path = self.journal_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = fs::read_to_string(&path)?;
        let mut events = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(ev) => events.push(ev),
                Err(e) => {
                    // Only the last line may be incomplete (crash mid-append).
                    if i + 1 == text.lines().count() {
                        break;
                    }
                    return Err(crate::error::Error::JournalCorrupted {
                        line: i + 1,
                        reason: e.to_string(),
                    });
                }
            }
        }
        Ok(events)
    }
}
