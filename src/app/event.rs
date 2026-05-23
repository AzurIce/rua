use crossterm::event::KeyEvent;

/// UI event sent from the main loop to the app
#[derive(Debug, Clone)]
pub enum UiEvent {
    Tick,
    Key(KeyEvent),
    Resize(u16, u16),
    StreamDelta(String),
    StreamDone,
    StreamError(String),
}
