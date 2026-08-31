//! Minimal abstraction over the agent engine so integration tests can inject
//! a mock. Object-safe via boxed futures; the production impl is `Engine`.

use std::future::Future;
use std::pin::Pin;

use rua_core::node::Node;
use rua_core::TurnEvent;
use rua_engine::{Engine, TurnParams};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

pub trait AgentEngine: Send + Sync {
    /// `Engine::run_turn` is `!Send` (it holds `&dyn Fn` across awaits), so
    /// the returned future deliberately has no `Send` bound; callers must
    /// drive it on a current-thread context (see `turn::spawn_turn`).
    fn run_turn<'a>(
        &'a self,
        params: TurnParams,
        events: UnboundedSender<TurnEvent>,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = rua_engine::Result<Node>> + 'a>>;

    fn summarize<'a>(
        &'a self,
        material: &'a str,
    ) -> Pin<Box<dyn Future<Output = rua_engine::Result<String>> + Send + 'a>>;
}

impl AgentEngine for Engine {
    fn run_turn<'a>(
        &'a self,
        params: TurnParams,
        events: UnboundedSender<TurnEvent>,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = rua_engine::Result<Node>> + 'a>> {
        Box::pin(Engine::run_turn(self, params, events, cancel))
    }

    fn summarize<'a>(
        &'a self,
        material: &'a str,
    ) -> Pin<Box<dyn Future<Output = rua_engine::Result<String>> + Send + 'a>> {
        Box::pin(Engine::summarize(self, material))
    }
}
