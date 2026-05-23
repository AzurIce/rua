# rua

A terminal-based AI coding agent built in Rust.

## Current Status

rua is a **streaming chat agent** with a TUI built on ratatui. It connects to the DeepSeek API, supports real-time multi-turn conversation, and can execute tools (currently `bash`) in the agent loop.

## Architecture

```
main.rs           Terminal lifecycle + event routing loop
├── app/
│   ├── state.rs    Pure UI state + state transitions
│   ├── render.rs   ratatui drawing (with reasoning toggle)
│   ├── input.rs    Keyboard event handling
│   └── event.rs    UiEvent enum
├── session.rs      Agent loop: LLM → tool calls → execute → re-request
├── deepseek.rs     HTTP client + SSE parsing (supports tool_calls / reasoning_content)
├── tools.rs        Tool definitions (BashTool) + ToolRegistry
├── model.rs        ChatEntry, Role, reasoning_content (domain models)
└── config.rs       TOML config + value resolution
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
- [x] **Reasoning content display** — DeepSeek reasoning models: real-time streaming + collapsible "thinking" block (press `r` to toggle)

### Phase 1: Enhanced Tools

- [ ] **Read tool** — Read file contents with line ranges
- [ ] **Write tool** — Create new files
- [ ] **Edit tool** — Apply string replacements (search + replace)
- [ ] **Glob tool** — File search by pattern
- [ ] **Grep tool** — Content search across files
- [ ] **Diff preview** — Show proposed changes before applying

### Phase 2: Safety & Control

- [ ] **Approval modes** — auto / ask / never for dangerous operations
- [ ] **Bash sandbox** — Restricted shell execution (cwd, timeout, denylist)
- [ ] **Git integration** — Auto-stage changes, generate commit messages
- [ ] **Undo / rollback** — Revert last tool action

### Phase 3: Multi-Provider Support

- [ ] **Provider trait** — Abstract LLM client interface
- [ ] **OpenAI** — GPT-4o, o1, o3 support
- [ ] **Anthropic** — Claude Sonnet, Opus support
- [ ] **Local models** — Ollama / llama.cpp compatibility
- [ ] **Model switching** — Runtime model selection

### Phase 4: Enhanced UX

- [ ] **Syntax highlighting** — Highlight code blocks in responses
- [ ] **Multi-line input** — Shift+Enter for newlines, Esc to send
- [ ] **Slash commands** — `/clear`, `/help`, `/model`, `/history`
- [ ] **Message persistence** — Save/load conversation history
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

## License

MIT
