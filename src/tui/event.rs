use crossterm::event::{Event, KeyEvent, KeyEventKind};

/// Terminal events after transport-specific noise has been removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiEvent {
    Key(KeyEvent),
    Paste(String),
    Resize { columns: u16, rows: u16 },
}

impl TuiEvent {
    pub fn from_crossterm(event: Event) -> Option<Self> {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                Some(Self::Key(key))
            }
            Event::Paste(text) => Some(Self::Paste(text)),
            Event::Resize(columns, rows) => Some(Self::Resize { columns, rows }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
}
