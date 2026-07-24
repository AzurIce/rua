use crate::app::state::AppState;
use crate::tui::{TuiKeyCode, TuiKeyEvent, TuiKeyModifiers};

pub fn handle_key(state: &mut AppState, key: TuiKeyEvent) {
    match key.code {
        TuiKeyCode::Char('c') if is_ctrl_shortcut(key.modifiers) => {
            state.should_quit = true;
        }
        TuiKeyCode::Char('r') if is_ctrl_shortcut(key.modifiers) => {
            state.toggle_latest_reasoning();
        }
        TuiKeyCode::Char(c) if !has_command_modifier(key.modifiers) => {
            state.composer.insert_char(c);
        }
        TuiKeyCode::Backspace => state.composer.delete_backward(),
        TuiKeyCode::Delete => state.composer.delete_forward(),
        TuiKeyCode::Left => state.composer.move_left(),
        TuiKeyCode::Right => state.composer.move_right(),
        TuiKeyCode::Home => state.composer.move_to_start(),
        TuiKeyCode::End => state.composer.move_to_end(),
        TuiKeyCode::Up => state.scroll_offset = state.scroll_offset.saturating_add(1),
        TuiKeyCode::Down => state.scroll_offset = state.scroll_offset.saturating_sub(1),
        _ => {}
    }
}

fn is_ctrl_shortcut(modifiers: TuiKeyModifiers) -> bool {
    modifiers.control && !modifiers.alt
}

fn has_command_modifier(modifiers: TuiKeyModifiers) -> bool {
    let has_ctrl_or_alt = modifiers.control || modifiers.alt;
    has_ctrl_or_alt && !is_altgr(modifiers)
}

#[cfg(windows)]
fn is_altgr(modifiers: TuiKeyModifiers) -> bool {
    modifiers.control && modifiers.alt
}

#[cfg(not(windows))]
fn is_altgr(_modifiers: TuiKeyModifiers) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_r_is_text() {
        let mut state = AppState::new();
        handle_key(
            &mut state,
            TuiKeyEvent::new(TuiKeyCode::Char('r'), TuiKeyModifiers::NONE),
        );

        assert_eq!(state.composer.text(), "r");
    }

    #[cfg(windows)]
    #[test]
    fn altgr_character_is_text_not_a_ctrl_shortcut() {
        let mut state = AppState::new();
        handle_key(
            &mut state,
            TuiKeyEvent::new(
                TuiKeyCode::Char('@'),
                TuiKeyModifiers::new(true, true, false),
            ),
        );

        assert_eq!(state.composer.text(), "@");
        assert!(!state.should_quit);
    }
}
