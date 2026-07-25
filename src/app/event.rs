use super::command::CompletionResponse;
use crate::agent::RuntimeEvent;
use crate::tui::TuiEvent;

/// UI event sent from the main loop to the app
#[derive(Debug, Clone)]
pub enum UiEvent {
    Tick,
    Terminal(TuiEvent),
    TerminalFailure(String),
    Runtime(RuntimeEvent),
    Completion(CompletionResponse),
}
