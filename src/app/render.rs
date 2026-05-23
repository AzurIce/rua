use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::state::{
    AppState, AppStatus, BG, BG_PANEL, TEXT, TEXT_MUTED, BORDER, PRIMARY, USER_ACCENT,
    AI_ACCENT, SYSTEM_ACCENT, SPINNER_FRAMES, SUCCESS,
};
use crate::model::Role;

pub fn draw(state: &AppState, frame: &mut Frame) {
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

    render_messages(state, frame, chunks[0], content_width);
    render_status(state, frame, chunks[1]);
    render_input(state, frame, chunks[2]);
}

fn render_messages(state: &AppState, frame: &mut Frame, area: Rect, content_width: usize) {
    let mut lines: Vec<Line> = Vec::new();

    for entry in &state.history {
        let (accent, label) = match entry.role {
            Role::User => (USER_ACCENT, Some("You")),
            Role::Assistant => (AI_ACCENT, Some("AI")),
            Role::System => (SYSTEM_ACCENT, None),
            Role::Tool => (SUCCESS, Some("🔧")),
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

        for line in crate::app::state::wrap_paragraph(&entry.text, content_width.max(1)) {
            lines.push(Line::from(vec![
                Span::styled("┃ ", Style::default().fg(accent)),
                Span::styled(line, Style::default().fg(TEXT)),
            ]));
        }

        // Render reasoning content for Assistant entries
        if entry.role == Role::Assistant {
            if let Some(ref reasoning) = entry.reasoning_content {
                if entry.reasoning_expanded {
                    lines.push(Line::from(vec![
                        Span::styled("┃ ", Style::default().fg(TEXT_MUTED)),
                        Span::styled("💭 thinking:", Style::default().fg(TEXT_MUTED).add_modifier(Modifier::ITALIC)),
                    ]));
                    for line in crate::app::state::wrap_paragraph(reasoning, content_width.max(1)) {
                        lines.push(Line::from(vec![
                            Span::styled("┃ ", Style::default().fg(TEXT_MUTED)),
                            Span::styled(line, Style::default().fg(TEXT_MUTED)),
                        ]));
                    }
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("┃ ", Style::default().fg(TEXT_MUTED)),
                        Span::styled("💭 ▶ thinking... (press r to expand)", Style::default().fg(TEXT_MUTED).add_modifier(Modifier::ITALIC)),
                    ]));
                }
            }
        }
    }

    if state.is_streaming || !state.current_response.is_empty() || !state.current_reasoning.is_empty() {
        lines.push(Line::from(""));
        let icon = if state.is_streaming {
            SPINNER_FRAMES[state.spinner_frame % SPINNER_FRAMES.len()].to_string()
        } else {
            "┃".to_string()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{} ", icon), Style::default().fg(AI_ACCENT)),
            Span::styled("AI", Style::default().fg(AI_ACCENT).add_modifier(Modifier::BOLD)),
        ]));

        // Show reasoning content in real-time during streaming
        if !state.current_reasoning.is_empty() {
            for line in crate::app::state::wrap_paragraph(&state.current_reasoning, content_width.max(1)) {
                lines.push(Line::from(vec![
                    Span::styled("┃ ", Style::default().fg(TEXT_MUTED)),
                    Span::styled(line, Style::default().fg(TEXT_MUTED)),
                ]));
            }
        }

        for line in crate::app::state::wrap_paragraph(&state.current_response, content_width.max(1)) {
            lines.push(Line::from(vec![
                Span::styled("┃ ", Style::default().fg(AI_ACCENT)),
                Span::styled(line, Style::default().fg(TEXT)),
            ]));
        }
    }

    frame.render_widget(
        Paragraph::new(lines).scroll((state.scroll_offset, 0)),
        area,
    );
}

fn render_status(state: &AppState, frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Block::default().style(Style::default().bg(BG_PANEL)),
        area,
    );

    let spinner = SPINNER_FRAMES[state.spinner_frame % SPINNER_FRAMES.len()];
    let icon = match state.status {
        AppStatus::Idle => "◆",
        AppStatus::Sending => "↑",
        AppStatus::Waiting => "",
        AppStatus::Streaming => "↓",
    };

    let (icon_str, color) = match state.status {
        AppStatus::Waiting => (spinner.to_string(), state.status.color()),
        _ => (icon.to_string(), state.status.color()),
    };

    let mut spans = vec![
        Span::styled(
            format!(" {} ", icon_str),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(state.status.label(), Style::default().fg(TEXT)),
    ];

    if state.status == AppStatus::Streaming && state.token_count > 0 {
        spans.push(Span::styled(
            format!("  ~{} tok", state.token_count),
            Style::default().fg(TEXT_MUTED),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_input(state: &AppState, frame: &mut Frame, area: Rect) {
    let prompt = Span::styled("> ", Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD));
    let content = if state.input.is_empty() {
        Text::from(vec![Line::from(vec![
            prompt,
            Span::styled(
                "Type a message...",
                Style::default().fg(TEXT_MUTED).add_modifier(Modifier::ITALIC),
            ),
        ])])
    } else {
        let text = if state.input_cursor >= state.input.len() {
            format!("{} ", state.input)
        } else {
            let before = &state.input[..state.input_cursor];
            let after = &state.input[state.input_cursor..];
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
