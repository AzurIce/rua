use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use crossterm::event::{Event, KeyCode};

pub struct InputTrace {
    file: File,
    started: Instant,
}

impl InputTrace {
    pub fn from_env(project_root: &Path) -> Result<Option<Self>, std::io::Error> {
        if std::env::var_os("RUA_INPUT_TRACE").is_none() {
            return Ok(None);
        }
        let directory = project_root.join(".rua");
        std::fs::create_dir_all(&directory)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("input-trace.log"))?;
        writeln!(
            file,
            "trace_start terminal={:?} term_program={:?} wt_session={} os={}",
            std::env::var_os("TERM"),
            std::env::var_os("TERM_PROGRAM"),
            std::env::var_os("WT_SESSION").is_some(),
            std::env::consts::OS
        )?;
        file.flush()?;
        Ok(Some(Self {
            file,
            started: Instant::now(),
        }))
    }

    pub fn record(&mut self, event: &Event) -> Result<(), std::io::Error> {
        writeln!(
            self.file,
            "t_us={} {}",
            self.started.elapsed().as_micros(),
            describe_event(event)
        )?;
        self.file.flush()
    }
}

fn describe_event(event: &Event) -> String {
    match event {
        Event::Key(key) => format!(
            "key kind={:?} code={} modifiers={:?}",
            key.kind,
            key_code_class(&key.code),
            key.modifiers
        ),
        Event::Paste(text) => format!(
            "paste bytes={} lines={}",
            text.len(),
            text.bytes().filter(|byte| *byte == b'\n').count() + 1
        ),
        Event::Resize(columns, rows) => format!("resize columns={columns} rows={rows}"),
        Event::FocusGained => "focus gained".to_owned(),
        Event::FocusLost => "focus lost".to_owned(),
        Event::Mouse(mouse) => format!(
            "mouse kind={:?} modifiers={:?}",
            mouse.kind, mouse.modifiers
        ),
    }
}

fn key_code_class(code: &KeyCode) -> &'static str {
    match code {
        KeyCode::Char(_) => "char",
        KeyCode::Backspace => "backspace",
        KeyCode::Enter => "enter",
        KeyCode::Left => "left",
        KeyCode::Right => "right",
        KeyCode::Up => "up",
        KeyCode::Down => "down",
        KeyCode::Home => "home",
        KeyCode::End => "end",
        KeyCode::PageUp => "page_up",
        KeyCode::PageDown => "page_down",
        KeyCode::Tab => "tab",
        KeyCode::BackTab => "back_tab",
        KeyCode::Delete => "delete",
        KeyCode::Insert => "insert",
        KeyCode::F(_) => "function",
        KeyCode::Null => "null",
        KeyCode::Esc => "escape",
        KeyCode::CapsLock => "caps_lock",
        KeyCode::ScrollLock => "scroll_lock",
        KeyCode::NumLock => "num_lock",
        KeyCode::PrintScreen => "print_screen",
        KeyCode::Pause => "pause",
        KeyCode::Menu => "menu",
        KeyCode::KeypadBegin => "keypad_begin",
        KeyCode::Media(_) => "media",
        KeyCode::Modifier(_) => "modifier",
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEvent, KeyModifiers};

    use super::*;

    #[test]
    fn trace_does_not_record_typed_character_content() {
        let description = describe_event(&Event::Key(KeyEvent::new(
            KeyCode::Char('秘'),
            KeyModifiers::NONE,
        )));

        assert!(description.contains("code=char"));
        assert!(!description.contains('秘'));
    }

    #[test]
    fn paste_trace_records_shape_not_content() {
        let description = describe_event(&Event::Paste("secret\ntext".to_owned()));

        assert_eq!(description, "paste bytes=11 lines=2");
        assert!(!description.contains("secret"));
    }
}
