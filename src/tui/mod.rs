//! Terminal-facing infrastructure and input models.
//!
//! This module deliberately does not own agent or conversation state. It
//! normalizes terminal events, owns terminal modes, and provides editor state
//! that the application can project into a view.

mod composer;
mod diagnostics;
mod event;
mod terminal;

pub use composer::{Composer, ComposerViewport};
pub use diagnostics::InputTrace;
pub use event::{TuiEvent, TuiKeyCode, TuiKeyEvent, TuiKeyModifiers};
pub use terminal::TerminalSession;
