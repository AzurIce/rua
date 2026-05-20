use std::collections::VecDeque;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::deepseek::Message;

/// UI event sent from the main loop to the app
#[derive(Debug, Clone)]
pub enum UiEvent {
    Tick,
    Key(crossterm::event::KeyEvent),
    Resize(u16, u16),
    StreamDelta(String),
    StreamDone,
    StreamError(String),
}

/// One entry in the chat history
#[derive(Debug, Clone)]
pub(crate) struct ChatEntry {
    pub(crate) role: Role,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
    System,
}

/// The main application state
pub struct App {
    pub messages: Vec<Message>,
    pub input: String,
    pub input_cursor: usize,
    pub(crate) history: VecDeque<ChatEntry>,
    pub current_response: String,
    pub is_streaming: bool,
    pub scroll_offset: u16,
    pub should_quit: bool,
}

/// Walk backwards from `idx` to the nearest UTF-8 char boundary.
fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

impl App {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            history: VecDeque::new(),
            current_response: String::new(),
            is_streaming: false,
            scroll_offset: 0,
            should_quit: false,
        }
    }

    /// Add a system message on startup
    pub fn add_system_message(&mut self, text: &str) {
        self.history.push_back(ChatEntry {
            role: Role::System,
            text: text.to_string(),
        });
    }

    /// Submit the current input and return the user message
    pub fn submit_input(&mut self) -> Option<Message> {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return None;
        }
        self.history.push_back(ChatEntry {
            role: Role::User,
            text: text.clone(),
        });
        self.messages.push(Message {
            role: "user".to_string(),
            content: text.clone(),
        });
        self.input.clear();
        self.input_cursor = 0;
        self.current_response.clear();
        self.is_streaming = true;
        Some(Message {
            role: "user".to_string(),
            content: text,
        })
    }

    /// Append a delta to the current streaming response
    pub fn append_delta(&mut self, delta: &str) {
        self.current_response.push_str(delta);
    }

    /// Mark streaming as done and save the response
    pub fn finish_stream(&mut self) {
        let text = self.current_response.trim().to_string();
        if !text.is_empty() {
            self.history.push_back(ChatEntry {
                role: Role::Assistant,
                text: text.clone(),
            });
            self.messages.push(Message {
                role: "assistant".to_string(),
                content: text,
            });
        }
        self.current_response.clear();
        self.is_streaming = false;
    }

    /// Add an error message to the chat history
    pub fn add_error(&mut self, text: &str) {
        self.is_streaming = false;
        self.history.push_back(ChatEntry {
            role: Role::System,
            text: format!("Error: {}", text),
        });
    }

    /// Handle a key event
    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char(c) => {
                if self.input_cursor >= self.input.len() {
                    self.input.push(c);
                } else {
                    self.input.insert(self.input_cursor, c);
                }
                self.input_cursor += c.len_utf8();
            }
            KeyCode::Backspace => {
                if self.input_cursor > 0 {
                    let prev = floor_char_boundary(&self.input, self.input_cursor - 1);
                    self.input_cursor = prev;
                    self.input.remove(prev);
                }
            }
            KeyCode::Delete => {
                if self.input_cursor < self.input.len() {
                    self.input.remove(self.input_cursor);
                }
            }
            KeyCode::Left => {
                if self.input_cursor > 0 {
                    let prev = floor_char_boundary(&self.input, self.input_cursor - 1);
                    self.input_cursor = prev;
                }
            }
            KeyCode::Right => {
                if self.input_cursor < self.input.len() {
                    if let Some(c) = self.input[self.input_cursor..].chars().next() {
                        self.input_cursor += c.len_utf8();
                    }
                }
            }
            KeyCode::Home => {
                self.input_cursor = 0;
            }
            KeyCode::End => {
                self.input_cursor = self.input.len();
            }
            KeyCode::Enter => {
                // Enter submits input
            }
            KeyCode::Up => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }
            KeyCode::Down => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            _ => {}
        }
    }

    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(area);

        let output_area = chunks[0];
        let input_area = chunks[1];

        // --- Output panel ---
        let mut output_lines: Vec<Line> = Vec::new();

        for entry in &self.history {
            let (label, style) = match entry.role {
                Role::User => (
                    "You",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Role::Assistant => (
                    "AI",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Role::System => (
                    "",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM),
                ),
            };
            if !label.is_empty() {
                output_lines.push(Line::from(vec![Span::styled(
                    format!("{}: ", label),
                    style,
                )]));
            }
            for line in entry.text.lines() {
                output_lines.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::White),
                )));
            }
            output_lines.push(Line::from(""));
        }

        // Show streaming response
        if self.is_streaming || !self.current_response.is_empty() {
            output_lines.push(Line::from(vec![Span::styled(
                "AI: ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )]));
            for line in self.current_response.lines() {
                output_lines.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::White),
                )));
            }
        }

        let output_block = Block::default()
            .title(" Chat ")
            .borders(Borders::ALL);
        let output_widget = Paragraph::new(output_lines)
            .block(output_block)
            .wrap(Wrap { trim: true })
            .scroll((self.scroll_offset, 0));
        frame.render_widget(output_widget, output_area);

        // --- Input panel ---
        let input_block = Block::default()
            .title(" Input (Enter to send, Ctrl+C to quit) ")
            .borders(Borders::ALL);
        let cursor_indicator = " ";
        let input_text = if self.input_cursor >= self.input.len() {
            format!("{}{}", self.input, cursor_indicator)
        } else {
            let before = &self.input[..self.input_cursor];
            let after = &self.input[self.input_cursor..];
            format!("{}{}{}", before, cursor_indicator, after)
        };
        let input_widget = Paragraph::new(input_text).block(input_block);
        frame.render_widget(input_widget, input_area);
    }
}
