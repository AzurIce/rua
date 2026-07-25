use std::collections::VecDeque;
use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;

use color_eyre::Result;
use crossterm::event::EventStream;
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;

use rua::agent::{
    AgentRuntime, ApiFamily, BashTool, DeepSeekProvider, LocalSessionStore, ModelRef, ProviderId,
    ReconciliationDecision, RuntimeEvent, SessionId, ToolCallId, ToolRegistry,
    register_coding_tools,
};
use rua::app::{App, AppCommand, AppController, FrameScheduler, UiEvent};
use rua::config::Config;
use rua::tui::{InputTrace, TerminalSession, TuiEvent};

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let options = startup_options()?;
    if let Some(action) = &options.maintenance {
        run_maintenance(action).await?;
        return Ok(());
    }

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

    let terminal_session = TerminalSession::enter()?;
    let app_result = run_app(config, options).await;
    let restore_result = terminal_session.leave();
    match (app_result, restore_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn run_app(config: Config, options: StartupOptions) -> Result<()> {
    let project_root = std::env::current_dir()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    // UI state
    let mut app = App::new();
    app.add_system_message("rua ready");
    let mut controller = AppController::new(app);

    // Terminal input is kept separate from runtime projection events so input
    // and cancellation can remain responsive under a delta-heavy stream.
    let (terminal_tx, mut terminal_rx) = mpsc::unbounded_channel::<UiEvent>();

    // Background: crossterm events
    let tx_crossterm = terminal_tx;
    let mut event_reader = EventStream::new();
    let mut input_trace = InputTrace::from_env(&project_root)?;
    tokio::spawn(async move {
        loop {
            match event_reader.next().await {
                Some(Ok(event)) => {
                    if input_trace
                        .as_mut()
                        .is_some_and(|trace| trace.record(&event).is_err())
                    {
                        input_trace = None;
                    }
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

    // Agent core: canonical conversation, provider adapter and tool runtime.
    let provider = Arc::new(DeepSeekProvider::new(&config.deepseek)?);
    let mut tools = ToolRegistry::new();
    tools.register(BashTool::for_workspace(&project_root)?)?;
    register_coding_tools(&mut tools, &project_root)?;
    let tools = Arc::new(tools);
    let model = ModelRef {
        provider: ProviderId::new("deepseek"),
        api_family: ApiFamily::new("openai-chat"),
        model: config.deepseek.model.clone(),
    };
    let store = Arc::new(LocalSessionStore::new(project_root));
    let recovered = options.session.is_some();
    let mut runtime = Arc::new(if let Some(session_id) = options.session {
        AgentRuntime::recover(
            provider.clone(),
            tools.clone(),
            model.clone(),
            store.clone(),
            session_id,
        )
        .await?
    } else {
        AgentRuntime::new(
            provider.clone(),
            tools.clone(),
            "You are rua, an AI coding agent.",
            model.clone(),
        )
        .with_session_store(store.clone())
    });
    if !recovered {
        controller
            .state_mut()
            .add_system_message(&format!("session {}", runtime.session_id()));
    }
    let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
    if recovered {
        runtime.publish_recovery(&runtime_tx).await;
    }

    let mut stream_task: Option<StreamTask> = None;
    let mut runtime_buffer = VecDeque::new();

    let mut frames = FrameScheduler::new(tokio::time::Instant::now());
    loop {
        let now = tokio::time::Instant::now();
        if frames.should_draw(now) {
            terminal.draw(|frame| rua::app::render::draw(controller.state(), frame))?;
            frames.frame_drawn(now);
        }

        let animated = controller.state().status != rua::app::AppStatus::Idle;
        let deadline = frames.deadline(tokio::time::Instant::now(), animated);
        let mut deadline_fired = false;
        let event = tokio::select! {
            biased;
            event = terminal_rx.recv() => event,
            event = async { runtime_buffer.pop_front() }, if !runtime_buffer.is_empty() => {
                event.map(UiEvent::Runtime)
            }
            event = runtime_rx.recv() => {
                if let Some(event) = event {
                    merge_runtime_event(&mut runtime_buffer, event);
                    drain_runtime_events(&mut runtime_rx, &mut runtime_buffer);
                }
                runtime_buffer.pop_front().map(UiEvent::Runtime)
            }
            _ = wait_for_deadline(deadline) => {
                deadline_fired = true;
                None
            }
        };
        if deadline_fired {
            let now = tokio::time::Instant::now();
            if frames.on_deadline(now, animated) {
                controller.advance_spinner();
                frames.request_frame();
            }
            continue;
        }
        let Some(event) = event else {
            break Ok(());
        };
        for command in controller.handle(event) {
            match command {
                AppCommand::SubmitUserInput(text) => {
                    stream_task = Some(spawn_user_turn(
                        Arc::clone(&runtime),
                        runtime_tx.clone(),
                        text,
                    ));
                }
                AppCommand::ResumeTurn => {
                    stream_task = Some(spawn_resume(Arc::clone(&runtime), runtime_tx.clone()));
                }
                AppCommand::InspectRecovery => {
                    let tools = runtime.recovery_tools().await;
                    if tools.is_empty() {
                        controller
                            .state_mut()
                            .add_system_message("no tool reconciliation is pending");
                    } else {
                        for tool in tools {
                            controller.state_mut().add_system_message(&format!(
                                "{} {}({}) replay={:?}",
                                tool.tool_call_id, tool.name, tool.arguments, tool.replay_class
                            ));
                        }
                    }
                }
                AppCommand::ListSessions => match store.list_session_ids() {
                    Ok(session_ids) => {
                        controller
                            .state_mut()
                            .set_session_ids(session_ids.iter().map(ToString::to_string).collect());
                        if session_ids.is_empty() {
                            controller
                                .state_mut()
                                .add_system_message("no local sessions");
                        } else {
                            for session_id in session_ids {
                                controller
                                    .state_mut()
                                    .add_system_message(session_id.as_str());
                            }
                        }
                    }
                    Err(error) => controller.state_mut().add_error(&error.to_string()),
                },
                AppCommand::LoadSession(session_id) => {
                    if stream_task.is_some() {
                        controller
                            .state_mut()
                            .add_error("cannot load a session while a turn is active");
                    } else {
                        match AgentRuntime::recover(
                            provider.clone(),
                            tools.clone(),
                            model.clone(),
                            store.clone(),
                            session_id,
                        )
                        .await
                        {
                            Ok(next) => {
                                runtime = Arc::new(next);
                                runtime.publish_recovery(&runtime_tx).await;
                            }
                            Err(error) => controller.state_mut().add_error(&error.to_string()),
                        }
                    }
                }
                AppCommand::RequestSessionCompletions(request) => {
                    let response = match store.list_session_ids() {
                        Ok(session_ids) => rua::app::CompletionResponse {
                            request_id: request.request_id,
                            draft_revision: request.draft_revision,
                            cursor: request.cursor,
                            context_revision: request.context_revision,
                            candidates: session_ids
                                .into_iter()
                                .filter(|session_id| session_id.as_str().contains(&request.query))
                                .take(32)
                                .map(|session_id| rua::app::command::CompletionItem {
                                    stable_key: format!("session:{session_id}"),
                                    label: session_id.to_string(),
                                    detail: "local session".to_owned(),
                                    replacement: session_id.to_string(),
                                    replacement_range: request.replacement_range.clone(),
                                    kind: rua::app::command::CompletionKind::Resource,
                                    disabled_reason: None,
                                })
                                .collect(),
                            error: None,
                        },
                        Err(error) => rua::app::CompletionResponse {
                            request_id: request.request_id,
                            draft_revision: request.draft_revision,
                            cursor: request.cursor,
                            context_revision: request.context_revision,
                            candidates: Vec::new(),
                            error: Some(error.to_string()),
                        },
                    };
                    controller.handle(UiEvent::Completion(response));
                }
                AppCommand::ReconcileTool {
                    tool_call_id,
                    decision,
                } => {
                    stream_task = Some(spawn_reconciliation(
                        Arc::clone(&runtime),
                        runtime_tx.clone(),
                        tool_call_id,
                        decision,
                    ));
                }
                AppCommand::CancelTurn => {
                    if let Some((_, cancel)) = &stream_task {
                        cancel.cancel();
                    }
                }
                AppCommand::Quit => {
                    if let Some((task, cancel)) = stream_task.take() {
                        cancel.cancel();
                        task.abort();
                    }
                    return Ok(());
                }
                AppCommand::TerminalFailed(error) => {
                    return Err(color_eyre::eyre::eyre!(error));
                }
            }
        }
        frames.request_frame();
    }
}

fn drain_runtime_events(
    runtime_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    buffer: &mut VecDeque<RuntimeEvent>,
) {
    for _ in 0..256 {
        let Ok(event) = runtime_rx.try_recv() else {
            break;
        };
        merge_runtime_event(buffer, event);
    }
}

fn merge_runtime_event(buffer: &mut VecDeque<RuntimeEvent>, event: RuntimeEvent) {
    match event {
        RuntimeEvent::TextDelta { turn_id, delta } => {
            for pending in buffer.iter_mut().rev() {
                match pending {
                    RuntimeEvent::TextDelta {
                        turn_id: previous_turn,
                        delta: previous_delta,
                    } if *previous_turn == turn_id => {
                        previous_delta.push_str(&delta);
                        return;
                    }
                    RuntimeEvent::ReasoningDelta {
                        turn_id: previous_turn,
                        ..
                    } if *previous_turn == turn_id => continue,
                    _ => break,
                }
            }
            buffer.push_back(RuntimeEvent::TextDelta { turn_id, delta });
        }
        RuntimeEvent::ReasoningDelta { turn_id, delta } => {
            for pending in buffer.iter_mut().rev() {
                match pending {
                    RuntimeEvent::ReasoningDelta {
                        turn_id: previous_turn,
                        delta: previous_delta,
                    } if *previous_turn == turn_id => {
                        previous_delta.push_str(&delta);
                        return;
                    }
                    RuntimeEvent::TextDelta {
                        turn_id: previous_turn,
                        ..
                    } if *previous_turn == turn_id => continue,
                    _ => break,
                }
            }
            buffer.push_back(RuntimeEvent::ReasoningDelta { turn_id, delta });
        }
        event => buffer.push_back(event),
    }
}

async fn wait_for_deadline(deadline: Option<tokio::time::Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    } else {
        std::future::pending::<()>().await;
    }
}

type StreamTask = (
    tokio::task::JoinHandle<()>,
    tokio_util::sync::CancellationToken,
);

fn spawn_user_turn(
    runtime: Arc<AgentRuntime>,
    runtime_tx: mpsc::UnboundedSender<RuntimeEvent>,
    text: String,
) -> StreamTask {
    let cancel = tokio_util::sync::CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        if let Err(rua::agent::RuntimeError::Persistence(error)) =
            runtime.run_user_turn(text, &runtime_tx, task_cancel).await
        {
            let _ = runtime_tx.send(RuntimeEvent::PersistenceFailed {
                message: error.to_string(),
            });
        }
    });
    (task, cancel)
}

fn spawn_resume(
    runtime: Arc<AgentRuntime>,
    runtime_tx: mpsc::UnboundedSender<RuntimeEvent>,
) -> StreamTask {
    let cancel = tokio_util::sync::CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        if let Err(rua::agent::RuntimeError::Persistence(error)) =
            runtime.resume_turn(&runtime_tx, task_cancel).await
        {
            let _ = runtime_tx.send(RuntimeEvent::PersistenceFailed {
                message: error.to_string(),
            });
        }
    });
    (task, cancel)
}

fn spawn_reconciliation(
    runtime: Arc<AgentRuntime>,
    runtime_tx: mpsc::UnboundedSender<RuntimeEvent>,
    tool_call_id: ToolCallId,
    decision: ReconciliationDecision,
) -> StreamTask {
    let cancel = tokio_util::sync::CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        if let Err(error) = runtime
            .reconcile_tool(tool_call_id, decision, &runtime_tx, task_cancel)
            .await
        {
            let event = match error {
                rua::agent::RuntimeError::Persistence(error) => RuntimeEvent::PersistenceFailed {
                    message: error.to_string(),
                },
                error => RuntimeEvent::OperationFailed {
                    message: error.to_string(),
                },
            };
            let _ = runtime_tx.send(event);
        }
    });
    (task, cancel)
}

struct StartupOptions {
    session: Option<SessionId>,
    maintenance: Option<MaintenanceAction>,
}

enum MaintenanceAction {
    Validate {
        session_id: SessionId,
    },
    Export {
        session_id: SessionId,
        destination: PathBuf,
    },
    Repair {
        session_id: SessionId,
    },
}

fn startup_options() -> Result<StartupOptions> {
    let mut args = std::env::args().skip(1);
    let mut session = None;
    let mut maintenance = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--session" => {
                let value = args
                    .next()
                    .ok_or_else(|| color_eyre::eyre::eyre!("--session requires a session id"))?;
                session = Some(SessionId::new(value));
            }
            "--validate-session" => {
                let value = args.next().ok_or_else(|| {
                    color_eyre::eyre::eyre!("--validate-session requires a session id")
                })?;
                maintenance = Some(MaintenanceAction::Validate {
                    session_id: SessionId::new(value),
                });
            }
            "--export-session" => {
                let session_id = args.next().ok_or_else(|| {
                    color_eyre::eyre::eyre!("--export-session requires a session id")
                })?;
                let destination = args.next().ok_or_else(|| {
                    color_eyre::eyre::eyre!("--export-session requires a destination directory")
                })?;
                maintenance = Some(MaintenanceAction::Export {
                    session_id: SessionId::new(session_id),
                    destination: PathBuf::from(destination),
                });
            }
            "--repair-session" => {
                let value = args.next().ok_or_else(|| {
                    color_eyre::eyre::eyre!("--repair-session requires a session id")
                })?;
                maintenance = Some(MaintenanceAction::Repair {
                    session_id: SessionId::new(value),
                });
            }
            "--help" | "-h" => {
                println!(
                    "Usage: rua [--session <session-id>]\n       rua --validate-session <session-id>\n       rua --export-session <session-id> <destination>\n       rua --repair-session <session-id>"
                );
                std::process::exit(0);
            }
            _ => {
                return Err(color_eyre::eyre::eyre!("unknown argument: {argument}"));
            }
        }
    }
    if maintenance.is_some() && session.is_some() {
        color_eyre::eyre::bail!("maintenance commands cannot be combined with --session");
    }
    Ok(StartupOptions {
        session,
        maintenance,
    })
}

async fn run_maintenance(action: &MaintenanceAction) -> Result<()> {
    let store = LocalSessionStore::new(std::env::current_dir()?);
    match action {
        MaintenanceAction::Validate { session_id } => {
            let recovered = store.validate_session(session_id).await?;
            println!(
                "session={} sequence={} revision={} phase={:?}",
                recovered.session_id,
                recovered.last_sequence.0,
                recovered.conversation.revision().0,
                recovered.active_turn.as_ref().map(|turn| &turn.phase)
            );
        }
        MaintenanceAction::Export {
            session_id,
            destination,
        } => {
            let exported = store.export_raw(session_id, destination)?;
            println!("exported session to {}", exported.display());
        }
        MaintenanceAction::Repair { session_id } => {
            let report = store.repair_incomplete_tail(session_id)?;
            println!(
                "repaired session {}; removed {} incomplete bytes; original saved at {}",
                session_id,
                report.removed_bytes,
                report.backup_path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_projection_coalesces_deltas_until_a_lifecycle_boundary() {
        let turn_id: rua::agent::TurnId = "turn-1".into();
        let mut buffer = VecDeque::new();
        merge_runtime_event(
            &mut buffer,
            RuntimeEvent::TextDelta {
                turn_id: turn_id.clone(),
                delta: "a".to_owned(),
            },
        );
        merge_runtime_event(
            &mut buffer,
            RuntimeEvent::ReasoningDelta {
                turn_id: turn_id.clone(),
                delta: "r".to_owned(),
            },
        );
        merge_runtime_event(
            &mut buffer,
            RuntimeEvent::TextDelta {
                turn_id: turn_id.clone(),
                delta: "b".to_owned(),
            },
        );
        merge_runtime_event(
            &mut buffer,
            RuntimeEvent::TurnCompleted {
                turn_id: turn_id.clone(),
            },
        );
        merge_runtime_event(
            &mut buffer,
            RuntimeEvent::TextDelta {
                turn_id,
                delta: "c".to_owned(),
            },
        );

        assert_eq!(buffer.len(), 4);
        assert!(matches!(
            &buffer[0],
            RuntimeEvent::TextDelta { delta, .. } if delta == "ab"
        ));
        assert!(matches!(
            &buffer[1],
            RuntimeEvent::ReasoningDelta { delta, .. } if delta == "r"
        ));
        assert!(matches!(buffer[2], RuntimeEvent::TurnCompleted { .. }));
        assert!(matches!(
            &buffer[3],
            RuntimeEvent::TextDelta { delta, .. } if delta == "c"
        ));
    }
}
