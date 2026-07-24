use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::types::{
    ConversationRevision, InstructionSet, Message, MessageId, StopReason, ToolCallId,
    ToolResultContent, UserContent,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    instructions: InstructionSet,
    messages: Vec<Message>,
    revision: ConversationRevision,
}

impl Conversation {
    pub fn new(instructions: InstructionSet) -> Self {
        Self {
            instructions,
            messages: Vec::new(),
            revision: ConversationRevision::default(),
        }
    }

    pub fn instructions(&self) -> &InstructionSet {
        &self.instructions
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn revision(&self) -> ConversationRevision {
        self.revision
    }

    pub fn append(&mut self, message: Message) -> Result<ConversationRevision, ConversationError> {
        self.validate_next(&message)?;
        let next_revision = self
            .revision
            .0
            .checked_add(1)
            .ok_or(ConversationError::RevisionOverflow)?;
        self.messages.push(message);
        self.revision.0 = next_revision;
        Ok(self.revision)
    }

    fn validate_next(&self, message: &Message) -> Result<(), ConversationError> {
        if message.id().is_empty() {
            return Err(ConversationError::EmptyMessageId);
        }
        if self.messages.iter().any(|item| item.id() == message.id()) {
            return Err(ConversationError::DuplicateMessageId(message.id().clone()));
        }

        let mut calls: HashMap<&ToolCallId, &str> = HashMap::new();
        let mut results = HashSet::new();
        for item in &self.messages {
            match item {
                Message::Assistant(assistant) => {
                    for call in assistant.tool_calls() {
                        calls.insert(&call.id, call.name.as_str());
                    }
                }
                Message::ToolResult(result) => {
                    results.insert(&result.tool_call_id);
                }
                Message::User(_) => {}
            }
        }

        match message {
            Message::User(user) => {
                if user.content.is_empty()
                    || user.content.iter().all(|part| match part {
                        UserContent::Text { text } => text.is_empty(),
                    })
                {
                    return Err(ConversationError::EmptyUserContent);
                }
                if calls.keys().any(|id| !results.contains(id)) {
                    return Err(ConversationError::PendingToolResults);
                }
            }
            Message::Assistant(assistant) => {
                if calls.keys().any(|id| !results.contains(id)) {
                    return Err(ConversationError::PendingToolResults);
                }
                let assistant_tool_calls = assistant.tool_calls().count();
                match assistant.stop_reason {
                    StopReason::ToolUse if assistant_tool_calls == 0 => {
                        return Err(ConversationError::ToolUseWithoutCalls);
                    }
                    StopReason::ToolUse => {}
                    _ if assistant_tool_calls > 0 => {
                        return Err(ConversationError::ToolCallsWithoutToolUseStop);
                    }
                    _ => {}
                }
                for call in assistant.tool_calls() {
                    if call.id.is_empty() {
                        return Err(ConversationError::EmptyToolCallId);
                    }
                    if call.name.is_empty() {
                        return Err(ConversationError::EmptyToolName(call.id.clone()));
                    }
                    if calls.contains_key(&call.id) {
                        return Err(ConversationError::DuplicateToolCallId(call.id.clone()));
                    }
                    calls.insert(&call.id, call.name.as_str());
                }
            }
            Message::ToolResult(result) => {
                if result.content.is_empty()
                    || result.content.iter().all(|part| match part {
                        ToolResultContent::Text { text } => text.is_empty(),
                    })
                {
                    return Err(ConversationError::EmptyToolResultContent);
                }
                let Some(expected_name) = calls.get(&result.tool_call_id) else {
                    return Err(ConversationError::UnknownToolCall(
                        result.tool_call_id.clone(),
                    ));
                };
                if results.contains(&result.tool_call_id) {
                    return Err(ConversationError::DuplicateToolResult(
                        result.tool_call_id.clone(),
                    ));
                }
                if *expected_name != result.name {
                    return Err(ConversationError::ToolNameMismatch {
                        call_id: result.tool_call_id.clone(),
                        expected: (*expected_name).to_owned(),
                        actual: result.name.clone(),
                    });
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversationError {
    #[error("message id cannot be empty")]
    EmptyMessageId,
    #[error("message id already exists: {0}")]
    DuplicateMessageId(MessageId),
    #[error("user message content cannot be empty")]
    EmptyUserContent,
    #[error("tool result content cannot be empty")]
    EmptyToolResultContent,
    #[error("a new user message cannot be appended while tool results are pending")]
    PendingToolResults,
    #[error("tool call id cannot be empty")]
    EmptyToolCallId,
    #[error("tool call {0} has an empty name")]
    EmptyToolName(ToolCallId),
    #[error("tool call id already exists: {0}")]
    DuplicateToolCallId(ToolCallId),
    #[error("tool-use stop reason requires at least one tool call")]
    ToolUseWithoutCalls,
    #[error("a message containing tool calls must use the tool-use stop reason")]
    ToolCallsWithoutToolUseStop,
    #[error("tool result references an unknown call: {0}")]
    UnknownToolCall(ToolCallId),
    #[error("tool result already exists for call: {0}")]
    DuplicateToolResult(ToolCallId),
    #[error("tool result name mismatch for {call_id}: expected {expected}, got {actual}")]
    ToolNameMismatch {
        call_id: ToolCallId,
        expected: String,
        actual: String,
    },
    #[error("conversation revision overflow")]
    RevisionOverflow,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::types::{
        ApiFamily, AssistantMessage, AssistantPart, ModelRef, ProviderId, ResponseProvenance,
        ToolCall, ToolResultMessage, UserMessage,
    };

    fn user(id: &str, text: &str) -> Message {
        Message::User(UserMessage {
            id: id.into(),
            content: vec![UserContent::Text {
                text: text.to_owned(),
            }],
        })
    }

    fn tool_request(message_id: &str, call_id: &str) -> Message {
        Message::Assistant(Box::new(AssistantMessage {
            id: message_id.into(),
            parts: vec![AssistantPart::ToolCall(ToolCall {
                id: call_id.into(),
                name: "read".to_owned(),
                arguments: json!({ "path": "README.md" }),
                provider_state: None,
            })],
            stop_reason: StopReason::ToolUse,
            usage: None,
            provenance: ResponseProvenance {
                requested: ModelRef {
                    provider: ProviderId::new("fake"),
                    api_family: ApiFamily::new("test"),
                    model: "test-model".to_owned(),
                },
                response_model: None,
                response_id: None,
            },
            provider_state: None,
        }))
    }

    fn tool_result(message_id: &str, call_id: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            id: message_id.into(),
            tool_call_id: call_id.into(),
            name: "read".to_owned(),
            content: vec![ToolResultContent::Text {
                text: "contents".to_owned(),
            }],
            is_error: false,
        })
    }

    #[test]
    fn appends_a_complete_tool_exchange_without_losing_identity() {
        let mut conversation = Conversation::new(InstructionSet::new("system"));

        conversation.append(user("m1", "read it")).unwrap();
        conversation.append(tool_request("m2", "call-1")).unwrap();
        conversation.append(tool_result("m3", "call-1")).unwrap();

        assert_eq!(conversation.revision(), ConversationRevision(3));
        assert_eq!(conversation.messages().len(), 3);
        let Message::ToolResult(result) = &conversation.messages()[2] else {
            panic!("expected tool result");
        };
        assert_eq!(result.tool_call_id.as_str(), "call-1");
    }

    #[test]
    fn rejects_new_input_while_a_tool_result_is_pending() {
        let mut conversation = Conversation::new(InstructionSet::new("system"));
        conversation.append(user("m1", "read it")).unwrap();
        conversation.append(tool_request("m2", "call-1")).unwrap();

        let error = conversation.append(user("m3", "another request"));

        assert_eq!(error, Err(ConversationError::PendingToolResults));
        assert_eq!(conversation.revision(), ConversationRevision(2));
        assert_eq!(conversation.messages().len(), 2);
    }

    #[test]
    fn rejects_orphaned_and_duplicate_tool_results() {
        let mut conversation = Conversation::new(InstructionSet::new("system"));
        assert_eq!(
            conversation.append(tool_result("m1", "missing")),
            Err(ConversationError::UnknownToolCall("missing".into()))
        );

        conversation.append(user("m1", "read it")).unwrap();
        conversation.append(tool_request("m2", "call-1")).unwrap();
        conversation.append(tool_result("m3", "call-1")).unwrap();

        assert_eq!(
            conversation.append(tool_result("m4", "call-1")),
            Err(ConversationError::DuplicateToolResult("call-1".into()))
        );
    }

    #[test]
    fn rejects_tool_calls_with_a_non_tool_stop_reason() {
        let mut conversation = Conversation::new(InstructionSet::new("system"));
        let Message::Assistant(mut assistant) = tool_request("m1", "call-1") else {
            unreachable!();
        };
        assistant.stop_reason = StopReason::EndTurn;

        assert_eq!(
            conversation.append(Message::Assistant(assistant)),
            Err(ConversationError::ToolCallsWithoutToolUseStop)
        );
    }
}
