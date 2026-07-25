use crate::agent::{ReconciliationDecision, SessionId, ToolCallId};
use crate::tui::{TuiEvent, TuiKeyCode, TuiKeyModifiers};

use super::{
    AppState, AppStatus, CommandId, CommandRegistry, UiEvent,
    command::{CommandInvocation, InputClassification, ParseState},
    input,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppCommand {
    SubmitUserInput(String),
    ResumeTurn,
    InspectRecovery,
    InspectApproval,
    ListSessions,
    LoadSession(SessionId),
    RequestSessionCompletions(super::command::CompletionRequest),
    ReconcileTool {
        tool_call_id: ToolCallId,
        decision: ReconciliationDecision,
    },
    ResolveApproval {
        tool_call_id: ToolCallId,
        approved: bool,
        reason: Option<String>,
    },
    CancelTurn,
    Quit,
    TerminalFailed(String),
}

pub struct AppController {
    state: AppState,
    registry: CommandRegistry,
}

impl AppController {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            registry: CommandRegistry::builtins(),
        }
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut AppState {
        &mut self.state
    }

    pub fn handle(&mut self, event: UiEvent) -> Vec<AppCommand> {
        match event {
            UiEvent::Tick => {
                self.advance_spinner();
                Vec::new()
            }
            UiEvent::Terminal(event) => self.handle_terminal(event),
            UiEvent::TerminalFailure(error) => vec![AppCommand::TerminalFailed(error)],
            UiEvent::Runtime(event) => {
                self.state.apply_runtime_event(&event);
                self.state.refresh_command_assist(&self.registry);
                Vec::new()
            }
            UiEvent::Completion(response) => {
                self.state.apply_completion_response(response);
                Vec::new()
            }
        }
    }

    pub fn advance_spinner(&mut self) {
        if self.state.status != AppStatus::Idle {
            self.state.spinner_frame = self.state.spinner_frame.wrapping_add(1);
        }
    }

    fn handle_terminal(&mut self, event: TuiEvent) -> Vec<AppCommand> {
        match event {
            TuiEvent::Key(key) => {
                if is_ctrl_c(key.code, key.modifiers) && self.state.is_streaming {
                    return vec![AppCommand::CancelTurn];
                }
                if self.state.command_assist.open {
                    match key.code {
                        TuiKeyCode::Escape => {
                            self.state.close_command_assist();
                            return Vec::new();
                        }
                        TuiKeyCode::Up => {
                            self.state.move_command_selection(-1);
                            return Vec::new();
                        }
                        TuiKeyCode::Down => {
                            self.state.move_command_selection(1);
                            return Vec::new();
                        }
                        TuiKeyCode::Tab => {
                            self.state.accept_selected_completion(&self.registry);
                            return Vec::new();
                        }
                        TuiKeyCode::Enter if !self.selected_completion_is_identity() => {
                            self.state.accept_selected_completion(&self.registry);
                            return Vec::new();
                        }
                        _ => {}
                    }
                }
                if key.code == TuiKeyCode::Enter {
                    return self.submit_composer();
                }
                if matches!(key.code, TuiKeyCode::Up | TuiKeyCode::Down)
                    && self
                        .state
                        .navigate_input_history(key.code == TuiKeyCode::Up)
                {
                    self.state.refresh_command_assist(&self.registry);
                    return Vec::new();
                }
                if is_ctrl_r(key.code, key.modifiers)
                    && !self.state.is_streaming
                    && self.state.can_resume_turn
                {
                    self.state.begin_resume();
                    return vec![AppCommand::ResumeTurn];
                }
                input::handle_key(&mut self.state, key);
                self.state.refresh_command_assist(&self.registry);
                let mut commands: Vec<_> = self
                    .state
                    .should_quit
                    .then_some(AppCommand::Quit)
                    .into_iter()
                    .collect();
                if let Some(request) = self.state.begin_session_completion() {
                    commands.push(AppCommand::RequestSessionCompletions(request));
                }
                commands
            }
            TuiEvent::Paste(text) => {
                self.state.composer.insert_str(&text);
                self.state.refresh_command_assist(&self.registry);
                self.state
                    .begin_session_completion()
                    .map(AppCommand::RequestSessionCompletions)
                    .into_iter()
                    .collect()
            }
            TuiEvent::Resize { .. } => Vec::new(),
        }
    }

    fn selected_completion_is_identity(&self) -> bool {
        let Some(candidate) = self
            .state
            .command_assist
            .candidates
            .get(self.state.command_assist.selected)
        else {
            return true;
        };
        self.state
            .composer
            .text()
            .get(candidate.replacement_range.clone())
            .is_some_and(|text| text == candidate.replacement)
    }

    fn submit_composer(&mut self) -> Vec<AppCommand> {
        match self
            .registry
            .classify_with_context(self.state.composer.text(), self.state.command_context())
        {
            InputClassification::Prompt => {
                if self.state.is_streaming {
                    self.state
                        .set_command_diagnostic("wait for the active turn or cancel it first");
                    return Vec::new();
                }
                if self.state.can_resume_turn {
                    self.state.set_command_diagnostic(
                        "a failed turn must be resumed with Ctrl+R before sending new input",
                    );
                    return Vec::new();
                }
                self.state
                    .submit_input()
                    .map(AppCommand::SubmitUserInput)
                    .into_iter()
                    .collect()
            }
            InputClassification::EscapedPrompt(text) => self
                .state
                .submit_text(text)
                .map(AppCommand::SubmitUserInput)
                .into_iter()
                .collect(),
            InputClassification::Command(ParseState::Complete(invocation)) => {
                self.dispatch_invocation(invocation)
            }
            InputClassification::Command(ParseState::Incomplete { message })
            | InputClassification::Command(ParseState::Invalid { message })
            | InputClassification::Command(ParseState::Unavailable { message }) => {
                self.state.set_command_diagnostic(message);
                Vec::new()
            }
        }
    }

    fn dispatch_invocation(&mut self, invocation: CommandInvocation) -> Vec<AppCommand> {
        if invocation.context_revision != self.state.command_context().revision {
            self.state
                .set_command_diagnostic("command context changed; review and submit again");
            return Vec::new();
        }
        let submitted_command = self.state.composer.text().to_owned();
        let commands = match invocation.id {
            CommandId::Help => {
                let command = invocation.arguments.first().map(String::as_str);
                match self.registry.help(command) {
                    Ok(help) => self.state.add_system_message(&help),
                    Err(error) => self.state.set_command_diagnostic(error),
                }
                Vec::new()
            }
            CommandId::Clear => {
                self.state.clear_transcript();
                Vec::new()
            }
            CommandId::Quit => vec![AppCommand::Quit],
            CommandId::RecoveryInspect => vec![AppCommand::InspectRecovery],
            CommandId::ApprovalInspect => vec![AppCommand::InspectApproval],
            CommandId::SessionList => vec![AppCommand::ListSessions],
            CommandId::SessionLoad => vec![AppCommand::LoadSession(SessionId::new(
                &invocation.arguments[0],
            ))],
            CommandId::RecoverySuccess => vec![AppCommand::ReconcileTool {
                tool_call_id: ToolCallId::new(&invocation.arguments[0]),
                decision: ReconciliationDecision::MarkSucceeded {
                    content: invocation.arguments[1].clone(),
                },
            }],
            CommandId::RecoveryFailed => vec![AppCommand::ReconcileTool {
                tool_call_id: ToolCallId::new(&invocation.arguments[0]),
                decision: ReconciliationDecision::MarkFailed {
                    message: invocation.arguments[1].clone(),
                },
            }],
            CommandId::RecoveryRetry => vec![AppCommand::ReconcileTool {
                tool_call_id: ToolCallId::new(&invocation.arguments[0]),
                decision: ReconciliationDecision::RetryAnyway,
            }],
            CommandId::RecoveryAbandon => vec![AppCommand::ReconcileTool {
                tool_call_id: ToolCallId::new(&invocation.arguments[0]),
                decision: ReconciliationDecision::AbandonTurn,
            }],
            CommandId::ApprovalApprove => vec![AppCommand::ResolveApproval {
                tool_call_id: ToolCallId::new(&invocation.arguments[0]),
                approved: true,
                reason: None,
            }],
            CommandId::ApprovalReject => vec![AppCommand::ResolveApproval {
                tool_call_id: ToolCallId::new(&invocation.arguments[0]),
                approved: false,
                reason: invocation.arguments.get(1).cloned(),
            }],
        };
        self.state.composer.clear();
        if !matches!(
            self.registry.history_policy(invocation.id),
            super::command::HistoryPolicy::Omit
        ) {
            self.state.record_command_history(submitted_command);
        }
        self.state.command_assist = Default::default();
        if matches!(
            invocation.id,
            CommandId::RecoverySuccess
                | CommandId::RecoveryFailed
                | CommandId::RecoveryRetry
                | CommandId::RecoveryAbandon
                | CommandId::ApprovalApprove
                | CommandId::ApprovalReject
        ) {
            self.state.begin_resume();
        }
        commands
    }
}

fn is_ctrl_r(code: TuiKeyCode, modifiers: TuiKeyModifiers) -> bool {
    matches!(code, TuiKeyCode::Char('r')) && modifiers.control && !modifiers.alt
}

fn is_ctrl_c(code: TuiKeyCode, modifiers: TuiKeyModifiers) -> bool {
    matches!(code, TuiKeyCode::Char('c')) && modifiers.control && !modifiers.alt
}

#[cfg(test)]
mod tests {
    use crate::tui::TuiKeyEvent;

    use super::*;

    fn key(code: TuiKeyCode) -> UiEvent {
        UiEvent::Terminal(TuiEvent::Key(TuiKeyEvent::new(code, TuiKeyModifiers::NONE)))
    }

    #[test]
    fn enter_produces_a_submit_command_without_trimming_the_prompt() {
        let mut controller = AppController::new(AppState::new());
        controller.state_mut().composer.insert_str(" hello ");

        assert_eq!(
            controller.handle(key(TuiKeyCode::Enter)),
            vec![AppCommand::SubmitUserInput(" hello ".to_owned())]
        );
    }

    #[test]
    fn escaped_slash_is_submitted_to_the_model() {
        let mut controller = AppController::new(AppState::new());
        controller.state_mut().composer.insert_str("//hello");

        assert_eq!(
            controller.handle(key(TuiKeyCode::Enter)),
            vec![AppCommand::SubmitUserInput("/hello".to_owned())]
        );
    }

    #[test]
    fn incomplete_command_keeps_the_draft_and_exposes_a_diagnostic() {
        let mut controller = AppController::new(AppState::new());
        controller
            .state_mut()
            .composer
            .insert_str("/approval approve");

        assert!(controller.handle(key(TuiKeyCode::Enter)).is_empty());
        assert_eq!(controller.state().composer.text(), "/approval approve");
        assert!(controller.state().command_assist.diagnostic.is_some());
    }

    #[test]
    fn tab_accepts_a_completion_without_dispatching_it() {
        let mut controller = AppController::new(AppState::new());
        controller.state_mut().composer.insert_str("/rec");
        controller
            .state_mut()
            .refresh_command_assist(&CommandRegistry::builtins());

        assert!(controller.handle(key(TuiKeyCode::Tab)).is_empty());
        assert_eq!(controller.state().composer.text(), "/recovery");
    }

    #[test]
    fn help_is_a_local_command() {
        let mut controller = AppController::new(AppState::new());
        controller.state_mut().composer.insert_str("/help approval");

        assert!(controller.handle(key(TuiKeyCode::Enter)).is_empty());
        assert!(
            controller
                .state()
                .history
                .back()
                .unwrap()
                .text
                .contains("/approval")
        );
    }

    #[test]
    fn session_load_is_an_application_command() {
        let mut controller = AppController::new(AppState::new());
        controller
            .state_mut()
            .composer
            .insert_str("/session load session-1");

        assert_eq!(
            controller.handle(key(TuiKeyCode::Enter)),
            vec![AppCommand::LoadSession(SessionId::new("session-1"))]
        );
    }

    #[test]
    fn command_history_is_separate_from_prompt_history() {
        let mut controller = AppController::new(AppState::new());
        controller.state_mut().composer.insert_str("hello");
        controller.handle(key(TuiKeyCode::Enter));
        controller.state_mut().composer.insert_str("/help");
        controller.handle(key(TuiKeyCode::Enter));
        controller.state_mut().composer.insert_str("/");

        controller.handle(key(TuiKeyCode::Up));

        assert_eq!(controller.state().composer.text(), "/help");
    }

    #[test]
    fn ctrl_r_resumes_only_a_recoverable_turn() {
        let mut controller = AppController::new(AppState::new());
        controller.state_mut().can_resume_turn = true;

        let commands = controller.handle(UiEvent::Terminal(TuiEvent::Key(TuiKeyEvent::new(
            TuiKeyCode::Char('r'),
            TuiKeyModifiers::new(true, false, false),
        ))));

        assert_eq!(commands, vec![AppCommand::ResumeTurn]);
        assert!(controller.state().is_streaming);
    }

    #[test]
    fn ctrl_c_cancels_an_active_turn_instead_of_quitting() {
        let mut state = AppState::new();
        state.is_streaming = true;
        let mut controller = AppController::new(state);

        let commands = controller.handle(UiEvent::Terminal(TuiEvent::Key(TuiKeyEvent::new(
            TuiKeyCode::Char('c'),
            TuiKeyModifiers::new(true, false, false),
        ))));

        assert_eq!(commands, vec![AppCommand::CancelTurn]);
        assert!(!controller.state().should_quit);
    }
}
