use std::io::stdout;
use std::time::Duration;

use color_eyre::Result;
use crossterm::{
    event::{Event, EventStream, KeyCode},
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
async fn main() -> Result<()> {
    color_eyre::install()?;

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

    enable_raw_mode()?;
    let mut out = stdout();
    out.execute(EnterAlternateScreen)?;

    let result = run_app(config).await;

    let _ = disable_raw_mode();
    let _ = stdout().execute(LeaveAlternateScreen);

    result
}

async fn run_app(config: Config) -> Result<()> {
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    // Create app
    let mut app = App::new();
    app.add_system_message("Welcome to rua! Type a message and press Enter. Ctrl+C to quit.");

    // Create DeepSeek client
    let client = DeepSeekClient::new(config.deepseek)?;

    // Channel for UI events from background tasks
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();

    // Spawn crossterm event reader using EventStream (non-blocking)
    let tx_crossterm = tx.clone();
    let mut event_reader = EventStream::new();
    tokio::spawn(async move {
        while let Some(Ok(event)) = event_reader.next().await {
            let ui_event = match event {
                Event::Key(key) => UiEvent::Key(key),
                Event::Resize(cols, rows) => UiEvent::Resize(cols, rows),
                _ => UiEvent::Tick,
            };
            if tx_crossterm.send(ui_event).is_err() {
                break;
            }
        }
    });

    // Tick timer for spinner animation
    let tx_tick = tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(80));
        loop {
            interval.tick().await;
            if tx_tick.send(UiEvent::Tick).is_err() {
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
            UiEvent::Tick => {
                if app.status != rua::app::AppStatus::Idle {
                    app.spinner_frame = app.spinner_frame.wrapping_add(1);
                }
            }
            UiEvent::Key(key) => {
                if key.code == KeyCode::Enter && !app.is_streaming {
                    if let Some(_user_msg) = app.submit_input() {
                        app.status = rua::app::AppStatus::Waiting;
                        let client_clone = client.clone();
                        let messages = app.history_messages();
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
            UiEvent::Resize(cols, rows) => {
                let _ = (cols, rows); // Terminal::draw handles resize automatically
            }
        }

        if app.should_quit {
            if let Some(task) = stream_task.take() {
                task.abort();
            }
            break Ok(());
        }
    };

    result
}
