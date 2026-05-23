use crossterm::event::KeyEvent;

/// UI event sent from the main loop to the app
#[derive(Debug, Clone)]
pub enum UiEvent {
    Tick,
    Key(KeyEvent),
    Resize(u16, u16),
    StreamDelta(String),
    /// Reasoning content from DeepSeek reasoning models (not displayed, but preserved).
    ReasoningDelta(String),
    StreamDone,
    StreamError(String),
    /// The LLM decided to call a tool
    ToolCall { name: String, arguments: String },
    /// A tool finished executing
    ToolResult { name: String, output: String },
}
