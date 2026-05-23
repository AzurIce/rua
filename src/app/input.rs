use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::state::{floor_char_boundary, AppState};

pub fn handle_key(state: &mut AppState, key: KeyEvent) {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.should_quit = true;
        }
        KeyCode::Char(c) => {
            if state.input_cursor >= state.input.len() {
                state.input.push(c);
            } else {
                state.input.insert(state.input_cursor, c);
            }
            state.input_cursor += c.len_utf8();
        }
        KeyCode::Backspace => {
            if state.input_cursor > 0 {
                let prev = floor_char_boundary(&state.input, state.input_cursor - 1);
                state.input_cursor = prev;
                state.input.remove(prev);
            }
        }
        KeyCode::Delete => {
            if state.input_cursor < state.input.len() {
                state.input.remove(state.input_cursor);
            }
        }
        KeyCode::Left => {
            if state.input_cursor > 0 {
                state.input_cursor = floor_char_boundary(&state.input, state.input_cursor - 1);
            }
        }
        KeyCode::Right => {
            if state.input_cursor < state.input.len() {
                if let Some(c) = state.input[state.input_cursor..].chars().next() {
                    state.input_cursor += c.len_utf8();
                }
            }
        }
        KeyCode::Home => state.input_cursor = 0,
        KeyCode::End => state.input_cursor = state.input.len(),
        KeyCode::Up => state.scroll_offset = state.scroll_offset.saturating_add(1),
        KeyCode::Down => state.scroll_offset = state.scroll_offset.saturating_sub(1),
        _ => {}
    }
}
