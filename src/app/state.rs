use std::collections::VecDeque;

use crate::model::{ChatEntry, Role};

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
    pub input: String,
    pub input_cursor: usize,
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
}

impl AppState {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            input_cursor: 0,
            history: VecDeque::new(),
            current_response: String::new(),
            current_reasoning: String::new(),
            is_streaming: false,
            scroll_offset: 0,
            should_quit: false,
            status: AppStatus::Idle,
            spinner_frame: 0,
            token_count: 0,
        }
    }

    /// Return a clone of history entries for the session layer.
    pub fn history_entries(&self) -> Vec<ChatEntry> {
        self.history.iter().cloned().collect()
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
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return None;
        }
        self.history.push_back(ChatEntry {
            role: Role::User,
            text: text.clone(),
            tool_call_id: None,
            reasoning_content: None,
            reasoning_expanded: false,
        });
        self.input.clear();
        self.input_cursor = 0;
        self.current_response.clear();
        self.current_reasoning.clear();
        self.is_streaming = true;
        self.status = AppStatus::Sending;
        self.token_count = 0;
        Some(text)
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

// ------------------------------------------------------------------
// Text helpers (used by render)
// ------------------------------------------------------------------

/// Walk backwards from `idx` to the nearest UTF-8 char boundary.
pub(crate) fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

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
