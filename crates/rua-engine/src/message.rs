//! Mapping between core's provider-agnostic `CoreMessage` IR and rig's
//! `Message` types.

use rig_core::completion::message::{AssistantContent, Message};
use rua_graph::message::CoreMessage;

/// Render one `CoreMessage` as a rig `Message`.
pub fn to_rig(msg: &CoreMessage) -> Message {
    match msg {
        CoreMessage::System { content } => Message::system(content.clone()),
        CoreMessage::User { content } => Message::user(content.clone()),
        CoreMessage::Assistant {
            content,
            tool_calls,
        } => {
            // Always carry a text block: an empty content list is rejected at
            // rig's request boundary, and DeepSeek wants an explicit (possibly
            // empty) `content` field on tool-call-only assistant turns.
            let mut parts = vec![AssistantContent::text(content.clone())];
            parts.extend(tool_calls.iter().map(|call| {
                AssistantContent::tool_call(
                    call.id.clone(),
                    call.name.clone(),
                    call.args.clone(),
                )
            }));
            Message::Assistant {
                id: None,
                content: parts,
            }
        }
        CoreMessage::ToolResult {
            call_id,
            name,
            output,
        } => Message::tool_result(call_id.clone(), name.clone(), output.clone()),
        CoreMessage::Context { body, sources } => {
            let sources = sources
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            Message::user(format!(
                "<context sources=\"{sources}\">\n{body}\n</context>"
            ))
        }
    }
}

/// Map a whole history.
pub fn history_to_rig(history: &[CoreMessage]) -> Vec<Message> {
    history.iter().map(to_rig).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::message::{ToolResultContent, UserContent};
    use rua_graph::id::NodeId;
    use rua_graph::message::CoreToolCall;

    #[test]
    fn maps_system_and_user() {
        assert_eq!(
            to_rig(&CoreMessage::System {
                content: "sys".into()
            }),
            Message::system("sys")
        );
        assert_eq!(
            to_rig(&CoreMessage::User {
                content: "hi".into()
            }),
            Message::user("hi")
        );
    }

    #[test]
    fn maps_assistant_with_tool_calls() {
        let msg = CoreMessage::Assistant {
            content: "let me run that".into(),
            tool_calls: vec![CoreToolCall {
                id: "call_1".into(),
                name: "bash".into(),
                args: serde_json::json!({"command": "ls"}),
            }],
        };
        let Message::Assistant { id, content } = to_rig(&msg) else {
            panic!("expected assistant message");
        };
        assert_eq!(id, None);
        assert_eq!(content.len(), 2);
        assert!(matches!(&content[0], AssistantContent::Text(t) if t.text == "let me run that"));
        let AssistantContent::ToolCall(call) = &content[1] else {
            panic!("expected tool call content");
        };
        assert_eq!(call.function.name, "bash");
        assert_eq!(call.function.arguments, serde_json::json!({"command": "ls"}));
        // The provider-issued id survives as the wire id.
        assert_eq!(call.wire_call_id(), "call_1");
    }

    #[test]
    fn maps_assistant_text_only() {
        let Message::Assistant { content, .. } = to_rig(&CoreMessage::Assistant {
            content: "done".into(),
            tool_calls: vec![],
        })
        else {
            panic!("expected assistant message");
        };
        assert_eq!(content.len(), 1);
    }

    #[test]
    fn maps_tool_result() {
        let Message::User { content } = to_rig(&CoreMessage::ToolResult {
            call_id: "call_1".into(),
            name: "bash".into(),
            output: "ok".into(),
        }) else {
            panic!("expected user message");
        };
        let [UserContent::ToolResult(result)] = content.as_slice() else {
            panic!("expected single tool result");
        };
        assert_eq!(result.call.as_str(), "call_1");
        assert_eq!(result.name, "bash");
        assert!(
            matches!(&result.content[0], ToolResultContent::Text(t) if t.text == "ok"),
            "tool result carries the output text"
        );
    }

    #[test]
    fn maps_context_with_provenance_header() {
        let src = NodeId::new();
        let Message::User { content } = to_rig(&CoreMessage::Context {
            body: "distilled material".into(),
            sources: vec![src],
        }) else {
            panic!("expected user message");
        };
        let [UserContent::Text(text)] = content.as_slice() else {
            panic!("expected single text block");
        };
        assert!(
            text.text
                .starts_with(&format!("<context sources=\"{src}\">"))
        );
        assert!(text.text.contains("distilled material"));
        assert!(text.text.ends_with("</context>"));
    }
}
