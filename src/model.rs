/// One entry in the chat history (domain model)
#[derive(Debug, Clone)]
pub struct ChatEntry {
    pub role: Role,
    pub text: String,
    /// For tool result messages, the matching tool_call id.
    pub tool_call_id: Option<String>,
    /// DeepSeek reasoning models produce this; must be passed back in subsequent requests.
    pub reasoning_content: Option<String>,
    /// Whether reasoning content is expanded in the UI.
    pub reasoning_expanded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

impl ChatEntry {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            tool_call_id: None,
            reasoning_content: None,
            reasoning_expanded: false,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
            tool_call_id: None,
            reasoning_content: None,
            reasoning_expanded: false,
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            text: text.into(),
            tool_call_id: None,
            reasoning_content: None,
            reasoning_expanded: false,
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            text: text.into(),
            tool_call_id: Some(tool_call_id.into()),
            reasoning_content: None,
            reasoning_expanded: false,
        }
    }
}
