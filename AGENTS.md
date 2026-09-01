# rua 项目说明

会话图模型的 agent（daemon + Dioxus Web UI）。Workspace 四 crate：

- `crates/rua-core` — 图引擎（纯库，不依赖 rig/axum/tokio）：不可变节点（Input/Turn/Context）、cursor 注册表、journal 事件溯源、装配纯函数
- `crates/rua-engine` — agent loop：rig-core 0.42（OpenAI-compatible / DeepSeek；多 provider 经 `[[providers]]` 具名注册，model ref = `"provider/model"`，裸名走默认 provider）、工具（bash，行为对齐 pi：尾部截断 2000 行/50KB + 全文落盘、`exec 2>&1` 交错、无默认超时、进程组击杀；spawn_turn/inspect 图生长工具经 `TurnSpawner` trait 注入）、`run_turn(...) -> Node`
- `crates/rua-server` — daemon（二进制 `rua`）：独占图 + 运行时，REST/WS API，只绑 127.0.0.1；`ServerSpawner` 实现 spawn_turn/inspect（spawn 复用 `/api/inputs` 原子路径，递归上限 4）
- `crates/rua-ui` — Dioxus 0.7 web UI：聊天视图（markdown 渲染）+ 图视图（基于 `../dioxus-flow` 画布库）

存储：`<project>/.rua/graphs/<name>/`（多图，每图 `nodes/<ulid>.json` + `journal.jsonl`；旧 `.rua/graph/` 启动时自动迁移为 `graphs/default`；删除图移入 `graphs/.trash/`）。
测试遵守资源约束：`cargo test -j8 -- --test-threads=4`。
UI 构建：`nix develop` 的 devShell 已 pin dioxus-cli 0.7.10 + wasm-bindgen-cli 0.2.121（dx 0.7.10 只接受此版本）+ wasm32 target；`dx build/serve -p rua-ui --platform web` 直接用，dioxus 依赖必须保持 `=0.7.10`。
本机默认 `cc` 是 nix gcc（缺 `-liconv`），跑含 C 依赖的 cargo 命令要用 `nix develop --command ...` 或 `PATH="/usr/bin:/bin:$PATH"`。

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
