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
//! model ref is always `"provider/model"` — there is no default provider.

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
    /// The `script` tool goes through this handle (the server hosts the JS
    /// interpreter with the graph bindings). Late-bound via `set_script_host`
    /// because the host (rua-server's runtime) itself holds the engine — a
    /// construction cycle.
    pub(crate) script: std::sync::OnceLock<std::sync::Arc<dyn crate::script::ScriptHost>>,
}

/// One provider's rig client + request extras.
pub(crate) struct ProviderRuntime {
    client: ProviderClient,
    additional_params: Option<serde_json::Value>,
    /// 该 provider 的 LLM 调用重试策略（config 驱动）。
    pub(crate) retry: RetryPolicy,
}

/// 单次 LLM 调用的重试策略：只重试「还没吐出任何内容」的可重试失败。
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay_ms: u64,
}

/// 重试退避封顶（避免服务端要求的超长等待卡住整轮）。
pub(crate) const MAX_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

impl RetryPolicy {
    /// 第 n 次重试（从 1 计）前的等待时长：指数退避，封顶 [`MAX_RETRY_DELAY`]。
    pub fn delay_for(&self, attempt: u32) -> std::time::Duration {
        let ms = self
            .base_delay_ms
            .saturating_mul(1u64 << (attempt - 1).min(20));
        std::time::Duration::from_millis(ms).min(MAX_RETRY_DELAY)
    }
}

/// 启发式判断一次 LLM 调用错误是否值得重试（连接/超时/限流/服务端错误/
/// 流早断）。宽松匹配 rig 透传上来的错误文本。
pub(crate) fn is_retryable_llm_error(error: &str) -> bool {
    let e = error.to_lowercase();
    [
        "timeout",
        "timed out",
        "connect",
        "refused",
        "reset",
        "closed",
        "broken pipe",
        "eof",
        "429",
        "500",
        "502",
        "503",
        "504",
        "rate limit",
        "overloaded",
        "service unavailable",
        "internal server error",
        "bad gateway",
        "stream ended",
        "ended without",
        "terminated",
    ]
    .iter()
    .any(|k| e.contains(k))
}

/// 错误全文：顶层 Display 加整条 source 链（"; caused by: " 连接）。
/// reqwest 的超时类型藏在 source 里——顶层 Display 只有 "error sending
/// request for url (…)"，单看顶层会把可重试的超时漏判成永久失败（spawn
/// 子轮 120s 挂死后一次重试都没走的直接原因）。
pub(crate) fn error_chain_text(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        text.push_str("; caused by: ");
        text.push_str(&e.to_string());
        source = e.source();
    }
    text
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
    /// Build from named providers (`Config::all_providers()` output).
    pub fn new(
        providers: &[(String, ProviderConfig)],
        project_root: impl Into<PathBuf>,
    ) -> Result<Self> {
        let mut map = HashMap::new();
        for (name, config) in providers {
            // 共享 HTTP 客户端：连接超时 + 读空闲超时（两次读到数据之间的
            // 最大等待，含首字节前）。读空闲治「连接半死但无错误」的无限
            // 挂起——健康的流式调用不会长时间静默；0 = 不设。
            // 用 rig re-export 的 ReqwestClient：必须与 rig 内部用的
            // reqwest 同一版本/同一类型，单独引 reqwest 会拿到不同版本。
            let http = {
                let mut builder = rig_core::http_client::ReqwestClient::builder()
                    .connect_timeout(std::time::Duration::from_secs(config.connect_timeout_secs));
                if config.read_timeout_secs > 0 {
                    builder = builder
                        .read_timeout(std::time::Duration::from_secs(config.read_timeout_secs));
                }
                builder
                    .build()
                    .map_err(|e| Error::Config(format!("failed to build HTTP client: {e}")))?
            };
            let client = match config.kind.as_str() {
                "deepseek" => ProviderClient::DeepSeek(
                    deepseek::Client::builder()
                        .http_client(http)
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
                        .http_client(http)
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
                    retry: RetryPolicy {
                        max_retries: config.llm_max_retries,
                        base_delay_ms: config.llm_retry_base_ms,
                    },
                },
            );
        }
        Ok(Self {
            providers: map,
            bash: BashTool::new(project_root.into()),
            script: std::sync::OnceLock::new(),
        })
    }

    /// Resolve a model ref (`"provider/model"` — bare names are invalid)
    /// to a model handle + that provider's additional_params + retry policy.
    pub(crate) fn model_for(
        &self,
        model_ref: &str,
    ) -> Result<(Model, Option<serde_json::Value>, RetryPolicy)> {
        let (pname, mname) = parse_model_ref(model_ref)?;
        let rt = self
            .providers
            .get(pname)
            .ok_or_else(|| Error::UnknownProvider(pname.to_string()))?;
        Ok((
            rt.client.completion_model(mname),
            rt.additional_params.clone(),
            rt.retry,
        ))
    }

    /// Inject the runtime's `ScriptHost` (rua-server). Called once after the
    /// shared state exists; before that, the script tool is not offered.
    pub fn set_script_host(&self, host: std::sync::Arc<dyn crate::script::ScriptHost>) {
        let _ = self.script.set(host);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// reqwest 超时的形状：顶层 Display 不含任何可重试关键词，"timed out"
    /// 在 source 链里。
    #[derive(Debug)]
    struct OpaqueEnvelope(std::io::Error);
    impl std::fmt::Display for OpaqueEnvelope {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "error sending request for url (https://api.example.com)")
        }
    }
    impl std::error::Error for OpaqueEnvelope {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn timeout_in_source_chain_is_retryable() {
        let e = OpaqueEnvelope(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "operation timed out",
        ));
        // 只看顶层 Display 会漏判（曾经的 bug：挂满读超时的调用零重试）。
        assert!(!is_retryable_llm_error(&e.to_string()));
        assert!(is_retryable_llm_error(&error_chain_text(&e)));
    }

    #[test]
    fn non_retryable_error_chain_stays_unclassified() {
        let e = OpaqueEnvelope(std::io::Error::other("invalid api key"));
        assert!(!is_retryable_llm_error(&error_chain_text(&e)));
    }
}
