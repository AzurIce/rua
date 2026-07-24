use std::io::{self, stdout};

use crossterm::{
    ExecutableCommand,
    cursor::Show,
    event::{DisableBracketedPaste, EnableBracketedPaste},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

/// Owns terminal modes and restores them on normal return or panic unwind.
pub struct TerminalSession {
    raw_mode: bool,
    alternate_screen: bool,
    bracketed_paste: bool,
}

impl TerminalSession {
    pub fn enter() -> io::Result<Self> {
        let mut session = Self {
            raw_mode: false,
            alternate_screen: false,
            bracketed_paste: false,
        };

        enable_raw_mode()?;
        session.raw_mode = true;

        stdout().execute(EnterAlternateScreen)?;
        session.alternate_screen = true;

        stdout().execute(EnableBracketedPaste)?;
        session.bracketed_paste = true;

        Ok(session)
    }

    fn restore(&mut self) {
        if self.bracketed_paste {
            let _ = stdout().execute(DisableBracketedPaste);
            self.bracketed_paste = false;
        }
        let _ = stdout().execute(Show);
        if self.alternate_screen {
            let _ = stdout().execute(LeaveAlternateScreen);
            self.alternate_screen = false;
        }
        if self.raw_mode {
            let _ = disable_raw_mode();
            self.raw_mode = false;
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.restore();
    }
}
