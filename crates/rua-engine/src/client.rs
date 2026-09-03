//! Engine construction from core config: per-provider rig clients + on-demand
//! model handles.
//!
//! Two provider kinds are wired:
//! - `"deepseek"` — DeepSeek API (rig's deepseek provider)
//! - `"openai"` — any OpenAI-compatible Chat Completions endpoint (incl.
//!   oMLX at `http://127.0.0.1:8000/v1`); `reasoning_content` streaming
//!   is handled by rig's shared OpenAI-compatible adapter.
//!
//! Multiple providers are registered by name (config `[[providers]]`); a
//! model ref is `"provider/model"`, a bare model name resolves to the
//! default provider (`crate::config::DEFAULT_PROVIDER`).

use std::collections::HashMap;
use std::path::PathBuf;

use rig_core::client::CompletionClient;
use rig_core::completion::{CompletionError, CompletionModel, CompletionRequest, CompletionResponse};
use rig_core::providers::{deepseek, openai};
use rig_core::streaming::StreamingCompletionResponse;
use rig_core::wasm_compat::WasmCompatSend;
use crate::config::{ProviderConfig, parse_model_ref};

use crate::error::{Error, Result};
use crate::tools::BashTool;

/// The agent engine: owns per-provider rig clients and the bash tool
/// executor.
///
/// Deliberately has no view of `rua_graph::Graph`; it consumes assembled
/// history and produces turn nodes.
pub struct Engine {
    pub(crate) providers: HashMap<String, ProviderRuntime>,
    pub(crate) bash: BashTool,
    /// Graph-growing tools (spawn_turn/inspect) go through this handle.
    /// Late-bound via `set_spawner` because the spawner (rua-server's
    /// runtime) itself holds the engine — a construction cycle.
    pub(crate) spawner: std::sync::OnceLock<std::sync::Arc<dyn crate::spawn::TurnSpawner>>,
}

/// One provider's rig client + request extras.
pub(crate) struct ProviderRuntime {
    client: ProviderClient,
    additional_params: Option<serde_json::Value>,
}

/// Provider-erased client; `completion_model(name)` is a cheap name binding.
pub(crate) enum ProviderClient {
    DeepSeek(deepseek::Client),
    OpenAi(openai::Client),
}

impl ProviderClient {
    fn completion_model(&self, model: &str) -> Model {
        match self {
            ProviderClient::DeepSeek(c) => Model::DeepSeek(c.completion_model(model)),
            // completions_api() 按值转换 client，clone 一份（内部是 Arc，廉价）。
            ProviderClient::OpenAi(c) => {
                Model::OpenAi(c.clone().completions_api().completion_model(model))
            }
        }
    }
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
    /// Build from named providers (`Config::all_providers()` output: the
    /// default provider named `"default"` first, then the named ones).
    pub fn new(
        providers: &[(String, ProviderConfig)],
        project_root: impl Into<PathBuf>,
    ) -> Result<Self> {
        let mut map = HashMap::new();
        for (name, config) in providers {
            let client = match config.kind.as_str() {
                "deepseek" => ProviderClient::DeepSeek(
                    deepseek::Client::builder()
                        .api_key(config.api_key.as_str())
                        .base_url(config.base_url.as_str())
                        .build()?,
                ),
                // OpenAI-compatible Chat Completions (oMLX, LM Studio,
                // llama.cpp, …). rig 默认的 openai Client 是 Responses
                // API；本地端点走 Chat Completions，在 completion_model
                // 时用 `completions_api()` 转换。
                "openai" | "lmstudio" => ProviderClient::OpenAi(
                    openai::Client::builder()
                        .api_key(config.api_key.as_str())
                        .base_url(config.base_url.as_str())
                        .build()?,
                ),
                other => return Err(Error::UnsupportedProvider(other.to_string())),
            };
            let additional_params = if config.additional_params.is_empty() {
                None
            } else {
                Some(serde_json::Value::Object(config.additional_params.clone()))
            };
            map.insert(
                name.clone(),
                ProviderRuntime {
                    client,
                    additional_params,
                },
            );
        }
        Ok(Self {
            providers: map,
            bash: BashTool::new(project_root.into()),
            spawner: std::sync::OnceLock::new(),
        })
    }

    /// Resolve a model ref (`"provider/model"` or bare = default provider)
    /// to a model handle + that provider's additional_params.
    pub(crate) fn model_for(&self, model_ref: &str) -> Result<(Model, Option<serde_json::Value>)> {
        let (pname, mname) = parse_model_ref(model_ref);
        let rt = self
            .providers
            .get(pname)
            .ok_or_else(|| Error::UnknownProvider(pname.to_string()))?;
        Ok((rt.client.completion_model(mname), rt.additional_params.clone()))
    }

    /// Inject the runtime's `TurnSpawner` (rua-server). Called once after the
    /// shared state exists; before that, spawn_turn/inspect are not offered.
    pub fn set_spawner(&self, spawner: std::sync::Arc<dyn crate::spawn::TurnSpawner>) {
        let _ = self.spawner.set(spawner);
    }
}
