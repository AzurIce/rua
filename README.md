# rua

> **Experimental / work in progress.** This is a research playground for a
> session-graph agent model. Expect breaking changes to the storage format,
> the API, and the UI at any time.

rua is a graph-native AI coding agent in Rust. Instead of the classic
"one main agent, one linear session" shape, conversations are organized as a
**graph** — and the agent itself can grow that graph.

## The idea

The classic agent loop is:

```
input → a turn (tool calls + thinking) → output → next input → next turn → …
```

Model this as a graph: every turn is a node, every input is an edge, and a
traditional "session" is just one chain in that graph.

- **Sessions are chains; switching sessions is moving a pointer.** A cursor
  (HEAD) points at a node; its "history" is simply the sequence of its
  ancestors, assembled as the context for the next turn.
- **Forking is trivial.** Pick any node, type an input, and a new branch
  grows from it — as many parallel investigations as you like, no session
  juggling.
- **Graph operations are tools given to the agent itself.** The agent can
  *continue* a node (fork a new turn from any committed turn, with a task as
  its input) and *inspect* a node (wait for it to commit and read its
  result). Subagents are not a separate abstraction — they emerge from the
  agent growing the graph. A goal loop falls out naturally: one chain
  delegates subtasks to other chains, inspects their outcomes, and continues
  until satisfied.
- **Compaction is a node too.** Summarizing several turns produces a
  *context node*: distilled material that any branch can reference as input.
  "Forgetting" is just starting a fresh chain carrying only the summary —
  nothing is lost, everything stays inspectable in the graph.
- **The subject is unbound from any single session.** There is no privileged
  "main line of consciousness" that gets lossily compacted over and over;
  identity lives in the graph as a whole, and work can be handed off between
  chains explicitly.

The outlook: the whole graph, with all its parallel agents, *is* the agent.
Wrap it in a black box — external input lands on a rootless turn node, the
agent extends the network by itself, and eventually an output emerges. Such
an agent has memory for free: its own history is the graph it lives in.

## What exists today

- Immutable nodes: **Input** (the edge-as-node, carrying the request's tool
  set), **Turn** (LLM calls + tool executions + outcome, recording the
  effective tool set), **Context** (distilled material).
- Cursor registry with movable HEADs; journal-based event sourcing
  (replayed at startup; unfinished turns are marked interrupted).
- An agent loop with a bash tool plus the graph-growing tools
  `spawn_turn` / `inspect` — with per-node tool capabilities that attenuate
  monotonically along spawn edges, and a system prompt assembled dynamically
  from the effective tool set.
- A daemon (`rua`) owning the graph with a REST + WebSocket API, and a web
  UI (chat view + interactive graph view, context-assembly sidebar, usage /
  prompt-cache visibility).

## Architecture

```
crates/
├── rua-graph    # Graph model (pure library; no rig/axum/tokio):
│                #   nodes, cursors, journal, chain loading
├── rua-engine   # Agent loop: rig-core 0.42 (OpenAI-compatible / DeepSeek),
│                #   provider config, graph→history assembly, tools,
│                #   EffectiveTools, dynamic system prompt
├── rua-server   # Daemon (binary `rua`): graph + runtime, REST + WS API,
│                #   serves the built web UI (127.0.0.1 only)
└── rua-ui       # Dioxus 0.7 web UI: chat view + graph view
```

Storage is per-project: `<project>/.rua/graphs/<name>/` holds immutable node
bodies (`nodes/<ulid>.json`, atomic temp+rename writes) and the journal
(`journal.jsonl`). Multiple named graphs are supported.

## Run

Prerequisites: Rust (workspace edition 2024); for the UI, `dioxus-cli`
0.7.10 with the `wasm32-unknown-unknown` target (this repo's Nix devShell
pins both — `nix develop`).

```bash
# 1. Configure a provider (see below), then start the daemon:
cargo run -p rua-server            # binds 127.0.0.1:3080, graph in ./.rua/

# 2. Build the web UI once (the daemon serves it):
dx build -p rua-ui --platform web

# 3. Open http://127.0.0.1:3080
```

For UI development with hot reload:

```bash
dx serve -p rua-ui --platform web  # http://127.0.0.1:8080
```

The dev server has no API routes; the UI detects port 8080 and redirects its
API/WS calls to the daemon at 127.0.0.1:3080 (CORS is open on the daemon).

Daemon options: `--port <PORT>`, `--project-root <PATH>` (the graph lives at
`<PATH>/.rua/graphs/` and the bash tool runs with `<PATH>` as cwd).

## Configuration

Config lives at `~/.config/rua/config.toml` (auto-created with defaults on
first run). Minimal example:

```toml
# DeepSeek API
model = "deepseek/deepseek-v4-pro"  # current model: full "provider/model" ref

[[providers]]
name = "deepseek"
kind = "deepseek"                   # or "openai" for any OpenAI-compatible endpoint
api_key = "$DEEPSEEK_API_KEY"       # env var, "!shell command", or literal
base_url = "https://api.deepseek.com"

[server]
port = 3080
```

Providers are a flat named registry (`[[providers]]`); the current model is a
top-level `model` ref pointing at one of them and is validated at load. The
UI model picker merges each provider's optional static `models` list with its
live `GET {base_url}/models`.

Full reference: [docs/config.md](docs/config.md).

## Development

```bash
cargo check --workspace
cargo test -j8 -- --test-threads=4   # resource constraints: limit parallelism
```

## License

MIT
