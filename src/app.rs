use std::collections::VecDeque;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::deepseek::Message;

// === opencode-inspired theme ===
const BG: Color = Color::Rgb(10, 10, 10);          // #0a0a0a
const BG_PANEL: Color = Color::Rgb(20, 20, 20);    // #141414
const BG_ELEMENT: Color = Color::Rgb(30, 30, 30);  // #1e1e1e
const TEXT: Color = Color::Rgb(238, 238, 238);     // #eeeeee
const TEXT_MUTED: Color = Color::Rgb(128, 128, 128); // #808080
const BORDER: Color = Color::Rgb(72, 72, 72);      // #484848
const PRIMARY: Color = Color::Rgb(250, 178, 131);  // #fab283 (peach)
const USER_ACCENT: Color = Color::Rgb(92, 156, 245); // #5c9cf5 (blue)
const AI_ACCENT: Color = Color::Rgb(159, 124, 216); // #9d7cd8 (purple)
const SYSTEM_ACCENT: Color = Color::Rgb(128, 128, 128);
const SUCCESS: Color = Color::Rgb(127, 216, 143); // #7fd88f
const ERROR: Color = Color::Rgb(224, 108, 117);   // #e06c75

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

/// Wrap a single line of text to a maximum display width (in columns).
/// Handles Unicode widths: CJK = 2 columns, ASCII = 1.
fn wrap_line(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;

    for word in text.split_whitespace() {
        let word_width = word.width();

        if current_width > 0 {
            // Try placing word after a space
            if current_width + 1 + word_width <= max_width {
                current.push(' ');
                current.push_str(word);
                current_width += 1 + word_width;
                continue;
            }
            // Flush current line
            lines.push(current);
            current = String::new();
            current_width = 0;
        }

        // Place word on fresh line (may need character-level break)
        if word_width <= max_width {
            current = word.to_string();
            current_width = word_width;
        } else {
            for c in word.chars() {
                let cw = c.width().unwrap_or(0);
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
fn wrap_paragraph(text: &str, max_width: usize) -> Vec<String> {
    let mut all = Vec::new();
    for (i, para) in text.split('\n').enumerate() {
        if i > 0 {
            // Preserve blank lines between paragraphs
        }
        if para.is_empty() {
            all.push(String::new());
            continue;
        }
        all.extend(wrap_line(para, max_width));
    }
    all
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

        // Clear background
        frame.render_widget(
            Block::default().style(Style::default().bg(BG)),
            area,
        );

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .margin(1)
            .split(area);

        let output_area = chunks[0];
        let input_area = chunks[1];

        // Content width = available width minus the "┃ " prefix (2 columns)
        let content_width = (output_area.width.saturating_sub(2)) as usize;

        // --- Output panel: build text with left-border messages ---
        let mut output_lines: Vec<Line> = Vec::new();

        for entry in &self.history {
            let (accent, label) = match entry.role {
                Role::User => (USER_ACCENT, "You"),
                Role::Assistant => (AI_ACCENT, "AI"),
                Role::System => (SYSTEM_ACCENT, ""),
            };

            // Spacer between messages
            output_lines.push(Line::from(""));

            if !label.is_empty() {
                output_lines.push(Line::from(vec![
                    Span::styled("┃ ", Style::default().fg(accent)),
                    Span::styled(label, Style::default().fg(accent).add_modifier(Modifier::BOLD)),
                ]));
            } else {
                output_lines.push(Line::from(vec![
                    Span::styled("┃ ", Style::default().fg(SYSTEM_ACCENT)),
                ]));
            }

            // Content lines: manual wrap so every wrapped line keeps the prefix
            let wrapped = wrap_paragraph(&entry.text, content_width.max(1));
            for line in wrapped {
                output_lines.push(Line::from(vec![
                    Span::styled("┃ ", Style::default().fg(accent)),
                    Span::styled(line, Style::default().fg(TEXT)),
                ]));
            }
        }

        // Show streaming response with a spinner indicator
        if self.is_streaming || !self.current_response.is_empty() {
            output_lines.push(Line::from(""));
            let spinner = if self.is_streaming {
                Span::styled("◐ ", Style::default().fg(AI_ACCENT))
            } else {
                Span::styled("┃ ", Style::default().fg(AI_ACCENT))
            };
            output_lines.push(Line::from(vec![
                spinner,
                Span::styled("AI", Style::default().fg(AI_ACCENT).add_modifier(Modifier::BOLD)),
            ]));
            let wrapped = wrap_paragraph(&self.current_response, content_width.max(1));
            for line in wrapped {
                output_lines.push(Line::from(vec![
                    Span::styled("┃ ", Style::default().fg(AI_ACCENT)),
                    Span::styled(line, Style::default().fg(TEXT)),
                ]));
            }
        }

        let output_widget = Paragraph::new(output_lines)
            .scroll((self.scroll_offset, 0));
        frame.render_widget(output_widget, output_area);

        // --- Input separator line ---
        let sep_y = input_area.y.saturating_sub(1);
        if sep_y >= area.y {
            let sep_line = "─".repeat(area.width as usize);
            let sep_widget = Paragraph::new(sep_line).style(Style::default().fg(BORDER));
            let sep_area = ratatui::layout::Rect {
                x: area.x,
                y: sep_y,
                width: area.width,
                height: 1,
            };
            frame.render_widget(sep_widget, sep_area);
        }

        // --- Input panel ---
        let prompt = Span::styled("> ", Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD));
        let input_content = if self.input.is_empty() {
            Text::from(vec![Line::from(vec![
                prompt,
                Span::styled(
                    "Type a message...",
                    Style::default().fg(TEXT_MUTED).add_modifier(Modifier::ITALIC),
                ),
            ])])
        } else {
            let cursor_indicator = " ";
            let text = if self.input_cursor >= self.input.len() {
                format!("{}{}", self.input, cursor_indicator)
            } else {
                let before = &self.input[..self.input_cursor];
                let after = &self.input[self.input_cursor..];
                format!("{}{}{}", before, cursor_indicator, after)
            };
            Text::from(vec![Line::from(vec![
                prompt,
                Span::styled(text, Style::default().fg(TEXT)),
            ])])
        };

        let input_widget = Paragraph::new(input_content)
            .style(Style::default().bg(BG_PANEL));
        frame.render_widget(input_widget, input_area);
    }
}
