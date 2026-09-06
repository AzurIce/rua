use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::error::{Error, Result};

/// Cache for shell command results (persists for process lifetime).
static COMMAND_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, String>> {
    COMMAND_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// rua configuration, read from `~/.config/rua/config.toml`.
///
/// Provider 注册表统一在 `[[providers]]`（每项具名），当前模型是顶层
/// `model`（完整 ref，指向某个已注册 provider）。没有隐式的默认 provider。
#[derive(Debug, Deserialize, Default, Clone)]
pub struct Config {
    /// 当前模型：`"provider/model"`，发送时不指定模型的请求落到它。
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub providers: Vec<NamedProviderConfig>,
    #[serde(default)]
    pub server: ServerConfig,
}

/// `[[providers]]` 里的一项：名字 + 扁平展开的 provider 字段。
#[derive(Debug, Deserialize, Clone)]
pub struct NamedProviderConfig {
    pub name: String,
    #[serde(flatten)]
    pub provider: ProviderConfig,
}

/// 模型引用：`"provider名/模型名"`。模型名本身可以含 `/`（按第一个
/// `/` 切）；裸名（无 `/`）无效——ref 必须带 provider 前缀。
pub fn parse_model_ref(model_ref: &str) -> Result<(&str, &str)> {
    match model_ref.split_once('/') {
        Some((p, m)) if !p.is_empty() && !m.is_empty() => Ok((p, m)),
        _ => Err(Error::Config(format!(
            "invalid model ref {model_ref:?} (expected \"provider/model\")"
        ))),
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProviderConfig {
    /// Provider kind: "openai" (OpenAI-compatible endpoint, incl. LM Studio)
    /// or "deepseek".
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// 可选静态模型表：与动态 `GET {base_url}/models` 合并去重后进
    /// /api/models（UI 模型选择器）。provider 不可达或端点不支持时，
    /// 这张表是唯一展示。
    #[serde(default)]
    pub models: Vec<String>,
    /// Provider-specific request params, passed through verbatim
    /// (e.g. thinking toggles).
    #[serde(default)]
    pub additional_params: serde_json::Map<String, serde_json::Value>,
    /// TCP 连接超时（秒）。
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// 流式响应的读空闲超时（秒）：两次读到数据之间的最大等待（含首字节
    /// 前的等待）。治「连接半死但无错误」的无限挂起；0 = 不设。
    #[serde(default = "default_read_timeout_secs")]
    pub read_timeout_secs: u64,
    /// 单次 LLM 调用的最大重试次数。只重试「还没吐出任何内容」的可重试
    /// 失败（连接/超时/429/5xx/流早断）；流建立后失败不重试（会重复内容）。
    #[serde(default = "default_llm_max_retries")]
    pub llm_max_retries: u32,
    /// 重试退避基数（毫秒），按 2^n 指数增长，封顶 60s。
    #[serde(default = "default_llm_retry_base_ms")]
    pub llm_retry_base_ms: u64,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: default_kind(),
            api_key: String::new(),
            base_url: default_base_url(),
            models: Vec::new(),
            additional_params: serde_json::Map::new(),
            connect_timeout_secs: default_connect_timeout_secs(),
            read_timeout_secs: default_read_timeout_secs(),
            llm_max_retries: default_llm_max_retries(),
            llm_retry_base_ms: default_llm_retry_base_ms(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
        }
    }
}

fn default_kind() -> String {
    "openai".to_string()
}

fn default_base_url() -> String {
    "http://127.0.0.1:1234/v1".to_string()
}

fn default_port() -> u16 {
    3080
}

fn default_connect_timeout_secs() -> u64 {
    10
}

fn default_read_timeout_secs() -> u64 {
    120
}

fn default_llm_max_retries() -> u32 {
    3
}

fn default_llm_retry_base_ms() -> u64 {
    1000
}

/// Resolve a config value using pi-style rules:
/// - starts with "!": execute the rest as a shell command, stdout is the
///   value (cached for the process lifetime)
/// - starts with "$": explicit environment variable reference (error if
///   unset — an explicit reference that resolves to nothing is a config
///   mistake, not a literal)
/// - otherwise (pi semantics): try as an environment variable name, then
///   treat as literal
pub fn resolve_value(value: &str) -> Result<String> {
    if let Some(command) = value.strip_prefix('!') {
        {
            let cache = cache().lock().unwrap();
            if let Some(cached) = cache.get(command) {
                return Ok(cached.clone());
            }
        }

        let output = Command::new("sh").arg("-c").arg(command).output().map_err(|e| {
            Error::Config(format!("failed to execute shell command {command:?}: {e}"))
        })?;
        if !output.status.success() {
            return Err(Error::Config(format!(
                "shell command failed (exit {}): {command:?}",
                output.status.code().unwrap_or(-1)
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        cache()
            .lock()
            .unwrap()
            .insert(command.to_string(), stdout.clone());
        Ok(stdout)
    } else if let Some(var) = value.strip_prefix('$') {
        std::env::var(var).map_err(|_| {
            Error::Config(format!("environment variable {var:?} referenced by {value:?} is not set"))
        })
    } else {
        Ok(std::env::var(value).unwrap_or_else(|_| value.to_string()))
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(config_path())
    }

    pub fn load_from(path: PathBuf) -> Result<Self> {
        if !path.exists() {
            ensure_default_config(&path)?;
        }
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| Error::Config(format!("failed to read {}: {e}", path.display())))?;
        let mut config: Config = toml::from_str(&contents)
            .map_err(|e| Error::Config(format!("failed to parse {}: {e}", path.display())))?;

        // 校验：provider 名非空、不含 "/"、不重复；全局 model 必须是
        // 指向已注册 provider 的完整 ref。全部 fail loud。
        let mut seen = std::collections::HashSet::new();
        for p in &config.providers {
            if p.name.is_empty() || p.name.contains('/') {
                return Err(Error::Config(format!(
                    "invalid provider name {:?} (empty and \"/\" are not allowed)",
                    p.name
                )));
            }
            if !seen.insert(p.name.clone()) {
                return Err(Error::Config(format!(
                    "duplicate provider name {:?}",
                    p.name
                )));
            }
        }
        let (model_provider, _) = parse_model_ref(&config.model)?;
        if !config.providers.iter().any(|p| p.name == model_provider) {
            return Err(Error::Config(format!(
                "config model {:?} references unknown provider {model_provider:?}",
                config.model
            )));
        }
        for p in &mut config.providers {
            p.provider.api_key = resolve_value(&p.provider.api_key).map_err(|e| {
                Error::Config(format!("failed to resolve api_key of provider {:?}: {e}", p.name))
            })?;
        }
        Ok(config)
    }

    /// 全部 provider，配置文件顺序。
    pub fn all_providers(&self) -> Vec<(String, ProviderConfig)> {
        self.providers
            .iter()
            .map(|p| (p.name.clone(), p.provider.clone()))
            .collect()
    }
}

fn ensure_default_config(path: &std::path::Path) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;
    let default_contents = r#"# rua configuration file

# 当前模型：完整 ref "provider/model"，必须指向下面某个 [[providers]]。
# 发送时不指定模型的请求落到它。
model = "local/qwen3.8-27b-uncensored-mlx"

# Provider 注册表。kind: "openai" = 任意 OpenAI 兼容端点（默认：LM
# Studio 本地 server）；"deepseek" = DeepSeek API。
[[providers]]
name = "local"
kind = "openai"
# API key supports these formats (LM Studio accepts any non-empty string):
# 1. Literal: api_key = "lm-studio"
# 2. Env var name: api_key = "DEEPSEEK_API_KEY"
#    (explicit $ form also works: api_key = "$DEEPSEEK_API_KEY",
#     which errors if the variable is unset)
# 3. Shell command (pi-style ! prefix):
#    api_key = "!echo $DEEPSEEK_API_KEY"
#    api_key = "!security find-generic-password -s deepseek-api-key -w"
api_key = "lm-studio"
base_url = "http://127.0.0.1:1234/v1"
# 可选静态模型表：与动态 GET {base_url}/models 合并去重后进模型选择器；
# 端点不支持 /models 或 provider 不可达时，这张表是唯一展示。
# models = ["qwen3.8-27b-uncensored-mlx"]
# Provider-specific params passed through verbatim, e.g.:
# additional_params = { thinking = { type = "enabled" } }
# LLM HTTP timeouts & retry (defaults shown):
# connect_timeout_secs = 10
# read_timeout_secs = 120   # idle gap between stream reads; 0 disables
# llm_max_retries = 3       # only retries calls that produced no content yet
# llm_retry_base_ms = 1000  # exponential backoff base, capped at 60s

[server]
port = 3080
"#;
    std::fs::write(path, default_contents)?;
    Ok(())
}

pub fn config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("rua")
        .join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_literal() {
        assert_eq!(resolve_value("sk-literal").unwrap(), "sk-literal");
    }

    #[test]
    fn resolves_env_var() {
        unsafe { std::env::set_var("RUA_TEST_KEY", "from-env") };
        assert_eq!(resolve_value("RUA_TEST_KEY").unwrap(), "from-env");
    }

    #[test]
    fn resolves_dollar_env_var() {
        unsafe { std::env::set_var("RUA_TEST_DOLLAR", "from-dollar") };
        assert_eq!(resolve_value("$RUA_TEST_DOLLAR").unwrap(), "from-dollar");
    }

    #[test]
    fn dollar_env_var_unset_is_an_error() {
        unsafe { std::env::remove_var("RUA_TEST_MISSING") };
        assert!(resolve_value("$RUA_TEST_MISSING").is_err());
    }

    #[test]
    fn resolves_shell_command_with_cache() {
        let v = resolve_value("!echo hello-cache").unwrap();
        assert_eq!(v, "hello-cache");
        // second call hits the cache (same value, no re-execution)
        assert_eq!(resolve_value("!echo hello-cache").unwrap(), "hello-cache");
    }

    #[test]
    fn providers_are_parsed_and_keys_resolved() {
        unsafe { std::env::set_var("RUA_TEST_DS_KEY", "sk-ds-env") };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
model = "deepseek/deepseek-v4-pro"

[[providers]]
name = "local"
kind = "openai"
api_key = "lm-studio"
base_url = "http://127.0.0.1:8000/v1"
models = ["local-model", "small-model"]

[[providers]]
name = "deepseek"
kind = "deepseek"
api_key = "$RUA_TEST_DS_KEY"
base_url = "https://api.deepseek.com"
"#,
        )
        .unwrap();
        let cfg = Config::load_from(path).unwrap();
        let all = cfg.all_providers();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, "local");
        assert_eq!(all[0].1.models, vec!["local-model", "small-model"]);
        assert_eq!(all[1].0, "deepseek");
        assert_eq!(all[1].1.api_key, "sk-ds-env");
        assert_eq!(all[1].1.kind, "deepseek");
        assert_eq!(cfg.model, "deepseek/deepseek-v4-pro");
    }

    #[test]
    fn duplicate_provider_name_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "model = \"a/m\"\n[[providers]]\nname = \"a\"\n[[providers]]\nname = \"a\"\n",
        )
        .unwrap();
        assert!(Config::load_from(path).is_err());
    }

    #[test]
    fn global_model_must_reference_registered_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "model = \"nope/m\"\n[[providers]]\nname = \"a\"\n").unwrap();
        let err = Config::load_from(path).unwrap_err().to_string();
        assert!(err.contains("unknown provider"), "{err}");
    }

    #[test]
    fn bare_global_model_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "model = \"bare\"\n[[providers]]\nname = \"bare\"\n").unwrap();
        let err = Config::load_from(path).unwrap_err().to_string();
        assert!(err.contains("expected \"provider/model\""), "{err}");
    }

    #[test]
    fn missing_model_field_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[[providers]]\nname = \"a\"\n").unwrap();
        assert!(Config::load_from(path).is_err());
    }

    #[test]
    fn legacy_provider_section_is_rejected() {
        // 硬断：旧格式（[provider] 节、无顶层 model）解析为缺字段错误。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[provider]\nkind = \"openai\"\napi_key = \"k\"\nmodel = \"m\"\n",
        )
        .unwrap();
        assert!(Config::load_from(path).is_err());
    }

    #[test]
    fn model_ref_parsing() {
        assert_eq!(
            parse_model_ref("deepseek/deepseek-v4-pro").unwrap(),
            ("deepseek", "deepseek-v4-pro")
        );
        // 模型名本身含 / 时按第一个 / 切（provider/model/sub）
        assert_eq!(parse_model_ref("p/a/b").unwrap(), ("p", "a/b"));
        // 裸名无效
        assert!(parse_model_ref("local-model").is_err());
        assert!(parse_model_ref("p/").is_err());
        assert!(parse_model_ref("/m").is_err());
    }

    #[test]
    fn timeout_and_retry_defaults_apply_when_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "model = \"p/m\"\n[[providers]]\nname = \"p\"\napi_key = \"k\"\n",
        )
        .unwrap();
        let cfg = Config::load_from(path).unwrap();
        assert_eq!(cfg.providers[0].provider.connect_timeout_secs, 10);
        assert_eq!(cfg.providers[0].provider.read_timeout_secs, 120);
        assert_eq!(cfg.providers[0].provider.llm_max_retries, 3);
        assert_eq!(cfg.providers[0].provider.llm_retry_base_ms, 1000);
    }
}
