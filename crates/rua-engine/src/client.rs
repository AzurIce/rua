//! Engine construction from core config: rig client + model.
//!
//! Two provider kinds are wired:
//! - `"deepseek"` — DeepSeek API (rig's deepseek provider)
//! - `"openai"` — any OpenAI-compatible Chat Completions endpoint (incl.
//!   LM Studio at `http://127.0.0.1:1234/v1`); `reasoning_content` streaming
//!   is handled by rig's shared OpenAI-compatible adapter.

use std::path::PathBuf;

use rig_core::client::CompletionClient;
use rig_core::completion::{CompletionError, CompletionModel, CompletionRequest, CompletionResponse};
use rig_core::providers::{deepseek, openai};
use rig_core::streaming::StreamingCompletionResponse;
use rig_core::wasm_compat::WasmCompatSend;
use rua_core::config::ProviderConfig;

use crate::error::{Error, Result};
use crate::tools::BashTool;

/// The agent engine: owns the LLM model handle and the bash tool executor.
///
/// Deliberately has no view of `rua_core::Graph`; it consumes assembled
/// history and produces turn nodes.
pub struct Engine {
    pub(crate) model: Model,
    pub(crate) additional_params: Option<serde_json::Value>,
    pub(crate) bash: BashTool,
    /// Graph-growing tools (spawn_turn/inspect) go through this handle.
    /// Late-bound via `set_spawner` because the spawner (rua-server's
    /// runtime) itself holds the engine — a construction cycle.
    pub(crate) spawner: std::sync::OnceLock<std::sync::Arc<dyn crate::spawn::TurnSpawner>>,
}

/// Provider-erased completion model.
#[derive(Clone)]
pub(crate) enum Model {
    DeepSeek(deepseek::CompletionModel),
    OpenAi(openai::completion::CompletionModel),
}

impl CompletionModel for Model {
    fn completion(
        &self,
        request: CompletionRequest,
    ) -> impl std::future::Future<Output = std::result::Result<CompletionResponse, CompletionError>>
    + WasmCompatSend {
        async move {
            match self {
                Model::DeepSeek(m) => m.completion(request).await,
                Model::OpenAi(m) => m.completion(request).await,
            }
        }
    }

    fn stream(
        &self,
        request: CompletionRequest,
    ) -> impl std::future::Future<
        Output = std::result::Result<StreamingCompletionResponse, CompletionError>,
    > + WasmCompatSend {
        async move {
            match self {
                Model::DeepSeek(m) => CompletionModel::stream(m, request).await,
                Model::OpenAi(m) => CompletionModel::stream(m, request).await,
            }
        }
    }
}

impl Engine {
    pub fn new(config: &ProviderConfig, project_root: impl Into<PathBuf>) -> Result<Self> {
        let model = match config.kind.as_str() {
            "deepseek" => Model::DeepSeek(
                deepseek::Client::builder()
                    .api_key(config.api_key.as_str())
                    .base_url(config.base_url.as_str())
                    .build()?
                    .completion_model(config.model.as_str()),
            ),
            // OpenAI-compatible Chat Completions (LM Studio, llama.cpp, …).
            // rig 默认的 openai Client 是 Responses API；本地端点走
            // Chat Completions，用 `completions_api()` 转换。
            "openai" | "lmstudio" => Model::OpenAi(
                openai::Client::builder()
                    .api_key(config.api_key.as_str())
                    .base_url(config.base_url.as_str())
                    .build()?
                    .completions_api()
                    .completion_model(config.model.as_str()),
            ),
            other => return Err(Error::UnsupportedProvider(other.to_string())),
        };
        let additional_params = if config.additional_params.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(config.additional_params.clone()))
        };
        Ok(Self {
            model,
            additional_params,
            bash: BashTool::new(project_root.into()),
            spawner: std::sync::OnceLock::new(),
        })
    }

    /// Inject the runtime's `TurnSpawner` (rua-server). Called once after the
    /// shared state exists; before that, spawn_turn/inspect are not offered.
    pub fn set_spawner(&self, spawner: std::sync::Arc<dyn crate::spawn::TurnSpawner>) {
        let _ = self.spawner.set(spawner);
    }
}
