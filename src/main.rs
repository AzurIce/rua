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
use rua::deepseek::DeepSeekClient;
use rua::session::Session;

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

    // UI state
    let mut app = App::new();
    app.add_system_message("Welcome to rua! Type a message and press Enter. Ctrl+C to quit.");

    // Event channel
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();

    // Background: crossterm events
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

    // Background: tick timer (spinner animation)
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

    // Session layer: manages LLM requests
    let client = DeepSeekClient::new(config.deepseek)?;
    let session = Session::new(client, tx.clone());

    let mut stream_task: Option<tokio::task::JoinHandle<()>> = None;

    let result = loop {
        terminal.draw(|f| rua::app::render::draw(&app, f))?;

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
                    if app.submit_input().is_some() {
                        app.status = rua::app::AppStatus::Waiting;
                        let messages: Vec<rua::deepseek::Message> = app
                            .history_entries()
                            .into_iter()
                            .filter(|e| e.role != rua::model::Role::System)
                            .map(Into::into)
                            .collect();
                        stream_task = Some(session.send_message(messages));
                    }
                } else {
                    rua::app::input::handle_key(&mut app, key);
                }
            }
            UiEvent::StreamDelta(delta) => app.append_delta(&delta),
            UiEvent::StreamDone => app.finish_stream(),
            UiEvent::StreamError(e) => app.add_error(&e),
            UiEvent::Resize(_, _) => {} // Terminal::draw handles resize automatically
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
