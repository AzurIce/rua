use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::state::AppState;

pub fn handle_key(state: &mut AppState, key: KeyEvent) {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return;
    }

    match key.code {
        KeyCode::Char('c') if is_ctrl_shortcut(key.modifiers) => {
            state.should_quit = true;
        }
        KeyCode::Char('r') if is_ctrl_shortcut(key.modifiers) => {
            state.toggle_latest_reasoning();
        }
        KeyCode::Char(c) if !has_command_modifier(key.modifiers) => {
            state.composer.insert_char(c);
        }
        KeyCode::Backspace => state.composer.delete_backward(),
        KeyCode::Delete => state.composer.delete_forward(),
        KeyCode::Left => state.composer.move_left(),
        KeyCode::Right => state.composer.move_right(),
        KeyCode::Home => state.composer.move_to_start(),
        KeyCode::End => state.composer.move_to_end(),
        KeyCode::Up => state.scroll_offset = state.scroll_offset.saturating_add(1),
        KeyCode::Down => state.scroll_offset = state.scroll_offset.saturating_sub(1),
        _ => {}
    }
}

fn is_ctrl_shortcut(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL) && !modifiers.contains(KeyModifiers::ALT)
}

fn has_command_modifier(modifiers: KeyModifiers) -> bool {
    let has_ctrl_or_alt = modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    has_ctrl_or_alt && !is_altgr(modifiers)
}

#[cfg(windows)]
fn is_altgr(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL) && modifiers.contains(KeyModifiers::ALT)
}

#[cfg(not(windows))]
fn is_altgr(_modifiers: KeyModifiers) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEventKind, KeyModifiers};

    use super::*;

    #[test]
    fn release_does_not_edit_the_composer() {
        let mut state = AppState::new();
        handle_key(
            &mut state,
            KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
        );

        assert!(state.composer.is_empty());
    }

    #[test]
    fn plain_r_is_text() {
        let mut state = AppState::new();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        );

        assert_eq!(state.composer.text(), "r");
    }

    #[cfg(windows)]
    #[test]
    fn altgr_character_is_text_not_a_ctrl_shortcut() {
        let mut state = AppState::new();
        handle_key(
            &mut state,
            KeyEvent::new(
                KeyCode::Char('@'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
        );

        assert_eq!(state.composer.text(), "@");
        assert!(!state.should_quit);
    }
}
