use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

use crate::app::state::{
    AI_ACCENT, AppState, AppStatus, BG, BG_PANEL, BORDER, PRIMARY, SPINNER_FRAMES, SUCCESS,
    SYSTEM_ACCENT, TEXT, TEXT_MUTED, USER_ACCENT,
};
use crate::model::Role;

pub fn draw(state: &AppState, frame: &mut Frame) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .margin(1)
        .split(area);

    let content_width = (chunks[0].width.saturating_sub(2)) as usize;

    render_messages(state, frame, chunks[0], content_width);
    render_command_assist(state, frame, chunks[0]);
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
                Span::styled(
                    label,
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ),
            ]));
        } else {
            lines.push(Line::from(vec![Span::styled(
                "┃ ",
                Style::default().fg(SYSTEM_ACCENT),
            )]));
        }

        for line in crate::app::state::wrap_paragraph(&entry.text, content_width.max(1)) {
            lines.push(Line::from(vec![
                Span::styled("┃ ", Style::default().fg(accent)),
                Span::styled(line, Style::default().fg(TEXT)),
            ]));
        }

        // Render reasoning content for Assistant entries
        if entry.role == Role::Assistant
            && let Some(ref reasoning) = entry.reasoning_content
        {
            if entry.reasoning_expanded {
                lines.push(Line::from(vec![
                    Span::styled("┃ ", Style::default().fg(TEXT_MUTED)),
                    Span::styled(
                        "💭 thinking:",
                        Style::default()
                            .fg(TEXT_MUTED)
                            .add_modifier(Modifier::ITALIC),
                    ),
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
                    Span::styled(
                        "💭 ▶ thinking... (press Ctrl+R to expand)",
                        Style::default()
                            .fg(TEXT_MUTED)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ]));
            }
        }
    }

    if state.is_streaming
        || !state.current_response.is_empty()
        || !state.current_reasoning.is_empty()
    {
        lines.push(Line::from(""));
        let icon = if state.is_streaming {
            SPINNER_FRAMES[state.spinner_frame % SPINNER_FRAMES.len()].to_string()
        } else {
            "┃".to_string()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{} ", icon), Style::default().fg(AI_ACCENT)),
            Span::styled(
                "AI",
                Style::default().fg(AI_ACCENT).add_modifier(Modifier::BOLD),
            ),
        ]));

        // Show reasoning content in real-time during streaming
        if !state.current_reasoning.is_empty() {
            for line in
                crate::app::state::wrap_paragraph(&state.current_reasoning, content_width.max(1))
            {
                lines.push(Line::from(vec![
                    Span::styled("┃ ", Style::default().fg(TEXT_MUTED)),
                    Span::styled(line, Style::default().fg(TEXT_MUTED)),
                ]));
            }
        }

        for line in crate::app::state::wrap_paragraph(&state.current_response, content_width.max(1))
        {
            lines.push(Line::from(vec![
                Span::styled("┃ ", Style::default().fg(AI_ACCENT)),
                Span::styled(line, Style::default().fg(TEXT)),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines).scroll((state.scroll_offset, 0)), area);
}

fn render_status(state: &AppState, frame: &mut Frame, area: Rect) {
    frame.render_widget(Block::default().style(Style::default().bg(BG_PANEL)), area);

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

    if let Some(usage) = &state.command_assist.usage {
        spans.push(Span::styled(
            format!("  {usage}"),
            Style::default().fg(TEXT_MUTED),
        ));
    }
    if let Some(diagnostic) = &state.command_assist.diagnostic {
        spans.push(Span::styled(
            format!("  {diagnostic}"),
            Style::default().fg(PRIMARY),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_input(state: &AppState, frame: &mut Frame, area: Rect) {
    let inner = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    );
    let prompt_width = 2;
    let editor_width = inner.width.saturating_sub(prompt_width);
    let viewport = state.composer.viewport(editor_width);
    let prompt = Span::styled(
        "> ",
        Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
    );
    let content = if state.composer.is_empty() {
        Text::from(vec![Line::from(vec![
            prompt,
            Span::styled(
                "Type a message...",
                Style::default()
                    .fg(TEXT_MUTED)
                    .add_modifier(Modifier::ITALIC),
            ),
        ])])
    } else {
        let mut spans = vec![
            prompt,
            Span::styled(viewport.text, Style::default().fg(TEXT)),
        ];
        if let Some(ghost) = ghost_completion(state) {
            spans.push(Span::styled(ghost, Style::default().fg(TEXT_MUTED)));
        }
        Text::from(vec![Line::from(spans)])
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

    if inner.width > prompt_width && inner.height > 0 {
        let cursor_x = inner
            .x
            .saturating_add(prompt_width)
            .saturating_add(viewport.cursor_column)
            .min(inner.right().saturating_sub(1));
        frame.set_cursor_position(Position::new(cursor_x, inner.y));
    }
}

fn render_command_assist(state: &AppState, frame: &mut Frame, area: Rect) {
    if !state.command_assist.open || state.command_assist.candidates.is_empty() || area.height < 3 {
        return;
    }
    let rows = u16::try_from(state.command_assist.candidates.len().min(4)).unwrap_or(4);
    let first_row = state
        .command_assist
        .selected
        .saturating_sub(rows.saturating_sub(1) as usize)
        .min(
            state
                .command_assist
                .candidates
                .len()
                .saturating_sub(rows as usize),
        );
    let height = rows.saturating_add(2).min(area.height);
    let popup = Rect::new(
        area.x,
        area.bottom().saturating_sub(height),
        area.width,
        height,
    );
    let lines = state
        .command_assist
        .candidates
        .iter()
        .skip(first_row)
        .take(rows as usize)
        .enumerate()
        .map(|(offset, candidate)| {
            let selected = first_row + offset == state.command_assist.selected;
            Line::from(vec![
                Span::styled(
                    if selected { "> " } else { "  " },
                    Style::default().fg(if selected { PRIMARY } else { TEXT_MUTED }),
                ),
                Span::styled(
                    &candidate.label,
                    Style::default()
                        .fg(if selected { PRIMARY } else { TEXT })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(
                    format!("  {}", candidate.detail),
                    Style::default().fg(TEXT_MUTED),
                ),
            ])
        })
        .collect::<Vec<_>>();
    let total = state.command_assist.candidates.len();
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(
                        " commands {}/{} ",
                        state.command_assist.selected + 1,
                        total
                    ))
                    .border_style(Style::default().fg(BORDER)),
            )
            .style(Style::default().bg(BG_PANEL)),
        popup,
    );
    if total > rows as usize {
        let mut scrollbar_state =
            ScrollbarState::new(total).position(state.command_assist.selected);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓")),
            popup.inner(Margin {
                horizontal: 0,
                vertical: 1,
            }),
            &mut scrollbar_state,
        );
    }
}

fn ghost_completion(state: &AppState) -> Option<String> {
    let [candidate] = state.command_assist.candidates.as_slice() else {
        return None;
    };
    if candidate.replacement_range.end != state.composer.cursor() {
        return None;
    }
    let typed = state
        .composer
        .text()
        .get(candidate.replacement_range.clone())?;
    candidate
        .replacement
        .strip_prefix(typed)
        .filter(|suffix| !suffix.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use ratatui::{
        Terminal,
        backend::{Backend, TestBackend},
    };

    use super::*;

    #[test]
    fn draw_places_the_hardware_cursor_after_wide_text() {
        let mut state = AppState::new();
        state.composer.insert_str("你a");
        let mut terminal = Terminal::new(TestBackend::new(20, 8)).unwrap();

        terminal.draw(|frame| draw(&state, frame)).unwrap();

        assert_eq!(
            terminal.backend_mut().get_cursor_position().unwrap(),
            Position::new(6, 5)
        );
    }

    #[test]
    fn draw_shows_command_candidates_without_moving_the_cursor() {
        let mut state = AppState::new();
        state.composer.insert_str("/rec");
        state.refresh_command_assist(&crate::app::CommandRegistry::builtins());
        let mut terminal = Terminal::new(TestBackend::new(50, 12)).unwrap();

        terminal.draw(|frame| draw(&state, frame)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("recovery"));
        assert_eq!(
            terminal.backend_mut().get_cursor_position().unwrap(),
            Position::new(7, 9)
        );
    }

    #[test]
    fn command_popup_shows_position_total_and_scrollbar() {
        let mut state = AppState::new();
        state.composer.insert_str("/");
        state.refresh_command_assist(&crate::app::CommandRegistry::builtins());
        let total = state.command_assist.candidates.len();
        assert!(total > 4);
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();

        terminal.draw(|frame| draw(&state, frame)).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains(&format!("commands 1/{total}")));
        assert!(rendered.contains('↓'));
    }
}
