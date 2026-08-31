use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::id::NodeId;
use crate::journal::JournalEvent;
use crate::node::Node;

/// On-disk layout for one project's graph:
///
/// ```text
/// <root>/
/// ├── nodes/<ulid>.json   immutable node bodies (temp + rename atomic write)
/// └── journal.jsonl       control-plane events + node meta index
/// ```
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// `root` is the graph directory (e.g. `<project>/.rua/graph`).
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("nodes"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn node_path(&self, id: NodeId) -> PathBuf {
        self.root.join("nodes").join(format!("{id}.json"))
    }

    /// Persist an immutable node body. Fails if the file already exists.
    pub fn write_node(&self, node: &Node) -> Result<()> {
        let path = self.node_path(node.id);
        if path.exists() {
            return Err(crate::error::Error::NodeAlreadyCommitted(node.id));
        }
        let tmp = self.root.join("nodes").join(format!(".{}.tmp", node.id));
        let json = serde_json::to_vec_pretty(node)?;
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn read_node(&self, id: NodeId) -> Result<Node> {
        let path = self.node_path(id);
        if !path.exists() {
            return Err(crate::error::Error::NodeNotFound(id));
        }
        let bytes = fs::read(&path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn journal_path(&self) -> PathBuf {
        self.root.join("journal.jsonl")
    }

    /// Append one event line to the journal.
    pub fn append_journal(&self, event: &JournalEvent) -> Result<()> {
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.journal_path())?;
        file.write_all(&line)?;
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
