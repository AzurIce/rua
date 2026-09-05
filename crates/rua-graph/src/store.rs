use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::journal::JournalEvent;

/// On-disk layout for one project's graph:
///
/// ```text
/// <root>/
/// ├── journal.jsonl       结构唯一事实源：node meta + cursor/turn 事件
/// ├── turns/<ulid>.jsonl  Turn 正文：轮内事件流（init/llm_call/tool_exec 逐行追加）
/// └── contexts/<ulid>.md  Context 正文：文本材料（tmp + rename 原子写）
/// ```
///
/// 本类型只管 journal 的 IO 与根目录；正文的文件知识（路径构造、读、写）
/// 内聚在各 kind 的 `Data` 实现里（`turns/`、`contexts/` 是它们的地盘）。
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
    /// 节点序列化)。
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
