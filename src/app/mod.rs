pub mod event;
pub mod input;
pub mod render;
pub mod state;

// Backwards-compatible re-exports
pub use event::UiEvent;
pub use state::{AppState, AppStatus};

/// Alias for backwards compatibility with code using `App`.
pub type App = AppState;
