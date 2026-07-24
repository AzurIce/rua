use std::io::stdout;
use std::sync::Arc;
use std::time::Duration;

use color_eyre::Result;
use crossterm::event::{EventStream, KeyCode, KeyModifiers};
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;

use rua::agent::{
    AgentRuntime, ApiFamily, BashTool, DeepSeekProvider, ModelRef, ProviderId, ToolRegistry,
};
use rua::app::{App, UiEvent};
use rua::config::Config;
use rua::tui::{TerminalSession, TuiEvent};

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

    let _terminal_session = TerminalSession::enter()?;
    run_app(config).await
}

async fn run_app(config: Config) -> Result<()> {
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    // UI state
    let mut app = App::new();
    app.add_system_message("rua ready");

    // Event channel
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();

    // Background: crossterm events
    let tx_crossterm = tx.clone();
    let mut event_reader = EventStream::new();
    tokio::spawn(async move {
        loop {
            match event_reader.next().await {
                Some(Ok(event)) => {
                    let Some(event) = TuiEvent::from_crossterm(event) else {
                        continue;
                    };
                    if tx_crossterm.send(UiEvent::Terminal(event)).is_err() {
                        break;
                    }
                }
                Some(Err(error)) => {
                    let _ = tx_crossterm.send(UiEvent::TerminalFailure(error.to_string()));
                    break;
                }
                None => {
                    let _ = tx_crossterm.send(UiEvent::TerminalFailure(
                        "terminal event stream closed".to_string(),
                    ));
                    break;
                }
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

    // Agent core: canonical conversation, provider adapter and tool runtime.
    let provider = Arc::new(DeepSeekProvider::new(&config.deepseek)?);
    let mut tools = ToolRegistry::new();
    tools.register(BashTool::default())?;
    let runtime = Arc::new(AgentRuntime::new(
        provider,
        Arc::new(tools),
        "You are rua, an AI coding agent.",
        ModelRef {
            provider: ProviderId::new("deepseek"),
            api_family: ApiFamily::new("openai-chat"),
            model: config.deepseek.model.clone(),
        },
    ));
    let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
    let tx_runtime = tx.clone();
    tokio::spawn(async move {
        while let Some(event) = runtime_rx.recv().await {
            if tx_runtime.send(UiEvent::Runtime(event)).is_err() {
                break;
            }
        }
    });

    let mut stream_task: Option<(
        tokio::task::JoinHandle<()>,
        tokio_util::sync::CancellationToken,
    )> = None;

    loop {
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
            UiEvent::Terminal(event) => match event {
                TuiEvent::Key(key) => {
                    if key.code == KeyCode::Enter && !app.is_streaming && !app.can_resume_turn {
                        if let Some(text) = app.submit_input() {
                            let runtime = Arc::clone(&runtime);
                            let runtime_tx = runtime_tx.clone();
                            let cancel = tokio_util::sync::CancellationToken::new();
                            let task_cancel = cancel.clone();
                            stream_task = Some((
                                tokio::spawn(async move {
                                    let _ =
                                        runtime.run_user_turn(text, &runtime_tx, task_cancel).await;
                                }),
                                cancel,
                            ));
                        }
                    } else if is_ctrl_r(key.code, key.modifiers)
                        && !app.is_streaming
                        && app.can_resume_turn
                    {
                        app.begin_resume();
                        let runtime = Arc::clone(&runtime);
                        let runtime_tx = runtime_tx.clone();
                        let cancel = tokio_util::sync::CancellationToken::new();
                        let task_cancel = cancel.clone();
                        stream_task = Some((
                            tokio::spawn(async move {
                                let _ = runtime.resume_turn(&runtime_tx, task_cancel).await;
                            }),
                            cancel,
                        ));
                    } else if key.code == KeyCode::Enter && !app.is_streaming {
                        // A resumable failed turn must be explicitly retried first.
                    } else {
                        rua::app::input::handle_key(&mut app, key);
                    }
                }
                TuiEvent::Paste(text) => app.composer.insert_str(&text),
                TuiEvent::Resize { .. } => {}
            },
            UiEvent::TerminalFailure(error) => {
                break Err(color_eyre::eyre::eyre!(error));
            }
            UiEvent::Runtime(event) => app.apply_runtime_event(&event),
        }

        if app.should_quit {
            if let Some((task, cancel)) = stream_task.take() {
                cancel.cancel();
                task.abort();
            }
            break Ok(());
        }
    }
}

fn is_ctrl_r(code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(code, KeyCode::Char('r'))
        && modifiers.contains(KeyModifiers::CONTROL)
        && !modifiers.contains(KeyModifiers::ALT)
}
