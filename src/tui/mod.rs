//! Terminal-facing infrastructure and input models.
//!
//! This module deliberately does not own agent or conversation state. It
//! normalizes terminal events, owns terminal modes, and provides editor state
//! that the application can project into a view.

mod composer;
mod event;
mod terminal;

pub use composer::{Composer, ComposerViewport};
pub use event::TuiEvent;
pub use terminal::TerminalSession;
