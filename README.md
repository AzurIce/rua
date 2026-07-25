# rua

A terminal-based AI coding agent built in Rust.

## Current Status

rua is a **streaming coding agent** with a TUI built on ratatui. A provider-neutral runtime owns the canonical conversation, connects to DeepSeek through an adapter, executes workspace coding tools and shell commands, and persists execution boundaries in a checksummed session journal.

## Architecture

```
main.rs              Application assembly + event routing loop
├── agent/
│   ├── runtime.rs   Provider-neutral model → tool → model loop
│   ├── journal.rs   Durable execution state and replay
│   ├── session_store.rs  Checksummed WAL, snapshots, locking, repair/export
│   ├── provider.rs  Provider contract + response accumulator
│   ├── deepseek.rs  DeepSeek adapter + SSE parsing
│   ├── tools.rs     Schema validation and Bash process-group execution
│   ├── coding_tools.rs  Workspace-scoped read/write/edit/glob/grep
│   └── conversation.rs  Canonical committed conversation
├── app/             UI projection, input handling, and rendering
├── tui/             Terminal lifecycle, normalized events, and composer
└── config.rs        TOML config + value resolution
```

## Roadmap

### Done

- [x] **TUI with ratatui** — Alternate-screen terminal UI with clean theme
- [x] **Streaming chat** — Real-time SSE streaming from DeepSeek API
- [x] **Multi-turn history** — Conversation context preserved across turns
- [x] **Event-driven architecture** — Async tokio with mpsc event channel
- [x] **Status indicators** — Spinner animation, state labels (idle/thinking/receiving)
- [x] **Input handling** — Cursor movement, backspace, delete, home/end
- [x] **Configuration** — TOML config at `~/.config/rua/config.toml`
- [x] **Secure API key resolution** — Shell command (`!cmd`), env var, or literal
- [x] **Codebase refactor** — Separated app/state/render/input, domain model extraction, Session layer
- [x] **Tool system** — Bash tool with safe execution
- [x] **Agent loop** — LLM → tool decision → execute → return result → continue
- [x] **Function calling protocol** — DeepSeek tool_call / tool_result message format
- [x] **Tool result rendering** — Display command output in the TUI
- [x] **Reasoning content display** — DeepSeek reasoning models: real-time streaming + collapsible "thinking" block (press `Ctrl+R` to toggle)
- [x] **Provider-neutral runtime** — Canonical messages and provider adapter boundary
- [x] **Durable execution journal** — Write-ahead records, checksummed WAL, snapshots, and recovery
- [x] **App controller and frame scheduler** — Explicit UI commands with dirty/deadline-driven drawing
- [x] **Workspace coding tools** — Read, write, edit, glob, and grep with bounded outputs

### Phase 1: Enhanced Tools

- [x] **Read tool** — Read file contents with line ranges
- [x] **Write tool** — Create new files
- [x] **Edit tool** — Apply string replacements (search + replace)
- [x] **Glob tool** — File search by pattern
- [x] **Grep tool** — Content search across files
- [ ] **Diff preview** — Show proposed changes before applying

### Phase 2: Safety & Control

- [x] **Persistent session recovery** — Snapshot/WAL storage, replay, and explicit reconciliation
- [x] **Direct tool execution** — Built-in tools run with the Rua process permissions, following pi's core model
- [ ] **Bash sandbox** — Workspace cwd, timeout, and process-tree cleanup are implemented; stronger OS isolation remains
- [ ] **Git integration** — Auto-stage changes, generate commit messages
- [ ] **Undo / rollback** — Revert last tool action

### Phase 3: Multi-Provider Support

- [x] **Provider trait** — Abstract LLM client interface
- [ ] **OpenAI** — GPT-4o, o1, o3 support
- [ ] **Anthropic** — Claude Sonnet, Opus support
- [ ] **Local models** — Ollama / llama.cpp compatibility
- [ ] **Model switching** — Runtime model selection

### Phase 4: Enhanced UX

- [ ] **Syntax highlighting** — Highlight code blocks in responses
- [ ] **Multi-line input** — Shift+Enter for newlines, Esc to send
- [ ] **Slash commands** — `/clear`, `/help`, `/model`, `/history`
- [x] **Message persistence** — Project-local sessions under `.rua/sessions`
- [ ] **Token/cost tracking** — Display usage stats per turn
- [ ] **Scrollback search** — Search conversation history

### Phase 5: Advanced Features

- [ ] **MCP support** — Model Context Protocol for external tools
- [ ] **Project indexing** — RAG over codebase for better context
- [ ] **Image input** — Vision model support for screenshots
- [ ] **Parallel tool calls** — Execute independent tools concurrently
- [ ] **Custom tools** — User-defined tool scripts
- [ ] **Workspace awareness** — `.rua/` project-local config and rules

## Configuration

See [docs/config.md](docs/config.md) for configuration options.

## Development

```bash
# Check
cargo check

# Run
cargo run

# Build release
cargo build --release
```

Sessions are created under the current project's `.rua/sessions` directory. Rua prints the session ID at startup; reopen one with:

```bash
cargo run -- --session <session-id>
```

Session maintenance is explicit and non-interactive:

```bash
cargo run -- --validate-session <session-id>
cargo run -- --export-session <session-id> <destination>
cargo run -- --repair-session <session-id>
```

Normal recovery automatically removes only an incomplete trailing frame after validating the retained WAL prefix. The maintenance repair command performs the same narrow operation offline, saves the original journal beside the session, and refuses checksum or middle-record corruption.

If recovery finds a tool that may already have produced side effects, use `/recovery inspect`, then explicitly resolve it with `/recovery success`, `/recovery failed`, `/recovery retry`, or `/recovery abandon`.

Rua does not show built-in permission popups. Run it in a container or OS sandbox when stronger isolation is required.

## License

MIT
