use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TuiKeyEvent {
    pub code: TuiKeyCode,
    pub modifiers: TuiKeyModifiers,
}

impl TuiKeyEvent {
    pub const fn new(code: TuiKeyCode, modifiers: TuiKeyModifiers) -> Self {
        Self { code, modifiers }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiKeyCode {
    Char(char),
    Backspace,
    Tab,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Delete,
    Escape,
    Other,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TuiKeyModifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
}

impl TuiKeyModifiers {
    pub const NONE: Self = Self {
        control: false,
        alt: false,
        shift: false,
    };

    pub const fn new(control: bool, alt: bool, shift: bool) -> Self {
        Self {
            control,
            alt,
            shift,
        }
    }
}

/// Terminal events after transport-specific noise has been removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiEvent {
    Key(TuiKeyEvent),
    Paste(String),
    Resize { columns: u16, rows: u16 },
    MouseScroll { up: bool },
}

impl TuiEvent {
    pub fn from_crossterm(event: Event) -> Option<Self> {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                Some(Self::Key(normalize_key(key)))
            }
            Event::Paste(text) => Some(Self::Paste(text)),
            Event::Resize(columns, rows) => Some(Self::Resize { columns, rows }),
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => Some(Self::MouseScroll { up: true }),
                MouseEventKind::ScrollDown => Some(Self::MouseScroll { up: false }),
                _ => None,
            },
            _ => None,
        }
    }
}

fn normalize_key(key: KeyEvent) -> TuiKeyEvent {
    let code = match key.code {
        KeyCode::Char(value) => TuiKeyCode::Char(value),
        KeyCode::Backspace => TuiKeyCode::Backspace,
        KeyCode::Tab => TuiKeyCode::Tab,
        KeyCode::Enter => TuiKeyCode::Enter,
        KeyCode::Left => TuiKeyCode::Left,
        KeyCode::Right => TuiKeyCode::Right,
        KeyCode::Up => TuiKeyCode::Up,
        KeyCode::Down => TuiKeyCode::Down,
        KeyCode::Home => TuiKeyCode::Home,
        KeyCode::End => TuiKeyCode::End,
        KeyCode::Delete => TuiKeyCode::Delete,
        KeyCode::Esc => TuiKeyCode::Escape,
        _ => TuiKeyCode::Other,
    };
    TuiKeyEvent {
        code,
        modifiers: TuiKeyModifiers {
            control: key.modifiers.contains(KeyModifiers::CONTROL),
            alt: key.modifiers.contains(KeyModifiers::ALT),
            shift: key.modifiers.contains(KeyModifiers::SHIFT),
        },
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};

    use super::*;

    #[test]
    fn release_events_are_dropped_at_the_terminal_boundary() {
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );

        assert_eq!(TuiEvent::from_crossterm(Event::Key(release)), None);
    }

    #[test]
    fn paste_is_not_decomposed_into_key_events() {
        assert_eq!(
            TuiEvent::from_crossterm(Event::Paste("你好\nworld".to_string())),
            Some(TuiEvent::Paste("你好\nworld".to_string()))
        );
    }

    #[test]
    fn mouse_wheel_is_normalized_without_leaking_crossterm_types() {
        let event = Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 4,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(
            TuiEvent::from_crossterm(event),
            Some(TuiEvent::MouseScroll { up: true })
        );
    }
}
