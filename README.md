# rua

A graph-native AI coding agent in Rust. Agent sessions are organized as a
**graph** instead of a fixed main-agent/subagent hierarchy: every turn is an
immutable node, sessions are movable cursors onto the graph, and any actor
(human / orchestrator / subagent) can attach at any node.

Design doc (Chinese): 会话图模型设计（`Projects/P2026-05-20 Rua/会话图模型设计.md`）.

## Architecture

```
crates/
├── rua-core     # Graph engine (pure library; no rig/axum/tokio)
│                #   immutable nodes (Input/Turn/Context), cursors, in-flight
│                #   turn handles, journal event sourcing, assembly (纯函数)
├── rua-engine   # Agent loop: rig-core 0.42 (DeepSeek streaming + reasoning),
│                #   bash tool, run_turn(...) -> committed Turn node
├── rua-server   # Daemon (binary `rua`): owns graph + runtime, REST + WS API,
│                #   serves the web UI
└── rua-ui       # Dioxus web UI: chat view + graph view (actor = human client)
```

Storage is per-project: `<project>/.rua/graph/` holds immutable node bodies
(`nodes/<ulid>.json`, temp+rename atomic writes) and the control-plane journal
(`journal.jsonl`, replayed at startup; unfinished turns are marked
interrupted).

## Run

```bash
# daemon (binds 127.0.0.1:3080 by default; graph stored in ./.rua/graph)
cargo run -p rua-server

# web UI (dev, hot reload; talks to the daemon over REST/WS)
dx serve -p rua-ui --platform web
```

## Development

```bash
cargo check --workspace
# resource constraints on this machine: limit build and test parallelism
cargo test -j8 -- --test-threads=4
```

## Configuration

See [docs/config.md](docs/config.md).

## License

MIT
