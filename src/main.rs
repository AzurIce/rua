use std::io;

use crossterm::{
    event::{self, Event, KeyCode},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    Terminal,
};
use tokio::sync::mpsc;

use rua::app::{App, UiEvent};
use rua::config::Config;
use rua::deepseek::{DeepSeekClient, StreamEvent};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load config
    let config = Config::load().unwrap_or_else(|e| {
        eprintln!("Warning: failed to load config: {}", e);
        Config::default()
    });

    if config.deepseek.api_key.is_empty() {
        eprintln!("Error: DeepSeek API key is not set.");
        eprintln!(
            "Set it in {} as deepseek.api_key,",
            rua::config::config_path().display()
        );
        eprintln!("or use an env var like DEEPSEEK_API_KEY.");
        std::process::exit(1);
    }

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app
    let mut app = App::new();
    app.add_system_message("Welcome to rua! Type a message and press Enter. Ctrl+C to quit.");

    // Create DeepSeek client
    let client = DeepSeekClient::new(config.deepseek)?;

    // Channel for UI events from background tasks
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();

    // Spawn a task to read crossterm events and send them to the channel
    let tx_crossterm = tx.clone();
    tokio::spawn(async move {
        loop {
            if event::poll(std::time::Duration::from_millis(100)).unwrap_or(false) {
                if let Ok(Event::Key(key)) = event::read() {
                    if tx_crossterm.send(UiEvent::Key(key)).is_err() {
                        break;
                    }
                }
            }
            if tx_crossterm.send(UiEvent::Tick).is_err() {
                break;
            }
        }
    });

    let mut stream_task: Option<tokio::task::JoinHandle<()>> = None;

    let result = loop {
        terminal.draw(|f| app.draw(f))?;

        let Some(event) = rx.recv().await else {
            break Ok(());
        };

        match event {
            UiEvent::Tick => {}
            UiEvent::Key(key) => {
                if key.code == KeyCode::Enter && !app.is_streaming {
                    if let Some(_user_msg) = app.submit_input() {
                        let client_clone = client.clone();
                        let messages = app.messages.clone();
                        let tx_stream = tx.clone();
                        stream_task = Some(tokio::spawn(async move {
                            match client_clone.chat_stream(messages).await {
                                Ok(mut stream) => {
                                    while let Some(event) = stream.next().await {
                                        let ui_event = match event {
                                            StreamEvent::TextDelta(delta) => {
                                                UiEvent::StreamDelta(delta)
                                            }
                                            StreamEvent::Done => UiEvent::StreamDone,
                                            StreamEvent::Error(e) => {
                                                UiEvent::StreamError(e)
                                            }
                                        };
                                        let _ = tx_stream.send(ui_event);
                                    }
                                }
                                Err(e) => {
                                    let _ = tx_stream.send(UiEvent::StreamError(format!(
                                        "Request failed: {}",
                                        e
                                    )));
                                }
                            }
                        }));
                    }
                } else {
                    app.on_key(key);
                }
            }
            UiEvent::StreamDelta(delta) => {
                app.append_delta(&delta);
            }
            UiEvent::StreamDone => {
                app.finish_stream();
            }
            UiEvent::StreamError(e) => {
                app.add_error(&e);
            }
            UiEvent::Resize(_, _) => {}
        }

        if app.should_quit {
            if let Some(task) = stream_task.take() {
                task.abort();
            }
            break Ok(());
        }
    };

    // Restore terminal
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;

    result
}
