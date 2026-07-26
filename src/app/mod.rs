pub mod command;
pub mod controller;
pub mod event;
pub mod frame;
pub mod input;
pub mod render;
pub mod state;

// Backwards-compatible re-exports
pub use command::{CommandId, CommandRegistry, CompletionRequest, CompletionResponse};
pub use controller::{AppCommand, AppController};
pub use event::UiEvent;
pub use frame::FrameScheduler;
pub use state::{AppState, AppStatus, TreeOverlayItem, TreeOverlayState};

/// Alias for backwards compatibility with code using `App`.
pub type App = AppState;
