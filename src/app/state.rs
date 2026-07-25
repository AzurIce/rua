use std::collections::VecDeque;

use crate::agent::{AssistantPart, Message, RuntimeEvent, ToolResultContent, UserContent};
use crate::app::command::CommandContext;
use crate::app::command::{
    CommandAssist, CommandRegistry, CompletionContext, CompletionItem, CompletionRequest,
    CompletionResponse,
};
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
    pub command_assist: CommandAssistState,
    approval_tool_calls: Vec<String>,
    recovery_tool_calls: Vec<String>,
    session_ids: Vec<String>,
    prompt_history: Vec<String>,
    command_history: Vec<String>,
    prompt_history_cursor: Option<usize>,
    command_history_cursor: Option<usize>,
    command_context_revision: u64,
}

#[derive(Debug, Clone, Default)]
pub struct CommandAssistState {
    pub candidates: Vec<CompletionItem>,
    pub selected: usize,
    pub usage: Option<String>,
    pub diagnostic: Option<String>,
    pub open: bool,
    pub active_request: Option<CompletionRequest>,
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
            command_assist: CommandAssistState::default(),
            approval_tool_calls: Vec::new(),
            recovery_tool_calls: Vec::new(),
            session_ids: Vec::new(),
            prompt_history: Vec::new(),
            command_history: Vec::new(),
            prompt_history_cursor: None,
            command_history_cursor: None,
            command_context_revision: 0,
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
        let text = self.composer.text().to_owned();
        self.submit_text(text)
    }

    pub fn submit_text(&mut self, text: String) -> Option<String> {
        if text.is_empty() {
            return None;
        }
        if text.trim().is_empty() {
            return None;
        }
        self.prompt_history.push(text.clone());
        self.prompt_history_cursor = None;
        self.composer.clear();
        self.command_assist = CommandAssistState::default();
        self.current_response.clear();
        self.current_reasoning.clear();
        self.is_streaming = true;
        self.status = AppStatus::Sending;
        self.token_count = 0;
        Some(text)
    }

    pub fn refresh_command_assist(&mut self, registry: &CommandRegistry) {
        let previous = self
            .command_assist
            .candidates
            .get(self.command_assist.selected)
            .map(|candidate| candidate.label.clone());
        let active_request = self.command_assist.active_request.clone();
        let CommandAssist {
            candidates,
            usage,
            diagnostic,
        } = registry.assist(
            self.composer.text(),
            self.composer.cursor(),
            CompletionContext {
                approval_tool_calls: &self.approval_tool_calls,
                recovery_tool_calls: &self.recovery_tool_calls,
                session_ids: &self.session_ids,
            },
        );
        let selected = previous
            .as_ref()
            .and_then(|label| {
                candidates
                    .iter()
                    .position(|candidate| &candidate.label == label)
            })
            .unwrap_or(0);
        self.command_assist = CommandAssistState {
            open: !candidates.is_empty(),
            candidates,
            selected,
            usage,
            diagnostic,
            active_request,
        };
    }

    pub fn close_command_assist(&mut self) {
        self.command_assist.open = false;
        self.command_assist.active_request = None;
    }

    pub fn begin_session_completion(&mut self) -> Option<CompletionRequest> {
        let prefix = "/session load ";
        if !self.composer.text().starts_with(prefix) || self.composer.cursor() < prefix.len() {
            self.command_assist.active_request = None;
            return None;
        }
        let cursor = self.composer.cursor();
        let start = self.composer.text()[..cursor]
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace())
            .map_or(prefix.len(), |(index, character)| {
                index + character.len_utf8()
            });
        let request_id = self
            .command_context_revision
            .wrapping_add(self.composer.revision())
            .wrapping_add(cursor as u64);
        let request = CompletionRequest {
            request_id,
            draft_revision: self.composer.revision(),
            cursor,
            context_revision: self.command_context_revision,
            query: self.composer.text()[start..cursor].to_owned(),
            replacement_range: start..cursor,
        };
        self.command_assist.active_request = Some(request.clone());
        Some(request)
    }

    pub fn apply_completion_response(&mut self, response: CompletionResponse) {
        let Some(request) = self.command_assist.active_request.as_ref() else {
            return;
        };
        if request.request_id != response.request_id
            || response.draft_revision != self.composer.revision()
            || response.cursor != self.composer.cursor()
            || response.context_revision != self.command_context_revision
        {
            return;
        }
        self.command_assist.active_request = None;
        if let Some(error) = response.error {
            self.command_assist.diagnostic = Some(error);
            return;
        }
        self.command_assist.candidates = response.candidates;
        self.command_assist.selected = 0;
        self.command_assist.open = !self.command_assist.candidates.is_empty();
    }

    pub fn set_command_diagnostic(&mut self, diagnostic: impl Into<String>) {
        self.command_assist.diagnostic = Some(diagnostic.into());
        self.command_assist.open = false;
    }

    pub fn move_command_selection(&mut self, delta: isize) {
        let len = self.command_assist.candidates.len();
        if len == 0 {
            return;
        }
        self.command_assist.selected = if delta.is_negative() {
            self.command_assist
                .selected
                .checked_sub(delta.unsigned_abs())
                .unwrap_or(len - 1)
        } else {
            (self.command_assist.selected + delta as usize) % len
        };
    }

    pub fn accept_selected_completion(&mut self, registry: &CommandRegistry) -> bool {
        let Some(candidate) = self
            .command_assist
            .candidates
            .get(self.command_assist.selected)
            .cloned()
        else {
            return false;
        };
        if !self
            .composer
            .replace_range(candidate.replacement_range, &candidate.replacement)
        {
            return false;
        }
        self.refresh_command_assist(registry);
        true
    }

    pub fn clear_transcript(&mut self) {
        self.history.clear();
        self.current_response.clear();
        self.current_reasoning.clear();
        self.scroll_offset = 0;
    }

    pub fn command_context(&self) -> CommandContext {
        CommandContext {
            revision: self.command_context_revision,
            is_streaming: self.is_streaming,
            approval_pending: !self.approval_tool_calls.is_empty(),
            recovery_pending: !self.recovery_tool_calls.is_empty(),
        }
    }

    pub fn set_session_ids(&mut self, session_ids: Vec<String>) {
        self.session_ids = session_ids;
    }

    pub fn record_command_history(&mut self, command: String) {
        self.command_history.push(command);
        self.command_history_cursor = None;
    }

    pub fn navigate_input_history(&mut self, older: bool) -> bool {
        let command_mode = self.composer.text().starts_with('/');
        let (entries, cursor) = if command_mode {
            (&self.command_history, &mut self.command_history_cursor)
        } else {
            (&self.prompt_history, &mut self.prompt_history_cursor)
        };
        if entries.is_empty() {
            return false;
        }
        let next = if older {
            cursor.map_or(entries.len() - 1, |index| index.saturating_sub(1))
        } else {
            match cursor {
                Some(index) if *index + 1 < entries.len() => *index + 1,
                Some(_) => {
                    *cursor = None;
                    self.composer.clear();
                    return true;
                }
                None => return false,
            }
        };
        *cursor = Some(next);
        self.composer.set_text(entries[next].clone());
        true
    }

    pub fn begin_resume(&mut self) {
        self.can_resume_turn = false;
        self.current_response.clear();
        self.current_reasoning.clear();
        self.is_streaming = true;
        self.status = AppStatus::Waiting;
    }

    pub fn apply_runtime_event(&mut self, event: &RuntimeEvent) {
        self.command_context_revision = self.command_context_revision.wrapping_add(1);
        match event {
            RuntimeEvent::SessionRecovered {
                session_id,
                conversation,
            } => {
                push_unique(&mut self.session_ids, session_id.to_string());
                self.approval_tool_calls.clear();
                self.recovery_tool_calls.clear();
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
                push_unique(&mut self.recovery_tool_calls, tool_call_id.to_string());
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
                push_unique(&mut self.approval_tool_calls, tool_call_id.to_string());
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
                call_id,
                name,
                arguments,
                ..
            } => {
                self.approval_tool_calls
                    .retain(|id| id != &call_id.to_string());
                self.recovery_tool_calls
                    .retain(|id| id != &call_id.to_string());
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

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_completion_response_cannot_replace_newer_assist_state() {
        let mut state = AppState::new();
        state.composer.insert_str("/session load s");
        let request = state.begin_session_completion().unwrap();
        state.composer.insert_char('2');

        state.apply_completion_response(CompletionResponse {
            request_id: request.request_id,
            draft_revision: request.draft_revision,
            cursor: request.cursor,
            context_revision: request.context_revision,
            candidates: vec![CompletionItem {
                stable_key: "session:session-1".to_owned(),
                label: "session-1".to_owned(),
                detail: "local session".to_owned(),
                replacement: "session-1".to_owned(),
                replacement_range: request.replacement_range,
                kind: crate::app::command::CompletionKind::Resource,
                disabled_reason: None,
            }],
            error: None,
        });

        assert!(state.command_assist.candidates.is_empty());
        assert_eq!(state.composer.text(), "/session load s2");
    }
}
