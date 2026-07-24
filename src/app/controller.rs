use crate::agent::{ReconciliationDecision, ToolCallId};
use crate::tui::{TuiEvent, TuiKeyCode, TuiKeyModifiers};

use super::{AppState, AppStatus, UiEvent, input};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppCommand {
    SubmitUserInput(String),
    ResumeTurn,
    InspectRecovery,
    InspectApproval,
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
}

impl AppController {
    pub fn new(state: AppState) -> Self {
        Self { state }
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
                if key.code == TuiKeyCode::Enter && !self.state.is_streaming {
                    let draft = self.state.composer.text().trim().to_owned();
                    if draft.starts_with("/recovery") || draft.starts_with("/approval") {
                        self.state.composer.clear();
                        return match parse_special_command(&draft) {
                            Ok(command) => {
                                if matches!(
                                    command,
                                    AppCommand::ReconcileTool { .. }
                                        | AppCommand::ResolveApproval { .. }
                                ) {
                                    self.state.begin_resume();
                                }
                                vec![command]
                            }
                            Err(error) => {
                                self.state.add_error(&error);
                                Vec::new()
                            }
                        };
                    }
                    if self.state.can_resume_turn {
                        return Vec::new();
                    }
                    if let Some(text) = self.state.submit_input() {
                        return vec![AppCommand::SubmitUserInput(text)];
                    }
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
                self.state
                    .should_quit
                    .then_some(AppCommand::Quit)
                    .into_iter()
                    .collect()
            }
            TuiEvent::Paste(text) => {
                self.state.composer.insert_str(&text);
                Vec::new()
            }
            TuiEvent::Resize { .. } => Vec::new(),
        }
    }
}

fn is_ctrl_r(code: TuiKeyCode, modifiers: TuiKeyModifiers) -> bool {
    matches!(code, TuiKeyCode::Char('r')) && modifiers.control && !modifiers.alt
}

fn is_ctrl_c(code: TuiKeyCode, modifiers: TuiKeyModifiers) -> bool {
    matches!(code, TuiKeyCode::Char('c')) && modifiers.control && !modifiers.alt
}

fn parse_recovery_command(input: &str) -> Result<AppCommand, String> {
    let mut parts = input.splitn(4, ' ');
    if parts.next() != Some("/recovery") {
        return Err("expected /recovery command".to_owned());
    }
    match parts.next() {
        Some("inspect") => Ok(AppCommand::InspectRecovery),
        Some("success") => {
            let call_id = required_part(parts.next(), "tool call id")?;
            let content = required_part(parts.next(), "result text")?;
            Ok(AppCommand::ReconcileTool {
                tool_call_id: ToolCallId::new(call_id),
                decision: ReconciliationDecision::MarkSucceeded {
                    content: content.to_owned(),
                },
            })
        }
        Some("failed") => {
            let call_id = required_part(parts.next(), "tool call id")?;
            let message = required_part(parts.next(), "failure text")?;
            Ok(AppCommand::ReconcileTool {
                tool_call_id: ToolCallId::new(call_id),
                decision: ReconciliationDecision::MarkFailed {
                    message: message.to_owned(),
                },
            })
        }
        Some("retry") => Ok(AppCommand::ReconcileTool {
            tool_call_id: ToolCallId::new(required_part(parts.next(), "tool call id")?),
            decision: ReconciliationDecision::RetryAnyway,
        }),
        Some("abandon") => Ok(AppCommand::ReconcileTool {
            tool_call_id: ToolCallId::new(required_part(parts.next(), "tool call id")?),
            decision: ReconciliationDecision::AbandonTurn,
        }),
        _ => Err(
            "usage: /recovery inspect | success <call-id> <result> | failed <call-id> <message> | retry <call-id> | abandon <call-id>"
                .to_owned(),
        ),
    }
}

fn parse_special_command(input: &str) -> Result<AppCommand, String> {
    if input.starts_with("/recovery") {
        parse_recovery_command(input)
    } else {
        parse_approval_command(input)
    }
}

fn parse_approval_command(input: &str) -> Result<AppCommand, String> {
    let mut parts = input.splitn(4, ' ');
    if parts.next() != Some("/approval") {
        return Err("expected /approval command".to_owned());
    }
    match parts.next() {
        Some("inspect") => Ok(AppCommand::InspectApproval),
        Some("approve") => Ok(AppCommand::ResolveApproval {
            tool_call_id: ToolCallId::new(required_part(parts.next(), "tool call id")?),
            approved: true,
            reason: None,
        }),
        Some("reject") => Ok(AppCommand::ResolveApproval {
            tool_call_id: ToolCallId::new(required_part(parts.next(), "tool call id")?),
            approved: false,
            reason: parts.next().map(str::to_owned),
        }),
        _ => Err(
            "usage: /approval inspect | approve <call-id> | reject <call-id> [reason]".to_owned(),
        ),
    }
}

fn required_part<'a>(value: Option<&'a str>, name: &str) -> Result<&'a str, String> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing {name}"))
}

#[cfg(test)]
mod tests {
    use crate::tui::TuiKeyEvent;

    use super::*;

    #[test]
    fn enter_produces_a_submit_command() {
        let mut controller = AppController::new(AppState::new());
        controller.state_mut().composer.insert_str("hello");

        let commands = controller.handle(UiEvent::Terminal(TuiEvent::Key(TuiKeyEvent::new(
            TuiKeyCode::Enter,
            TuiKeyModifiers::NONE,
        ))));

        assert_eq!(
            commands,
            vec![AppCommand::SubmitUserInput("hello".to_owned())]
        );
    }

    #[test]
    fn parses_recovery_resolution_with_spaces() {
        let command = parse_recovery_command("/recovery success call-1 verified result text")
            .expect("valid command");
        assert_eq!(
            command,
            AppCommand::ReconcileTool {
                tool_call_id: ToolCallId::new("call-1"),
                decision: ReconciliationDecision::MarkSucceeded {
                    content: "verified result text".to_owned(),
                },
            }
        );
    }

    #[test]
    fn parses_approval_rejection_with_reason() {
        assert_eq!(
            parse_approval_command("/approval reject call-1 command is unsafe").unwrap(),
            AppCommand::ResolveApproval {
                tool_call_id: ToolCallId::new("call-1"),
                approved: false,
                reason: Some("command is unsafe".to_owned()),
            }
        );
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
