use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
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
    render_tree_overlay(state, frame, area);
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

    if !state.tree_overlay.open && inner.width > prompt_width && inner.height > 0 {
        let cursor_x = inner
            .x
            .saturating_add(prompt_width)
            .saturating_add(viewport.cursor_column)
            .min(inner.right().saturating_sub(1));
        frame.set_cursor_position(Position::new(cursor_x, inner.y));
    }
}

fn render_tree_overlay(state: &AppState, frame: &mut Frame, area: Rect) {
    if !state.tree_overlay.open || area.width < 20 || area.height < 8 {
        return;
    }
    let width = area
        .width
        .saturating_mul(4)
        .checked_div(5)
        .unwrap_or(area.width)
        .max(20);
    let height = area
        .height
        .saturating_mul(3)
        .checked_div(4)
        .unwrap_or(area.height)
        .max(8);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width.min(area.width),
        height.min(area.height),
    );
    frame.render_widget(Clear, popup);

    let visible_rows = popup.height.saturating_sub(3) as usize;
    let visible_indices = state
        .tree_overlay
        .items
        .iter()
        .enumerate()
        .filter_map(|(index, _)| state.tree_item_visible(index).then_some(index))
        .collect::<Vec<_>>();
    let total = visible_indices.len();
    let selected_position = visible_indices
        .iter()
        .position(|index| *index == state.tree_overlay.selected)
        .unwrap_or(0);
    let first = selected_position
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(total.saturating_sub(visible_rows));
    let lines = visible_indices
        .iter()
        .skip(first)
        .take(visible_rows)
        .enumerate()
        .map(|(offset, index)| {
            let item = &state.tree_overlay.items[*index];
            let selected = first + offset == selected_position;
            let indent = "  ".repeat(item.depth);
            let head = if item.is_head { " *" } else { "" };
            let branch_color = if item.is_active_path { SUCCESS } else { BORDER };
            Line::from(vec![
                Span::styled(
                    if selected { "> " } else { "  " },
                    Style::default().fg(if selected { PRIMARY } else { TEXT_MUTED }),
                ),
                Span::styled(indent, Style::default().fg(branch_color)),
                Span::styled("├─ ", Style::default().fg(branch_color)),
                Span::styled(
                    format!("[{}] ", item.kind),
                    Style::default().fg(SYSTEM_ACCENT),
                ),
                Span::styled(
                    &item.label,
                    Style::default()
                        .fg(if selected {
                            PRIMARY
                        } else if item.is_active_path {
                            TEXT
                        } else {
                            TEXT_MUTED
                        })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(head, Style::default().fg(SUCCESS)),
            ])
        })
        .collect::<Vec<_>>();
    let title = if total == 0 {
        " session tree ".to_owned()
    } else {
        format!(
            " session tree {}/{} · Enter checkout · e edit · history only, files unchanged · Esc close ",
            selected_position + 1,
            total,
        )
    };
    let title = format!(
        "{} · t tools:{} · d cwd:{} ",
        title.trim_end(),
        if state.tree_overlay.show_tools {
            "on"
        } else {
            "off"
        },
        if state.tree_overlay.show_cwd {
            "on"
        } else {
            "off"
        }
    );
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(PRIMARY)),
            )
            .style(Style::default().bg(BG_PANEL)),
        popup,
    );
    if total > visible_rows {
        let mut scrollbar = ScrollbarState::new(total).position(selected_position);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            popup.inner(Margin {
                horizontal: 0,
                vertical: 1,
            }),
            &mut scrollbar,
        );
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

    #[test]
    fn draw_renders_the_session_tree_overlay() {
        let mut state = AppState::new();
        state.open_tree_overlay(
            vec![crate::app::TreeOverlayItem {
                entry_id: None,
                depth: 0,
                kind: "root".to_owned(),
                label: "virtual root".to_owned(),
                is_head: true,
                is_active_path: true,
                editable: false,
            }],
            crate::agent::HeadRevision(0),
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

        terminal.draw(|frame| draw(&state, frame)).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("session tree"));
        assert!(rendered.contains("virtual root"));
    }
}
