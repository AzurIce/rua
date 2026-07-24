use std::collections::VecDeque;

use crate::agent::{AssistantPart, Message, RuntimeEvent, ToolResultContent, UserContent};
use crate::model::{ChatEntry, Role};
use crate::tui::Composer;

// === opencode-inspired theme ===
pub(crate) const BG: ratatui::style::Color = ratatui::style::Color::Rgb(10, 10, 10);
pub(crate) const BG_PANEL: ratatui::style::Color = ratatui::style::Color::Rgb(20, 20, 20);
pub(crate) const TEXT: ratatui::style::Color = ratatui::style::Color::Rgb(238, 238, 238);
pub(crate) const TEXT_MUTED: ratatui::style::Color = ratatui::style::Color::Rgb(128, 128, 128);
pub(crate) const BORDER: ratatui::style::Color = ratatui::style::Color::Rgb(72, 72, 72);
pub(crate) const PRIMARY: ratatui::style::Color = ratatui::style::Color::Rgb(250, 178, 131);
pub(crate) const USER_ACCENT: ratatui::style::Color = ratatui::style::Color::Rgb(92, 156, 245);
pub(crate) const AI_ACCENT: ratatui::style::Color = ratatui::style::Color::Rgb(159, 124, 216);
pub(crate) const SYSTEM_ACCENT: ratatui::style::Color = ratatui::style::Color::Rgb(128, 128, 128);
pub(crate) const SUCCESS: ratatui::style::Color = ratatui::style::Color::Rgb(127, 216, 143);

pub(crate) const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppStatus {
    Idle,
    Sending,
    Waiting,
    Streaming,
}

impl AppStatus {
    pub fn label(&self) -> &'static str {
        match self {
            AppStatus::Idle => "idle",
            AppStatus::Sending => "sending",
            AppStatus::Waiting => "thinking",
            AppStatus::Streaming => "receiving",
        }
    }

    pub fn icon_char(&self, spinner_frame: char) -> char {
        match self {
            AppStatus::Idle => '◆',
            AppStatus::Sending => '↑',
            AppStatus::Waiting => spinner_frame,
            AppStatus::Streaming => '↓',
        }
    }

    pub fn color(&self) -> ratatui::style::Color {
        match self {
            AppStatus::Idle => TEXT_MUTED,
            AppStatus::Sending => PRIMARY,
            AppStatus::Waiting => AI_ACCENT,
            AppStatus::Streaming => SUCCESS,
        }
    }
}

/// The main application state (pure data + state transitions)
pub struct AppState {
    pub composer: Composer,
    pub(crate) history: VecDeque<ChatEntry>,
    pub current_response: String,
    /// Accumulates reasoning_content from DeepSeek reasoning models.
    pub current_reasoning: String,
    pub is_streaming: bool,
    pub scroll_offset: u16,
    pub should_quit: bool,
    pub status: AppStatus,
    pub spinner_frame: usize,
    pub token_count: usize,
    pub can_resume_turn: bool,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            composer: Composer::new(),
            history: VecDeque::new(),
            current_response: String::new(),
            current_reasoning: String::new(),
            is_streaming: false,
            scroll_offset: 0,
            should_quit: false,
            status: AppStatus::Idle,
            spinner_frame: 0,
            token_count: 0,
            can_resume_turn: false,
        }
    }

    pub fn add_system_message(&mut self, text: &str) {
        self.history.push_back(ChatEntry {
            role: Role::System,
            text: text.to_string(),
            tool_call_id: None,
            reasoning_content: None,
            reasoning_expanded: false,
        });
    }

    pub fn submit_input(&mut self) -> Option<String> {
        let text = self.composer.text().trim().to_string();
        if text.is_empty() {
            return None;
        }
        self.composer.clear();
        self.current_response.clear();
        self.current_reasoning.clear();
        self.is_streaming = true;
        self.status = AppStatus::Sending;
        self.token_count = 0;
        Some(text)
    }

    pub fn begin_resume(&mut self) {
        self.can_resume_turn = false;
        self.current_response.clear();
        self.current_reasoning.clear();
        self.is_streaming = true;
        self.status = AppStatus::Waiting;
    }

    pub fn apply_runtime_event(&mut self, event: &RuntimeEvent) {
        match event {
            RuntimeEvent::SessionRecovered {
                session_id,
                conversation,
            } => {
                self.history.clear();
                self.add_system_message(&format!("recovered session {session_id}"));
                for message in conversation.messages() {
                    match message {
                        Message::User(message) => {
                            for part in &message.content {
                                match part {
                                    UserContent::Text { text } => {
                                        self.history.push_back(ChatEntry {
                                            role: Role::User,
                                            text: text.clone(),
                                            tool_call_id: None,
                                            reasoning_content: None,
                                            reasoning_expanded: false,
                                        })
                                    }
                                }
                            }
                        }
                        Message::Assistant(message) => {
                            let mut text = String::new();
                            let mut reasoning = String::new();
                            for part in &message.parts {
                                match part {
                                    AssistantPart::Text(part) => text.push_str(&part.text),
                                    AssistantPart::Reasoning(part) => {
                                        if let Some(value) = &part.text {
                                            reasoning.push_str(value);
                                        }
                                    }
                                    AssistantPart::ToolCall(call) => {
                                        self.add_tool_call(&call.name, &call.arguments.to_string())
                                    }
                                }
                            }
                            if !text.trim().is_empty() {
                                self.history.push_back(ChatEntry {
                                    role: Role::Assistant,
                                    text: text.trim().to_owned(),
                                    tool_call_id: None,
                                    reasoning_content: (!reasoning.trim().is_empty())
                                        .then(|| reasoning.trim().to_owned()),
                                    reasoning_expanded: false,
                                });
                            }
                        }
                        Message::ToolResult(message) => {
                            let text = message
                                .content
                                .iter()
                                .map(|part| match part {
                                    ToolResultContent::Text { text } => text.as_str(),
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            self.add_tool_result(&message.name, &text);
                        }
                    }
                }
                self.finish_stream();
            }
            RuntimeEvent::RecoveryRequired {
                tool_call_id,
                message,
                ..
            } => {
                self.is_streaming = false;
                self.status = AppStatus::Idle;
                self.current_response.clear();
                self.current_reasoning.clear();
                self.can_resume_turn = false;
                self.add_system_message(&format!(
                    "Recovery required for {tool_call_id}: {message}. Use /recovery inspect."
                ));
            }
            RuntimeEvent::ApprovalRequired {
                tool_call_id,
                name,
                arguments,
                replay_class,
                ..
            } => {
                self.is_streaming = false;
                self.status = AppStatus::Idle;
                self.can_resume_turn = false;
                self.add_system_message(&format!(
                    "Approval required for {tool_call_id}: {name}({arguments}) replay={replay_class:?}. Use /approval approve {tool_call_id} or /approval reject {tool_call_id} [reason]."
                ));
            }
            RuntimeEvent::PersistenceFailed { message } => {
                self.can_resume_turn = false;
                self.add_error(&format!("Persistence failed: {message}"));
            }
            RuntimeEvent::OperationFailed { message } => {
                self.is_streaming = false;
                self.status = AppStatus::Idle;
                self.can_resume_turn = false;
                self.add_error(message);
            }
            RuntimeEvent::TurnStarted { .. } => {
                self.is_streaming = true;
                self.status = AppStatus::Sending;
                self.can_resume_turn = false;
            }
            RuntimeEvent::UserCommitted { text, .. } => {
                self.history.push_back(ChatEntry {
                    role: Role::User,
                    text: text.clone(),
                    tool_call_id: None,
                    reasoning_content: None,
                    reasoning_expanded: false,
                });
                self.is_streaming = true;
                self.status = AppStatus::Sending;
                self.can_resume_turn = false;
            }
            RuntimeEvent::ModelStepStarted { .. } => {
                self.current_response.clear();
                self.current_reasoning.clear();
                self.status = AppStatus::Waiting;
            }
            RuntimeEvent::TextDelta { delta, .. } => self.append_delta(delta),
            RuntimeEvent::ReasoningDelta { delta, .. } => self.append_reasoning_delta(delta),
            RuntimeEvent::ModelStepRetrying { .. } => {
                self.current_response.clear();
                self.current_reasoning.clear();
                self.status = AppStatus::Waiting;
            }
            RuntimeEvent::AssistantCommitted { message, .. } => {
                let mut text = String::new();
                let mut reasoning = String::new();
                for part in &message.parts {
                    match part {
                        AssistantPart::Text(part) => text.push_str(&part.text),
                        AssistantPart::Reasoning(part) => {
                            if let Some(value) = &part.text {
                                reasoning.push_str(value);
                            }
                        }
                        AssistantPart::ToolCall(_) => {}
                    }
                }
                if !text.trim().is_empty() {
                    self.history.push_back(ChatEntry {
                        role: Role::Assistant,
                        text: text.trim().to_owned(),
                        tool_call_id: None,
                        reasoning_content: (!reasoning.trim().is_empty())
                            .then(|| reasoning.trim().to_owned()),
                        reasoning_expanded: false,
                    });
                }
                self.current_response.clear();
                self.current_reasoning.clear();
                self.status = AppStatus::Waiting;
            }
            RuntimeEvent::ToolStarted {
                name, arguments, ..
            } => {
                self.add_tool_call(name, &arguments.to_string());
            }
            RuntimeEvent::ToolCompleted { content, .. } => self.add_tool_result("", content),
            RuntimeEvent::ToolFailed { message, .. } => {
                self.add_tool_result("", &format!("Error: {message}"));
            }
            RuntimeEvent::ToolRejected { message, .. } => {
                self.add_tool_result("", &format!("Rejected: {message}"));
            }
            RuntimeEvent::TurnCompleted { .. } => self.finish_stream(),
            RuntimeEvent::TurnFailed {
                error, recoverable, ..
            } => {
                self.current_response.clear();
                self.current_reasoning.clear();
                self.is_streaming = false;
                self.status = AppStatus::Idle;
                self.token_count = 0;
                self.can_resume_turn = *recoverable;
                self.history.push_back(ChatEntry {
                    role: Role::System,
                    text: format!("Error: {error}"),
                    tool_call_id: None,
                    reasoning_content: None,
                    reasoning_expanded: false,
                });
            }
            RuntimeEvent::TurnCancelled { .. } => self.finish_stream(),
        }
    }

    pub fn add_tool_call(&mut self, name: &str, arguments: &str) {
        self.history.push_back(ChatEntry {
            role: Role::Tool,
            text: format!("{}({})", name, arguments),
            tool_call_id: None,
            reasoning_content: None,
            reasoning_expanded: false,
        });
    }

    pub fn add_tool_result(&mut self, _name: &str, output: &str) {
        self.history.push_back(ChatEntry {
            role: Role::Tool,
            text: format!("→ {}", output),
            tool_call_id: None,
            reasoning_content: None,
            reasoning_expanded: false,
        });
    }

    pub fn append_delta(&mut self, delta: &str) {
        self.current_response.push_str(delta);
        self.status = AppStatus::Streaming;
        self.token_count = self.current_response.len() / 4;
    }

    pub fn append_reasoning_delta(&mut self, delta: &str) {
        self.current_reasoning.push_str(delta);
    }

    pub fn finish_stream(&mut self) {
        let text = self.current_response.trim().to_string();
        let reasoning = if self.current_reasoning.is_empty() {
            None
        } else {
            Some(self.current_reasoning.trim().to_string())
        };
        if !text.is_empty() {
            self.history.push_back(ChatEntry {
                role: Role::Assistant,
                text,
                tool_call_id: None,
                reasoning_content: reasoning,
                reasoning_expanded: false,
            });
        }
        self.current_response.clear();
        self.current_reasoning.clear();
        self.is_streaming = false;
        self.status = AppStatus::Idle;
        self.token_count = 0;
    }

    pub fn add_error(&mut self, text: &str) {
        self.is_streaming = false;
        self.status = AppStatus::Idle;
        self.token_count = 0;
        self.history.push_back(ChatEntry {
            role: Role::System,
            text: format!("Error: {}", text),
            tool_call_id: None,
            reasoning_content: None,
            reasoning_expanded: false,
        });
    }

    /// Toggle the reasoning expansion state of the most recent Assistant entry.
    pub fn toggle_latest_reasoning(&mut self) {
        for entry in self.history.iter_mut().rev() {
            if entry.role == Role::Assistant && entry.reasoning_content.is_some() {
                entry.reasoning_expanded = !entry.reasoning_expanded;
                break;
            }
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------------
// Text helpers (used by render)
// ------------------------------------------------------------------

/// Wrap a single line of text to a maximum display width (in columns).
pub(crate) fn wrap_line(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;

    for word in text.split_whitespace() {
        let word_width = unicode_width::UnicodeWidthStr::width(word);

        if current_width > 0 {
            if current_width + 1 + word_width <= max_width {
                current.push(' ');
                current.push_str(word);
                current_width += 1 + word_width;
                continue;
            }
            lines.push(current);
            current = String::new();
            current_width = 0;
        }

        if word_width <= max_width {
            current = word.to_string();
            current_width = word_width;
        } else {
            for c in word.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if current_width + cw > max_width && !current.is_empty() {
                    lines.push(current);
                    current = String::new();
                    current_width = 0;
                }
                current.push(c);
                current_width += cw;
            }
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }

    lines
}

/// Wrap a multi-paragraph text, preserving blank lines.
pub(crate) fn wrap_paragraph(text: &str, max_width: usize) -> Vec<String> {
    let mut all = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            all.push(String::new());
            continue;
        }
        all.extend(wrap_line(para, max_width));
    }
    all
}
