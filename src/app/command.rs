use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandId {
    Help,
    Clear,
    Quit,
    RecoveryInspect,
    RecoverySuccess,
    RecoveryFailed,
    RecoveryRetry,
    RecoveryAbandon,
    SessionList,
    SessionLoad,
    SessionRename,
    SessionMove,
    ChangeDirectory,
    PrintWorkingDirectory,
    SessionTreeList,
    SessionTreeCheckout,
    SessionTreeEdit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandTarget {
    Local,
    Application,
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInvocation {
    pub id: CommandId,
    pub arguments: Vec<String>,
    pub context_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputClassification {
    Prompt,
    EscapedPrompt(String),
    Command(ParseState),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseState {
    Incomplete { message: String },
    Complete(CommandInvocation),
    Invalid { message: String },
    Unavailable { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub stable_key: String,
    pub label: String,
    pub detail: String,
    pub replacement: String,
    pub replacement_range: Range<usize>,
    pub kind: CompletionKind,
    pub disabled_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    Subcommand,
    Resource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionRequest {
    pub request_id: u64,
    pub draft_revision: u64,
    pub cursor: usize,
    pub context_revision: u64,
    pub query: String,
    pub replacement_range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResponse {
    pub request_id: u64,
    pub draft_revision: u64,
    pub cursor: usize,
    pub context_revision: u64,
    pub candidates: Vec<CompletionItem>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandAssist {
    pub candidates: Vec<CompletionItem>,
    pub usage: Option<String>,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionSource {
    CommandNames,
    RecoveryToolCalls,
    SessionIds,
    SessionEntryIds,
}

#[derive(Debug, Clone, Copy)]
enum ArgumentKind {
    Completion(CompletionSource),
    Value,
    Rest,
}

#[derive(Debug, Clone, Copy)]
struct ArgumentSpec {
    name: &'static str,
    required: bool,
    kind: ArgumentKind,
}

#[derive(Debug, Clone, Copy)]
struct CommandForm {
    id: CommandId,
    path: &'static [&'static str],
    arguments: &'static [ArgumentSpec],
    summary: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct CommandDefinition {
    name: &'static str,
    aliases: &'static [&'static str],
    summary: &'static str,
    target: CommandTarget,
    forms: &'static [CommandForm],
    availability: Availability,
    history: HistoryPolicy,
    source: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryPolicy {
    Store,
    Redact,
    Omit,
}

#[derive(Debug, Clone, Copy)]
enum Availability {
    Always,
    Idle,
    RecoveryPending,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CommandContext {
    pub revision: u64,
    pub is_streaming: bool,
    pub recovery_pending: bool,
}

const RECOVERY_ID: ArgumentKind = ArgumentKind::Completion(CompletionSource::RecoveryToolCalls);
const COMMAND_NAME: ArgumentKind = ArgumentKind::Completion(CompletionSource::CommandNames);
const SESSION_ID: ArgumentKind = ArgumentKind::Completion(CompletionSource::SessionIds);
const SESSION_ENTRY_ID: ArgumentKind = ArgumentKind::Completion(CompletionSource::SessionEntryIds);

const HELP_ARGS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "command",
    required: false,
    kind: COMMAND_NAME,
}];
const RECOVERY_CALL_ID: &[ArgumentSpec] = &[ArgumentSpec {
    name: "tool-call-id",
    required: true,
    kind: RECOVERY_ID,
}];
const RECOVERY_RESULT: &[ArgumentSpec] = &[
    ArgumentSpec {
        name: "tool-call-id",
        required: true,
        kind: RECOVERY_ID,
    },
    ArgumentSpec {
        name: "result",
        required: true,
        kind: ArgumentKind::Rest,
    },
];
const RECOVERY_FAILURE: &[ArgumentSpec] = &[
    ArgumentSpec {
        name: "tool-call-id",
        required: true,
        kind: RECOVERY_ID,
    },
    ArgumentSpec {
        name: "message",
        required: true,
        kind: ArgumentKind::Rest,
    },
];
const SESSION_LOAD: &[ArgumentSpec] = &[ArgumentSpec {
    name: "session-id",
    required: true,
    kind: SESSION_ID,
}];
const SESSION_RENAME: &[ArgumentSpec] = &[ArgumentSpec {
    name: "entry-name",
    required: true,
    kind: ArgumentKind::Rest,
}];
const SESSION_MOVE: &[ArgumentSpec] = &[
    ArgumentSpec {
        name: "project-root",
        required: true,
        kind: ArgumentKind::Value,
    },
    ArgumentSpec {
        name: "entry-name",
        required: false,
        kind: ArgumentKind::Value,
    },
];
const CHANGE_DIRECTORY_ARGS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "path",
    required: true,
    kind: ArgumentKind::Rest,
}];
const TREE_ENTRY_ARGS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "entry-id|root",
    required: true,
    kind: SESSION_ENTRY_ID,
}];

const HELP_FORMS: &[CommandForm] = &[CommandForm {
    id: CommandId::Help,
    path: &[],
    arguments: HELP_ARGS,
    summary: "show available commands or detailed command help",
}];
const CLEAR_FORMS: &[CommandForm] = &[CommandForm {
    id: CommandId::Clear,
    path: &[],
    arguments: &[],
    summary: "clear the local transcript",
}];
const QUIT_FORMS: &[CommandForm] = &[CommandForm {
    id: CommandId::Quit,
    path: &[],
    arguments: &[],
    summary: "exit Rua",
}];
const CHANGE_DIRECTORY_FORMS: &[CommandForm] = &[CommandForm {
    id: CommandId::ChangeDirectory,
    path: &[],
    arguments: CHANGE_DIRECTORY_ARGS,
    summary: "change the session working directory",
}];
const PRINT_WORKING_DIRECTORY_FORMS: &[CommandForm] = &[CommandForm {
    id: CommandId::PrintWorkingDirectory,
    path: &[],
    arguments: &[],
    summary: "show the session working directory",
}];
const SESSION_TREE_FORMS: &[CommandForm] = &[
    CommandForm {
        id: CommandId::SessionTreeCheckout,
        path: &["checkout"],
        arguments: TREE_ENTRY_ARGS,
        summary: "move the durable head to an entry or virtual root",
    },
    CommandForm {
        id: CommandId::SessionTreeEdit,
        path: &["edit"],
        arguments: TREE_ENTRY_ARGS,
        summary: "edit a user prompt from its parent as a new branch",
    },
    CommandForm {
        id: CommandId::SessionTreeList,
        path: &[],
        arguments: &[],
        summary: "show the current session entry tree",
    },
];
const RECOVERY_FORMS: &[CommandForm] = &[
    CommandForm {
        id: CommandId::RecoveryInspect,
        path: &["inspect"],
        arguments: &[],
        summary: "show tool calls requiring reconciliation",
    },
    CommandForm {
        id: CommandId::RecoverySuccess,
        path: &["success"],
        arguments: RECOVERY_RESULT,
        summary: "record a verified successful result",
    },
    CommandForm {
        id: CommandId::RecoveryFailed,
        path: &["failed"],
        arguments: RECOVERY_FAILURE,
        summary: "record a verified failed result",
    },
    CommandForm {
        id: CommandId::RecoveryRetry,
        path: &["retry"],
        arguments: RECOVERY_CALL_ID,
        summary: "retry a call whose outcome is unknown",
    },
    CommandForm {
        id: CommandId::RecoveryAbandon,
        path: &["abandon"],
        arguments: RECOVERY_CALL_ID,
        summary: "abandon the interrupted turn",
    },
];
const SESSION_FORMS: &[CommandForm] = &[
    CommandForm {
        id: CommandId::SessionList,
        path: &["list"],
        arguments: &[],
        summary: "list local sessions",
    },
    CommandForm {
        id: CommandId::SessionLoad,
        path: &["load"],
        arguments: SESSION_LOAD,
        summary: "load a local session",
    },
    CommandForm {
        id: CommandId::SessionRename,
        path: &["rename"],
        arguments: SESSION_RENAME,
        summary: "rename the current session entry without changing its stable id",
    },
    CommandForm {
        id: CommandId::SessionMove,
        path: &["move"],
        arguments: SESSION_MOVE,
        summary: "move the current session entry to another project store",
    },
];

const DEFINITIONS: &[CommandDefinition] = &[
    CommandDefinition {
        name: "help",
        aliases: &[],
        summary: "show command help",
        target: CommandTarget::Local,
        forms: HELP_FORMS,
        availability: Availability::Always,
        history: HistoryPolicy::Store,
        source: "builtin",
    },
    CommandDefinition {
        name: "session",
        aliases: &[],
        summary: "list or load local sessions",
        target: CommandTarget::Application,
        forms: SESSION_FORMS,
        availability: Availability::Idle,
        history: HistoryPolicy::Store,
        source: "builtin",
    },
    CommandDefinition {
        name: "cd",
        aliases: &[],
        summary: "change the session working directory",
        target: CommandTarget::Runtime,
        forms: CHANGE_DIRECTORY_FORMS,
        availability: Availability::Idle,
        history: HistoryPolicy::Store,
        source: "builtin",
    },
    CommandDefinition {
        name: "pwd",
        aliases: &[],
        summary: "show the session working directory",
        target: CommandTarget::Runtime,
        forms: PRINT_WORKING_DIRECTORY_FORMS,
        availability: Availability::Idle,
        history: HistoryPolicy::Store,
        source: "builtin",
    },
    CommandDefinition {
        name: "tree",
        aliases: &[],
        summary: "browse or move within the session entry tree",
        target: CommandTarget::Runtime,
        forms: SESSION_TREE_FORMS,
        availability: Availability::Idle,
        history: HistoryPolicy::Store,
        source: "builtin",
    },
    CommandDefinition {
        name: "clear",
        aliases: &[],
        summary: "clear the local transcript",
        target: CommandTarget::Local,
        forms: CLEAR_FORMS,
        availability: Availability::Always,
        history: HistoryPolicy::Store,
        source: "builtin",
    },
    CommandDefinition {
        name: "quit",
        aliases: &["exit"],
        summary: "exit Rua",
        target: CommandTarget::Local,
        forms: QUIT_FORMS,
        availability: Availability::Always,
        history: HistoryPolicy::Omit,
        source: "builtin",
    },
    CommandDefinition {
        name: "recovery",
        aliases: &[],
        summary: "inspect or reconcile interrupted tool calls",
        target: CommandTarget::Runtime,
        forms: RECOVERY_FORMS,
        availability: Availability::RecoveryPending,
        history: HistoryPolicy::Store,
        source: "builtin",
    },
];

#[derive(Debug, Clone, Default)]
pub struct CompletionContext<'a> {
    pub recovery_tool_calls: &'a [String],
    pub session_ids: &'a [String],
    pub session_entry_ids: &'a [String],
}

#[derive(Debug, Clone, Default)]
pub struct CommandRegistry;

impl CommandRegistry {
    pub fn builtins() -> Self {
        debug_assert!(registry_is_valid());
        Self
    }

    pub fn classify(&self, input: &str) -> InputClassification {
        self.classify_with_context(
            input,
            CommandContext {
                recovery_pending: true,
                ..CommandContext::default()
            },
        )
    }

    pub fn classify_with_context(
        &self,
        input: &str,
        context: CommandContext,
    ) -> InputClassification {
        if let Some(prompt) = input.strip_prefix("//") {
            return InputClassification::EscapedPrompt(format!("/{prompt}"));
        }
        if !input.starts_with('/') {
            return InputClassification::Prompt;
        }
        InputClassification::Command(self.parse_command(input, context))
    }

    pub fn assist(
        &self,
        input: &str,
        cursor: usize,
        context: CompletionContext<'_>,
    ) -> CommandAssist {
        if !input.starts_with('/') || input.starts_with("//") || cursor > input.len() {
            return CommandAssist::default();
        }

        let name_end = input[1..]
            .find(char::is_whitespace)
            .map_or(input.len(), |index| index + 1);
        if cursor <= name_end {
            let query = &input[1..cursor];
            return CommandAssist {
                candidates: command_candidates(query, 0..name_end),
                usage: Some("/command [arguments]".to_owned()),
                diagnostic: None,
            };
        }

        let tokens = match lex(&input[1..], 1) {
            Ok(tokens) => tokens,
            Err(error) => {
                return CommandAssist {
                    candidates: Vec::new(),
                    usage: None,
                    diagnostic: Some(error),
                };
            }
        };
        let Some(name) = tokens.first() else {
            return CommandAssist::default();
        };
        let Some(definition) = find_definition(&name.value) else {
            return CommandAssist {
                candidates: Vec::new(),
                usage: None,
                diagnostic: Some(format!("unknown command /{}", name.value)),
            };
        };

        let replacement_range = word_range(input, cursor);
        let query = input[replacement_range.clone()].trim_matches(['\'', '"']);
        let path_len = definition
            .forms
            .iter()
            .map(|form| form.path.len())
            .max()
            .unwrap_or(0);
        if path_len > 0
            && tokens.len() <= 2
            && cursor <= tokens.get(1).map_or(input.len(), |t| t.range.end)
        {
            let candidates = definition
                .forms
                .iter()
                .filter_map(|form| {
                    form.path
                        .first()
                        .copied()
                        .map(|value| (value, form.summary))
                })
                .filter(|(value, _)| fuzzy_match(value, query))
                .map(|(value, summary)| CompletionItem {
                    stable_key: format!("subcommand:{}:{value}", definition.name),
                    label: value.to_owned(),
                    detail: summary.to_owned(),
                    replacement: value.to_owned(),
                    replacement_range: replacement_range.clone(),
                    kind: CompletionKind::Subcommand,
                    disabled_reason: None,
                })
                .collect();
            return CommandAssist {
                candidates,
                usage: Some(command_usage(definition)),
                diagnostic: None,
            };
        }

        let form = definition.forms.iter().find(|form| {
            form.path.iter().enumerate().all(|(index, expected)| {
                tokens
                    .get(index + 1)
                    .is_some_and(|token| token.value == *expected)
            })
        });
        let Some(form) = form else {
            return CommandAssist {
                candidates: Vec::new(),
                usage: Some(command_usage(definition)),
                diagnostic: Some(format!("usage: {}", command_usage(definition))),
            };
        };
        let typed_arguments = tokens.len().saturating_sub(1 + form.path.len());
        let argument_index = tokens
            .last()
            .filter(|token| token.range.start <= cursor && cursor <= token.range.end)
            .map_or(typed_arguments, |_| typed_arguments.saturating_sub(1));
        let argument = form.arguments.get(argument_index).or_else(|| {
            form.arguments
                .last()
                .filter(|argument| matches!(argument.kind, ArgumentKind::Rest))
        });
        let candidates = argument.map_or_else(Vec::new, |argument| {
            let values: Vec<(&str, &str)> = match argument.kind {
                ArgumentKind::Completion(CompletionSource::CommandNames) => DEFINITIONS
                    .iter()
                    .map(|definition| (definition.name, definition.summary))
                    .collect(),
                ArgumentKind::Completion(CompletionSource::RecoveryToolCalls) => context
                    .recovery_tool_calls
                    .iter()
                    .map(|value| (value.as_str(), "recovery required"))
                    .collect(),
                ArgumentKind::Completion(CompletionSource::SessionIds) => context
                    .session_ids
                    .iter()
                    .map(|value| (value.as_str(), "local session"))
                    .collect(),
                ArgumentKind::Completion(CompletionSource::SessionEntryIds) => context
                    .session_entry_ids
                    .iter()
                    .map(|value| (value.as_str(), "session tree entry"))
                    .collect(),
                _ => Vec::new(),
            };
            values
                .into_iter()
                .filter(|(value, _)| fuzzy_match(value, query))
                .map(|(value, detail)| CompletionItem {
                    stable_key: format!("resource:{value}"),
                    label: value.to_owned(),
                    detail: detail.to_owned(),
                    replacement: value.to_owned(),
                    replacement_range: replacement_range.clone(),
                    kind: CompletionKind::Resource,
                    disabled_reason: None,
                })
                .collect()
        });
        CommandAssist {
            candidates,
            usage: Some(form_usage(definition.name, form)),
            diagnostic: None,
        }
    }

    pub fn help(&self, command: Option<&str>) -> Result<String, String> {
        match command {
            None => Ok(DEFINITIONS
                .iter()
                .map(|definition| format!("/{:<10} {}", definition.name, definition.summary))
                .collect::<Vec<_>>()
                .join("\n")),
            Some(name) => {
                let name = name.trim_start_matches('/');
                let definition =
                    find_definition(name).ok_or_else(|| format!("unknown command /{name}"))?;
                let aliases = if definition.aliases.is_empty() {
                    String::new()
                } else {
                    format!("\naliases: {}", definition.aliases.join(", "))
                };
                Ok(format!(
                    "/{} — {}\ntarget: {:?}\nsource: {}{}\n{}",
                    definition.name,
                    definition.summary,
                    definition.target,
                    definition.source,
                    aliases,
                    command_usage(definition)
                ))
            }
        }
    }

    pub fn history_policy(&self, id: CommandId) -> HistoryPolicy {
        DEFINITIONS
            .iter()
            .find(|definition| definition.forms.iter().any(|form| form.id == id))
            .map_or(HistoryPolicy::Omit, |definition| definition.history)
    }

    fn parse_command(&self, input: &str, context: CommandContext) -> ParseState {
        let tokens = match lex(&input[1..], 1) {
            Ok(tokens) => tokens,
            Err(message) => return ParseState::Incomplete { message },
        };
        let Some(name) = tokens.first() else {
            return ParseState::Incomplete {
                message: "type a command name after /".to_owned(),
            };
        };
        let Some(definition) = find_definition(&name.value) else {
            let suggestion = closest_command(&name.value)
                .map(|value| format!("; did you mean /{value}?"))
                .unwrap_or_default();
            return ParseState::Invalid {
                message: format!("unknown command /{}{suggestion}", name.value),
            };
        };
        if let Some(message) = unavailable_reason(definition.availability, context) {
            return ParseState::Unavailable { message };
        }

        let form = definition.forms.iter().find(|form| {
            form.path.iter().enumerate().all(|(index, expected)| {
                tokens
                    .get(index + 1)
                    .is_some_and(|token| token.value == *expected)
            })
        });
        let Some(form) = form else {
            return ParseState::Incomplete {
                message: format!("usage: {}", command_usage(definition)),
            };
        };
        let argument_tokens = &tokens[1 + form.path.len()..];
        let mut arguments = Vec::new();
        let mut token_index = 0;
        for argument in form.arguments {
            match argument.kind {
                ArgumentKind::Completion(_) => {
                    if let Some(token) = argument_tokens.get(token_index) {
                        arguments.push(token.value.clone());
                        token_index += 1;
                    } else if argument.required {
                        return ParseState::Incomplete {
                            message: format!(
                                "missing <{}>; usage: {}",
                                argument.name,
                                form_usage(definition.name, form)
                            ),
                        };
                    }
                }
                ArgumentKind::Value => {
                    if let Some(token) = argument_tokens.get(token_index) {
                        arguments.push(token.value.clone());
                        token_index += 1;
                    } else if argument.required {
                        return ParseState::Incomplete {
                            message: format!(
                                "missing <{}>; usage: {}",
                                argument.name,
                                form_usage(definition.name, form)
                            ),
                        };
                    }
                }
                ArgumentKind::Rest => {
                    if token_index < argument_tokens.len() {
                        arguments.push(
                            argument_tokens[token_index..]
                                .iter()
                                .map(|token| token.value.as_str())
                                .collect::<Vec<_>>()
                                .join(" "),
                        );
                        token_index = argument_tokens.len();
                    } else if argument.required {
                        return ParseState::Incomplete {
                            message: format!(
                                "missing <{}>; usage: {}",
                                argument.name,
                                form_usage(definition.name, form)
                            ),
                        };
                    }
                }
            }
        }
        if token_index < argument_tokens.len() {
            return ParseState::Invalid {
                message: format!(
                    "too many arguments; usage: {}",
                    form_usage(definition.name, form)
                ),
            };
        }
        ParseState::Complete(CommandInvocation {
            id: form.id,
            arguments,
            context_revision: context.revision,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    value: String,
    range: Range<usize>,
}

fn lex(input: &str, offset: usize) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < input.len() {
        while input[index..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
        {
            index += input[index..].chars().next().unwrap().len_utf8();
        }
        if index == input.len() {
            break;
        }
        let start = index;
        let mut value = String::new();
        let mut quote = None;
        let mut escaped = false;
        while index < input.len() {
            let character = input[index..].chars().next().unwrap();
            if escaped {
                value.push(character);
                escaped = false;
                index += character.len_utf8();
                continue;
            }
            if character == '\\' {
                escaped = true;
                index += 1;
                continue;
            }
            if let Some(expected) = quote {
                index += character.len_utf8();
                if character == expected {
                    quote = None;
                } else {
                    value.push(character);
                }
                continue;
            }
            if matches!(character, '\'' | '"') {
                quote = Some(character);
                index += 1;
                continue;
            }
            if character.is_whitespace() {
                break;
            }
            value.push(character);
            index += character.len_utf8();
        }
        if escaped {
            return Err("incomplete escape at end of command".to_owned());
        }
        if let Some(expected) = quote {
            return Err(format!("missing closing {expected}"));
        }
        tokens.push(Token {
            value,
            range: offset + start..offset + index,
        });
    }
    Ok(tokens)
}

fn find_definition(name: &str) -> Option<&'static CommandDefinition> {
    DEFINITIONS
        .iter()
        .find(|definition| definition.name == name || definition.aliases.contains(&name))
}

fn unavailable_reason(availability: Availability, context: CommandContext) -> Option<String> {
    match availability {
        Availability::Always => None,
        Availability::Idle if context.is_streaming => {
            Some("command is unavailable while a turn is active".to_owned())
        }
        Availability::RecoveryPending if !context.recovery_pending => {
            Some("no tool recovery is pending".to_owned())
        }
        _ => None,
    }
}

fn command_usage(definition: &CommandDefinition) -> String {
    definition
        .forms
        .iter()
        .map(|form| form_usage(definition.name, form))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn form_usage(name: &str, form: &CommandForm) -> String {
    let mut parts = vec![format!("/{name}")];
    parts.extend(form.path.iter().map(|part| (*part).to_owned()));
    parts.extend(form.arguments.iter().map(|argument| {
        if argument.required {
            format!("<{}>", argument.name)
        } else {
            format!("[{}]", argument.name)
        }
    }));
    parts.join(" ")
}

fn command_candidates(query: &str, range: Range<usize>) -> Vec<CompletionItem> {
    let mut candidates = DEFINITIONS
        .iter()
        .filter(|definition| fuzzy_match(definition.name, query))
        .map(|definition| CompletionItem {
            stable_key: format!("command:{}", definition.name),
            label: format!("/{}", definition.name),
            detail: definition.summary.to_owned(),
            replacement: format!("/{}", definition.name),
            replacement_range: range.clone(),
            kind: CompletionKind::Command,
            disabled_reason: None,
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| {
        let name = candidate.label.trim_start_matches('/');
        (!name.starts_with(query), name.to_owned())
    });
    candidates
}

fn fuzzy_match(candidate: &str, query: &str) -> bool {
    if query.is_empty() || candidate.starts_with(query) {
        return true;
    }
    let mut query = query.chars();
    let mut next = query.next();
    for character in candidate.chars() {
        if next == Some(character) {
            next = query.next();
        }
    }
    next.is_none()
}

fn closest_command(query: &str) -> Option<&'static str> {
    DEFINITIONS
        .iter()
        .filter(|definition| fuzzy_match(definition.name, query))
        .map(|definition| definition.name)
        .next()
}

fn word_range(input: &str, cursor: usize) -> Range<usize> {
    let start = input[..cursor]
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map_or(0, |(index, character)| index + character.len_utf8());
    let end = input[cursor..]
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
        .map_or(input.len(), |(index, _)| cursor + index);
    start..end
}

fn registry_is_valid() -> bool {
    let mut names = Vec::new();
    for definition in DEFINITIONS {
        for name in std::iter::once(definition.name).chain(definition.aliases.iter().copied()) {
            if names.contains(&name) {
                return false;
            }
            names.push(name);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_literal_slash_as_an_escaped_prompt() {
        assert_eq!(
            CommandRegistry::builtins().classify("//review this"),
            InputClassification::EscapedPrompt("/review this".to_owned())
        );
    }

    #[test]
    fn parses_quoted_and_rest_arguments() {
        assert_eq!(
            CommandRegistry::builtins()
                .classify("/recovery success 'call one' verified result text"),
            InputClassification::Command(ParseState::Complete(CommandInvocation {
                id: CommandId::RecoverySuccess,
                arguments: vec!["call one".to_owned(), "verified result text".to_owned()],
                context_revision: 0,
            }))
        );
    }

    #[test]
    fn unknown_commands_are_not_prompts() {
        assert!(matches!(
            CommandRegistry::builtins().classify("/halp"),
            InputClassification::Command(ParseState::Invalid { .. })
        ));
    }

    #[test]
    fn completion_replaces_only_the_command_token() {
        let assist =
            CommandRegistry::builtins().assist("/rec inspect", 4, CompletionContext::default());
        let recovery = assist
            .candidates
            .iter()
            .find(|candidate| candidate.label == "/recovery")
            .unwrap();
        assert_eq!(recovery.replacement_range, 0..4);
        assert_eq!(recovery.replacement, "/recovery");
    }

    #[test]
    fn registry_has_no_name_or_alias_collisions() {
        assert!(registry_is_valid());
    }

    #[test]
    fn parses_working_directory_commands() {
        assert_eq!(
            CommandRegistry::builtins().classify("/cd crates/runtime"),
            InputClassification::Command(ParseState::Complete(CommandInvocation {
                id: CommandId::ChangeDirectory,
                arguments: vec!["crates/runtime".to_owned()],
                context_revision: 0,
            }))
        );
        assert_eq!(
            CommandRegistry::builtins().classify("/pwd"),
            InputClassification::Command(ParseState::Complete(CommandInvocation {
                id: CommandId::PrintWorkingDirectory,
                arguments: Vec::new(),
                context_revision: 0,
            }))
        );
        assert!(matches!(
            CommandRegistry::builtins().classify("/cd"),
            InputClassification::Command(ParseState::Incomplete { .. })
        ));
    }

    #[test]
    fn parses_tree_navigation_and_session_rename_commands() {
        assert_eq!(
            CommandRegistry::builtins().classify("/tree checkout entry-2"),
            InputClassification::Command(ParseState::Complete(CommandInvocation {
                id: CommandId::SessionTreeCheckout,
                arguments: vec!["entry-2".to_owned()],
                context_revision: 0,
            }))
        );
        assert_eq!(
            CommandRegistry::builtins().classify("/tree edit entry-1"),
            InputClassification::Command(ParseState::Complete(CommandInvocation {
                id: CommandId::SessionTreeEdit,
                arguments: vec!["entry-1".to_owned()],
                context_revision: 0,
            }))
        );
        assert_eq!(
            CommandRegistry::builtins().classify("/session rename investigation"),
            InputClassification::Command(ParseState::Complete(CommandInvocation {
                id: CommandId::SessionRename,
                arguments: vec!["investigation".to_owned()],
                context_revision: 0,
            }))
        );
        assert_eq!(
            CommandRegistry::builtins().classify("/session move ../other-project moved-name"),
            InputClassification::Command(ParseState::Complete(CommandInvocation {
                id: CommandId::SessionMove,
                arguments: vec!["../other-project".to_owned(), "moved-name".to_owned()],
                context_revision: 0,
            }))
        );
    }
}
