//! rua-engine: the agent loop for rua.
//!
//! Builds an LLM client (rig-core, DeepSeek) from `rua_core::config`, maps the
//! `CoreMessage` IR onto rig messages, runs a turn (streaming LLM calls + bash
//! tool loop), and produces a `rua_core::Node` (kind = Turn) for the server to
//! commit. The engine never touches `rua_core::Graph`.

pub mod client;
pub mod distill;
pub mod error;
pub mod message;
pub mod spawn;
pub mod tools;
pub mod turn;

pub use client::Engine;
pub use error::{Error, Result};
pub use spawn::{InspectOutcome, SpawnedTurn, TurnSpawner};
pub use turn::{MAX_TOOL_ROUNDS, TurnParams};
