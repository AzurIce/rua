use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::conversation::Conversation;
use super::types::{
    DirectoryRevision, DirectorySnapshot, EntryId, HeadRevision, InstructionSet, Message,
    StableHead, ToolCallId,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectoryChangeSource {
    UserCommand,
    ModelTool { tool_call_id: ToolCallId },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionEntryPayload {
    Message(Message),
    CwdChanged {
        input: String,
        from: Option<DirectorySnapshot>,
        to: DirectorySnapshot,
        source: DirectoryChangeSource,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: EntryId,
    pub parent_id: Option<EntryId>,
    pub timestamp_unix_ms: u128,
    pub payload: SessionEntryPayload,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationHead {
    pub entry_id: Option<EntryId>,
    pub revision: HeadRevision,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionTree {
    entries: HashMap<EntryId, SessionEntry>,
    head: ConversationHead,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MaterializedBranch {
    pub conversation: Conversation,
    pub directory: Option<DirectorySnapshot>,
}

impl SessionTree {
    pub fn from_legacy_messages(messages: &[Message]) -> Result<Self, SessionTreeError> {
        let mut tree = Self::default();
        for (index, message) in messages.iter().enumerate() {
            let id = EntryId::new(format!("legacy-entry-{}", index + 1));
            let parent_id = tree.head.entry_id.clone();
            tree.entries.insert(
                id.clone(),
                SessionEntry {
                    id: id.clone(),
                    parent_id,
                    timestamp_unix_ms: 0,
                    payload: SessionEntryPayload::Message(message.clone()),
                },
            );
            tree.head = ConversationHead {
                entry_id: Some(id),
                revision: HeadRevision((index + 1) as u64),
            };
        }
        Ok(tree)
    }

    pub fn entries(&self) -> &HashMap<EntryId, SessionEntry> {
        &self.entries
    }

    pub fn head(&self) -> &ConversationHead {
        &self.head
    }

    pub fn stable_head(&self, directory: &DirectorySnapshot) -> StableHead {
        StableHead {
            entry_id: self.head.entry_id.clone(),
            revision: self.head.revision,
            directory_revision: directory.revision,
        }
    }

    pub fn append(
        &mut self,
        entry: SessionEntry,
        expected_head: &ConversationHead,
        resulting_revision: HeadRevision,
        instructions: &InstructionSet,
        initial_directory: Option<&DirectorySnapshot>,
    ) -> Result<MaterializedBranch, SessionTreeError> {
        if &self.head != expected_head {
            return Err(SessionTreeError::HeadMismatch {
                expected: expected_head.clone(),
                actual: self.head.clone(),
            });
        }
        if entry.id.is_empty() {
            return Err(SessionTreeError::EmptyEntryId);
        }
        if self.entries.contains_key(&entry.id) {
            return Err(SessionTreeError::DuplicateEntry(entry.id));
        }
        if entry.parent_id != expected_head.entry_id {
            return Err(SessionTreeError::ParentMismatch {
                expected: expected_head.entry_id.clone(),
                actual: entry.parent_id,
            });
        }
        if let Some(parent) = &entry.parent_id
            && !self.entries.contains_key(parent)
        {
            return Err(SessionTreeError::MissingEntry(parent.clone()));
        }
        let next_revision = self
            .head
            .revision
            .0
            .checked_add(1)
            .ok_or(SessionTreeError::HeadRevisionOverflow)?;
        if resulting_revision != HeadRevision(next_revision) {
            return Err(SessionTreeError::HeadRevisionMismatch {
                expected: HeadRevision(next_revision),
                actual: resulting_revision,
            });
        }

        let entry_id = entry.id.clone();
        let mut next = self.clone();
        next.entries.insert(entry_id.clone(), entry);
        next.head = ConversationHead {
            entry_id: Some(entry_id),
            revision: resulting_revision,
        };
        let branch = next.materialize(instructions, initial_directory)?;
        *self = next;
        Ok(branch)
    }

    pub fn move_head(
        &mut self,
        expected_head: &ConversationHead,
        target_entry_id: Option<EntryId>,
        resulting_revision: HeadRevision,
        instructions: &InstructionSet,
        initial_directory: Option<&DirectorySnapshot>,
    ) -> Result<MaterializedBranch, SessionTreeError> {
        if &self.head != expected_head {
            return Err(SessionTreeError::HeadMismatch {
                expected: expected_head.clone(),
                actual: self.head.clone(),
            });
        }
        if let Some(target) = &target_entry_id
            && !self.entries.contains_key(target)
        {
            return Err(SessionTreeError::MissingEntry(target.clone()));
        }
        let next_revision = self
            .head
            .revision
            .0
            .checked_add(1)
            .ok_or(SessionTreeError::HeadRevisionOverflow)?;
        if resulting_revision != HeadRevision(next_revision) {
            return Err(SessionTreeError::HeadRevisionMismatch {
                expected: HeadRevision(next_revision),
                actual: resulting_revision,
            });
        }
        let mut next = self.clone();
        next.head = ConversationHead {
            entry_id: target_entry_id,
            revision: resulting_revision,
        };
        let branch = next.materialize(instructions, initial_directory)?;
        *self = next;
        Ok(branch)
    }

    pub fn materialize(
        &self,
        instructions: &InstructionSet,
        initial_directory: Option<&DirectorySnapshot>,
    ) -> Result<MaterializedBranch, SessionTreeError> {
        self.materialize_from(self.head.entry_id.as_ref(), instructions, initial_directory)
    }

    pub fn validate(
        &self,
        instructions: &InstructionSet,
        initial_directory: Option<&DirectorySnapshot>,
    ) -> Result<MaterializedBranch, SessionTreeError> {
        self.validate_structure()?;
        for key in self.entries.keys() {
            self.materialize_from(Some(key), instructions, initial_directory)?;
        }
        self.materialize(instructions, initial_directory)
    }

    pub fn validate_structure(&self) -> Result<(), SessionTreeError> {
        if let Some(head) = &self.head.entry_id
            && !self.entries.contains_key(head)
        {
            return Err(SessionTreeError::MissingEntry(head.clone()));
        }
        for (key, entry) in &self.entries {
            if entry.id.is_empty() {
                return Err(SessionTreeError::EmptyEntryId);
            }
            if key != &entry.id {
                return Err(SessionTreeError::EntryKeyMismatch {
                    key: key.clone(),
                    entry_id: entry.id.clone(),
                });
            }
            let mut visited = HashSet::new();
            let mut cursor = Some(key);
            while let Some(entry_id) = cursor {
                if !visited.insert(entry_id.clone()) {
                    return Err(SessionTreeError::Cycle(entry_id.clone()));
                }
                let current = self
                    .entries
                    .get(entry_id)
                    .ok_or_else(|| SessionTreeError::MissingEntry(entry_id.clone()))?;
                cursor = current.parent_id.as_ref();
            }
        }
        Ok(())
    }

    fn materialize_from(
        &self,
        head: Option<&EntryId>,
        instructions: &InstructionSet,
        initial_directory: Option<&DirectorySnapshot>,
    ) -> Result<MaterializedBranch, SessionTreeError> {
        let mut path = Vec::new();
        let mut visited = HashSet::new();
        let mut cursor = head;
        while let Some(entry_id) = cursor {
            if !visited.insert(entry_id.clone()) {
                return Err(SessionTreeError::Cycle(entry_id.clone()));
            }
            let entry = self
                .entries
                .get(entry_id)
                .ok_or_else(|| SessionTreeError::MissingEntry(entry_id.clone()))?;
            path.push(entry);
            cursor = entry.parent_id.as_ref();
        }
        path.reverse();

        let mut conversation = Conversation::new(instructions.clone());
        let mut directory = initial_directory.cloned();
        for entry in path {
            match &entry.payload {
                SessionEntryPayload::Message(message) => {
                    conversation.append(message.clone()).map_err(|error| {
                        SessionTreeError::InvalidConversation(error.to_string())
                    })?;
                }
                SessionEntryPayload::CwdChanged { from, to, .. } => {
                    if directory != *from {
                        return Err(SessionTreeError::DirectoryTransitionMismatch {
                            expected: directory,
                            actual: from.clone(),
                        });
                    }
                    let expected_revision = match from {
                        Some(from) => from
                            .revision
                            .0
                            .checked_add(1)
                            .ok_or(SessionTreeError::DirectoryRevisionOverflow)?,
                        None => 0,
                    };
                    if to.revision != DirectoryRevision(expected_revision) {
                        return Err(SessionTreeError::DirectoryRevisionMismatch {
                            expected: DirectoryRevision(expected_revision),
                            actual: to.revision,
                        });
                    }
                    directory = Some(to.clone());
                }
            }
        }
        Ok(MaterializedBranch {
            conversation,
            directory,
        })
    }

    pub fn path_to_head(&self) -> Result<Vec<&SessionEntry>, SessionTreeError> {
        let mut path = Vec::new();
        let mut visited = HashSet::new();
        let mut cursor = self.head.entry_id.as_ref();
        while let Some(entry_id) = cursor {
            if !visited.insert(entry_id.clone()) {
                return Err(SessionTreeError::Cycle(entry_id.clone()));
            }
            let entry = self
                .entries
                .get(entry_id)
                .ok_or_else(|| SessionTreeError::MissingEntry(entry_id.clone()))?;
            path.push(entry);
            cursor = entry.parent_id.as_ref();
        }
        path.reverse();
        Ok(path)
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SessionTreeError {
    #[error("session entry id cannot be empty")]
    EmptyEntryId,
    #[error("duplicate session entry: {0}")]
    DuplicateEntry(EntryId),
    #[error("session entry map key {key} does not match payload id {entry_id}")]
    EntryKeyMismatch { key: EntryId, entry_id: EntryId },
    #[error("session entry does not exist: {0}")]
    MissingEntry(EntryId),
    #[error("session tree contains a cycle at {0}")]
    Cycle(EntryId),
    #[error("session head mismatch: expected {expected:?}, got {actual:?}")]
    HeadMismatch {
        expected: ConversationHead,
        actual: ConversationHead,
    },
    #[error("session entry parent mismatch: expected {expected:?}, got {actual:?}")]
    ParentMismatch {
        expected: Option<EntryId>,
        actual: Option<EntryId>,
    },
    #[error("session head revision overflow")]
    HeadRevisionOverflow,
    #[error("session head revision mismatch: expected {expected:?}, got {actual:?}")]
    HeadRevisionMismatch {
        expected: HeadRevision,
        actual: HeadRevision,
    },
    #[error("working directory revision overflow")]
    DirectoryRevisionOverflow,
    #[error("working directory revision mismatch: expected {expected:?}, got {actual:?}")]
    DirectoryRevisionMismatch {
        expected: DirectoryRevision,
        actual: DirectoryRevision,
    },
    #[error("working directory transition mismatch: expected {expected:?}, got {actual:?}")]
    DirectoryTransitionMismatch {
        expected: Option<DirectorySnapshot>,
        actual: Option<DirectorySnapshot>,
    },
    #[error("invalid conversation on session branch: {0}")]
    InvalidConversation(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{MessageId, UserContent, UserMessage};

    fn user_entry(id: &str, parent_id: Option<&str>) -> SessionEntry {
        SessionEntry {
            id: EntryId::new(id),
            parent_id: parent_id.map(EntryId::new),
            timestamp_unix_ms: 0,
            payload: SessionEntryPayload::Message(Message::User(UserMessage {
                id: MessageId::new(format!("message-{id}")),
                content: vec![UserContent::Text {
                    text: id.to_owned(),
                }],
            })),
        }
    }

    #[test]
    fn validation_rejects_a_cycle_on_an_inactive_branch() {
        let mut tree = SessionTree::default();
        tree.entries
            .insert(EntryId::new("a"), user_entry("a", Some("b")));
        tree.entries
            .insert(EntryId::new("b"), user_entry("b", Some("a")));
        let directory = DirectorySnapshot {
            path: std::path::PathBuf::from("/project"),
            revision: DirectoryRevision(0),
        };

        let error = tree
            .validate(&InstructionSet::new("system"), Some(&directory))
            .unwrap_err();

        assert!(matches!(error, SessionTreeError::Cycle(_)));
    }

    #[test]
    fn validation_rejects_an_entry_stored_under_the_wrong_key() {
        let mut tree = SessionTree::default();
        tree.entries
            .insert(EntryId::new("wrong-key"), user_entry("entry-id", None));
        let directory = DirectorySnapshot {
            path: std::path::PathBuf::from("/project"),
            revision: DirectoryRevision(0),
        };

        let error = tree
            .validate(&InstructionSet::new("system"), Some(&directory))
            .unwrap_err();

        assert!(matches!(error, SessionTreeError::EntryKeyMismatch { .. }));
    }
}
