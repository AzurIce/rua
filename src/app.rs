use std::collections::VecDeque;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::deepseek::Message;

// === opencode-inspired theme ===
const BG: Color = Color::Rgb(10, 10, 10);
const BG_PANEL: Color = Color::Rgb(20, 20, 20);
const TEXT: Color = Color::Rgb(238, 238, 238);
const TEXT_MUTED: Color = Color::Rgb(128, 128, 128);
const BORDER: Color = Color::Rgb(72, 72, 72);
const PRIMARY: Color = Color::Rgb(250, 178, 131);
const USER_ACCENT: Color = Color::Rgb(92, 156, 245);
const AI_ACCENT: Color = Color::Rgb(159, 124, 216);
const SYSTEM_ACCENT: Color = Color::Rgb(128, 128, 128);
const SUCCESS: Color = Color::Rgb(127, 216, 143);

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

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

    pub fn color(&self) -> Color {
        match self {
            AppStatus::Idle => TEXT_MUTED,
            AppStatus::Sending => PRIMARY,
            AppStatus::Waiting => AI_ACCENT,
            AppStatus::Streaming => SUCCESS,
        }
    }
}

/// The main application state
pub struct App {
    pub input: String,
    pub input_cursor: usize,
    pub(crate) history: VecDeque<ChatEntry>,
    pub current_response: String,
    pub is_streaming: bool,
    pub scroll_offset: u16,
    pub should_quit: bool,
    pub status: AppStatus,
    pub spinner_frame: usize,
    pub token_count: usize,
}

/// Walk backwards from `idx` to the nearest UTF-8 char boundary.
fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Wrap a single line of text to a maximum display width (in columns).
fn wrap_line(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;

    for word in text.split_whitespace() {
        let word_width = word.width();

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
    for para in text.split('\n') {
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
            input: String::new(),
            input_cursor: 0,
            history: VecDeque::new(),
            current_response: String::new(),
            is_streaming: false,
            scroll_offset: 0,
            should_quit: false,
            status: AppStatus::Idle,
            spinner_frame: 0,
            token_count: 0,
        }
    }

    /// Convert history entries to API message format.
    pub fn history_messages(&self) -> Vec<Message> {
        self.history
            .iter()
            .filter_map(|entry| match entry.role {
                Role::User => Some(Message {
                    role: "user".to_string(),
                    content: entry.text.clone(),
                }),
                Role::Assistant => Some(Message {
                    role: "assistant".to_string(),
                    content: entry.text.clone(),
                }),
                Role::System => None,
            })
            .collect()
    }

    pub fn add_system_message(&mut self, text: &str) {
        self.history.push_back(ChatEntry {
            role: Role::System,
            text: text.to_string(),
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
        });
        self.input.clear();
        self.input_cursor = 0;
        self.current_response.clear();
        self.is_streaming = true;
        self.status = AppStatus::Sending;
        self.token_count = 0;
        Some(text)
    }

    pub fn append_delta(&mut self, delta: &str) {
        self.current_response.push_str(delta);
        self.status = AppStatus::Streaming;
        self.token_count = self.current_response.len() / 4;
    }

    pub fn finish_stream(&mut self) {
        let text = self.current_response.trim().to_string();
        if !text.is_empty() {
            self.history.push_back(ChatEntry {
                role: Role::Assistant,
                text: text,
            });
        }
        self.current_response.clear();
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
        });
    }

    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
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
                    self.input_cursor = floor_char_boundary(&self.input, self.input_cursor - 1);
                }
            }
            KeyCode::Right => {
                if self.input_cursor < self.input.len() {
                    if let Some(c) = self.input[self.input_cursor..].chars().next() {
                        self.input_cursor += c.len_utf8();
                    }
                }
            }
            KeyCode::Home => self.input_cursor = 0,
            KeyCode::End => self.input_cursor = self.input.len(),
            KeyCode::Up => self.scroll_offset = self.scroll_offset.saturating_add(1),
            KeyCode::Down => self.scroll_offset = self.scroll_offset.saturating_sub(1),
            _ => {}
        }
    }

    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(
            Block::default().style(Style::default().bg(BG)),
            area,
        );

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1), Constraint::Length(3)])
            .margin(1)
            .split(area);

        let content_width = (chunks[0].width.saturating_sub(2)) as usize;

        self.render_messages(frame, chunks[0], content_width);
        self.render_status(frame, chunks[1]);
        self.render_input(frame, chunks[2]);
    }

    fn render_messages(&self, frame: &mut Frame, area: Rect, content_width: usize) {
        let mut lines: Vec<Line> = Vec::new();

        for entry in &self.history {
            let (accent, label) = match entry.role {
                Role::User => (USER_ACCENT, Some("You")),
                Role::Assistant => (AI_ACCENT, Some("AI")),
                Role::System => (SYSTEM_ACCENT, None),
            };

            lines.push(Line::from(""));

            if let Some(label) = label {
                lines.push(Line::from(vec![
                    Span::styled("┃ ", Style::default().fg(accent)),
                    Span::styled(label, Style::default().fg(accent).add_modifier(Modifier::BOLD)),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled("┃ ", Style::default().fg(SYSTEM_ACCENT)),
                ]));
            }

            for line in wrap_paragraph(&entry.text, content_width.max(1)) {
                lines.push(Line::from(vec![
                    Span::styled("┃ ", Style::default().fg(accent)),
                    Span::styled(line, Style::default().fg(TEXT)),
                ]));
            }
        }

        if self.is_streaming || !self.current_response.is_empty() {
            lines.push(Line::from(""));
            let icon = if self.is_streaming {
                SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()].to_string()
            } else {
                "┃".to_string()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", icon), Style::default().fg(AI_ACCENT)),
                Span::styled("AI", Style::default().fg(AI_ACCENT).add_modifier(Modifier::BOLD)),
            ]));
            for line in wrap_paragraph(&self.current_response, content_width.max(1)) {
                lines.push(Line::from(vec![
                    Span::styled("┃ ", Style::default().fg(AI_ACCENT)),
                    Span::styled(line, Style::default().fg(TEXT)),
                ]));
            }
        }

        frame.render_widget(
            Paragraph::new(lines).scroll((self.scroll_offset, 0)),
            area,
        );
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(
            Block::default().style(Style::default().bg(BG_PANEL)),
            area,
        );

        let spinner = SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()];
        let icon = match self.status {
            AppStatus::Idle => "◆",
            AppStatus::Sending => "↑",
            AppStatus::Waiting => {
                // Use the spinner char directly
                // We need to create a String for this case since it's dynamic
                // But Span::styled needs &str... let's use format! for the whole thing
                // Actually let's just handle the text separately
                ""
            }
            AppStatus::Streaming => "↓",
        };

        let (icon_str, color) = match self.status {
            AppStatus::Waiting => (spinner.to_string(), self.status.color()),
            _ => (icon.to_string(), self.status.color()),
        };

        let mut spans = vec![
            Span::styled(
                format!(" {} ", icon_str),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(self.status.label(), Style::default().fg(TEXT)),
        ];

        if self.status == AppStatus::Streaming && self.token_count > 0 {
            spans.push(Span::styled(
                format!("  ~{} tok", self.token_count),
                Style::default().fg(TEXT_MUTED),
            ));
        }

        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_input(&self, frame: &mut Frame, area: Rect) {
        let prompt = Span::styled("> ", Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD));
        let content = if self.input.is_empty() {
            Text::from(vec![Line::from(vec![
                prompt,
                Span::styled(
                    "Type a message...",
                    Style::default().fg(TEXT_MUTED).add_modifier(Modifier::ITALIC),
                ),
            ])])
        } else {
            let text = if self.input_cursor >= self.input.len() {
                format!("{} ", self.input)
            } else {
                let before = &self.input[..self.input_cursor];
                let after = &self.input[self.input_cursor..];
                format!("{} {}", before, after)
            };
            Text::from(vec![Line::from(vec![
                prompt,
                Span::styled(text, Style::default().fg(TEXT)),
            ])])
        };

        frame.render_widget(
            Paragraph::new(content)
                .block(
                    Block::default()
                        .borders(Borders::TOP)
                        .border_style(Style::default().fg(BORDER)),
                )
                .style(Style::default().bg(BG_PANEL)),
            area,
        );
    }
}
