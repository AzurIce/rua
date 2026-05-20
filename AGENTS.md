# External Resources

When relevant context is needed beyond this workspace, the following sibling directories under `../` are available for reference.

## `../codex/`

OpenAI's **Codex CLI** and **codex-rs** (Rust-based TUI agent).

- **Languages**: Rust, TypeScript
- **Key areas**: Terminal UI (ratatui), sandboxed execution (Seatbelt), LLM protocol, app-server API, MCP tool calls
- **Build tools**: Bazel, Cargo, `just`
- **Reference for**: Rust TUI patterns, sandbox architecture, LLM streaming protocol design, snapshot testing with `insta`
- **Entry docs**: `codex-rs/` (Rust workspace), `codex-cli/` (TypeScript CLI), `docs/`

## `../claude-code-sourcemap/`

Extracted source of **Claude Code** (Anthropic's agentic coding tool).

- **Languages**: TypeScript
- **Key areas**: Agent loop, tool use, codebase exploration, context management
- **Reference for**: Claude Code's internal architecture, tool definitions, and interaction patterns
- **Entry docs**: `restored-src/`, `README.md`

## `../pi-mono/`

**pi** — a terminal-based AI coding agent (monorepo).

- **Languages**: TypeScript
- **Key areas**: TUI, coding agent, AI streaming abstractions (`packages/ai`), model providers, keybindings
- **Build tools**: npm, Vitest
- **Reference for**: Multi-provider LLM streaming design, agent test harness, TUI keybinding patterns, monorepo structure
- **Entry docs**: `packages/ai/`, `packages/coding-agent/`, `packages/tui/`

## `../opencode/`

**OpenCode** — open-source AI coding assistant.

- **Languages**: TypeScript
- **Key areas**: Agent configuration, SDK generation, session management, Drizzle schemas
- **Build tools**: Bun, Turbo
- **Reference for**: Agent config patterns, SDK build pipelines, functional TS style, schema design
- **Entry docs**: `packages/`, `specs/`, `sdks/`

---

> **Note**: These directories are read-only references. Do not modify them. If you need to borrow patterns or verify implementation details, read the relevant files directly from `../<project>/`.
