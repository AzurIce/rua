//! rua-engine: the agent loop for rua.
//!
//! Builds an LLM client (rig-core, DeepSeek) from `config`, maps the
//! `CoreMessage` IR onto rig messages, runs a turn (streaming LLM calls + bash
//! tool loop), and produces a `rua_graph::Node` (kind = Turn) for the server to
//! commit. The engine never touches `rua_graph::Graph`.

pub mod assemble;
pub mod client;
pub mod config;
pub mod distill;
pub mod error;
pub mod message;
pub mod prompt;
pub mod script;
pub mod tools;
pub mod turn;

pub use assemble::assemble;
pub use client::Engine;
pub use error::{Error, Result};
pub use script::{ScriptHost, MAX_SPAWN_DEPTH};
pub use turn::{MAX_TOOL_ROUNDS, TurnParams};
