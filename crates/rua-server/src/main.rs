//! `rua` daemon entry point: parse CLI args, load config, open the graph,
//! build the engine, and serve the API on 127.0.0.1.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use rua_core::config::Config;
use rua_core::graph::Graph;
use rua_engine::Engine;
use rua_server::state::AppState;

const HELP: &str = "\
rua — session-graph coding agent daemon

USAGE:
    rua [OPTIONS]

OPTIONS:
    --port <PORT>              Port to listen on (default: config server.port, else 3080)
    --project-root <PATH>      Project root; the graph lives at <PATH>/.rua/graph
                               and the bash tool runs with <PATH> as cwd (default: cwd)
    -h, --help                 Print this help

The server binds to 127.0.0.1 only. Config is read from
~/.config/rua/config.toml ([provider] + [server] sections).

API overview:
    GET  /api/graph                  node metas + cursors + interrupted turns
    GET  /api/nodes/{id}             full node body
    GET  /api/cursors                list cursors
    POST /api/cursors                create cursor  {\"actor\": ..., \"capabilities\": [...]}
    GET  /api/cursors/{id}/chain     conversation chain root->tip
    POST /api/cursors/{id}/input     commit input + start turn  {\"text\": ...}
    POST /api/cursors/{id}/move      move cursor (fork/rewind)  {\"node_id\": ...}
    POST /api/cursors/{id}/cancel    cancel the in-flight turn
    POST /api/summarize              distill nodes into a context node  {\"sources\": [...]}
    GET  /api/ws                     WebSocket event stream
";

struct Options {
    port: Option<u16>,
    project_root: PathBuf,
}

fn parse_args() -> Result<Options, String> {
    let mut port = None;
    let mut project_root = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                std::process::exit(0);
            }
            "--port" => {
                let value = args.next().ok_or("--port requires a value")?;
                port = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid port: {value:?}"))?,
                );
            }
            "--project-root" => {
                project_root = PathBuf::from(args.next().ok_or("--project-root requires a value")?);
            }
            other => return Err(format!("unknown argument {other:?} (try --help)")),
        }
    }
    Ok(Options {
        port,
        project_root,
    })
}

#[tokio::main]
async fn main() {
    let opts = match parse_args() {
        Ok(opts) => opts,
        Err(e) => {
            eprintln!("rua: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(opts).await {
        eprintln!("rua: {e}");
        std::process::exit(1);
    }
}

async fn run(opts: Options) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let port = opts.port.unwrap_or(config.server.port);

    let rua_dir = opts.project_root.join(".rua");
    rua_server::graphs::migrate_legacy(&rua_dir)?;
    let graphs_root = rua_dir.join("graphs");
    let default_dir = graphs_root.join(rua_server::graphs::DEFAULT_GRAPH);
    let graph = Graph::open(&default_dir)?;
    let engine = Engine::new(&config.provider, &opts.project_root)?;
    let engine = Arc::new(engine);
    let state = Arc::new(AppState::new(
        graph,
        engine.clone(),
        config.provider.model.clone(),
        graphs_root,
        rua_server::graphs::DEFAULT_GRAPH.to_string(),
        config.provider.clone(),
    ));
    // 注入图生长工具的运行时后端（循环依赖：spawner 持有 state，
    // state 持有 engine——所以 late-bind）。
    engine.set_spawner(Arc::new(rua_server::spawn::ServerSpawner::new(state.clone())));

    let ui_dist = find_ui_dist();
    if let Some(d) = &ui_dist {
        println!("rua: serving web UI from {}", d.display());
    }
    let app = rua_server::build_router(state, ui_dist);

    // Loopback only: the bash tool runs without approval, so the API must
    // not be reachable from the network.
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("rua: listening on http://{addr}");
    println!("rua: graph at {}", default_dir.display());
    axum::serve(listener, app).await?;
    Ok(())
}

/// Locate the built Dioxus web bundle. dx 0.7 outputs to
/// `target/dx/rua-ui/<profile>/web/public`; also accept a legacy
/// `crates/rua-ui/dist`. Paths are relative to the rua repo (found via the
/// server crate's manifest dir), NOT the user's project root.
fn find_ui_dist() -> Option<std::path::PathBuf> {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()? // crates/
        .parent()?; // repo root
    let candidates = [
        repo.join("target/dx/rua-ui/release/web/public"),
        repo.join("target/dx/rua-ui/debug/web/public"),
        repo.join("crates/rua-ui/dist"),
    ];
    candidates
        .into_iter()
        .find(|p| p.join("index.html").is_file())
}
