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

    pub fn leave(mut self) -> io::Result<()> {
        self.restore()
    }

    fn restore(&mut self) -> io::Result<()> {
        let mut first_error = None;
        if self.bracketed_paste {
            record_first_error(
                &mut first_error,
                stdout().execute(DisableBracketedPaste).map(|_| ()),
            );
            self.bracketed_paste = false;
        }
        record_first_error(&mut first_error, stdout().execute(Show).map(|_| ()));
        if self.alternate_screen {
            record_first_error(
                &mut first_error,
                stdout().execute(LeaveAlternateScreen).map(|_| ()),
            );
            self.alternate_screen = false;
        }
        if self.raw_mode {
            record_first_error(&mut first_error, disable_raw_mode());
            self.raw_mode = false;
        }
        first_error.map_or(Ok(()), Err)
    }
}

fn record_first_error(first_error: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result
        && first_error.is_none()
    {
        *first_error = Some(error);
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
