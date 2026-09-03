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
#[derive(Debug, Deserialize, Default, Clone)]
pub struct Config {
    #[serde(default)]
    pub provider: ProviderConfig,
    /// 额外注册的具名 provider（`[[providers]]`）。默认 provider 即上面的
    /// `[provider]` 节，名为 `"default"`。
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

/// 默认 provider 的名字（model ref 中裸模型名解析到它）。
pub const DEFAULT_PROVIDER: &str = "default";

/// 模型引用：`"provider名/模型名"`；裸名（无 `/`）指默认 provider。
/// 返回 (provider 名, 模型名)。
pub fn parse_model_ref(model_ref: &str) -> (&str, &str) {
    match model_ref.split_once('/') {
        Some((p, m)) if !p.is_empty() && !m.is_empty() => (p, m),
        _ => (DEFAULT_PROVIDER, model_ref),
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
    #[serde(default = "default_model")]
    pub model: String,
    /// Provider-specific request params, passed through verbatim
    /// (e.g. thinking toggles).
    #[serde(default)]
    pub additional_params: serde_json::Map<String, serde_json::Value>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: default_kind(),
            api_key: String::new(),
            base_url: default_base_url(),
            model: default_model(),
            additional_params: serde_json::Map::new(),
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

fn default_model() -> String {
    "qwen3.8-27b-uncensored-mlx".to_string()
}

fn default_port() -> u16 {
    3080
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

        // Legacy configs use `[deepseek]`; accept it as a fallback when
        // `[provider]` is absent.
        #[derive(Deserialize)]
        struct RawConfig {
            provider: Option<ProviderConfig>,
            deepseek: Option<ProviderConfig>,
            providers: Option<Vec<NamedProviderConfig>>,
            server: Option<ServerConfig>,
        }
        let raw: RawConfig = toml::from_str(&contents)
            .map_err(|e| Error::Config(format!("failed to parse {}: {e}", path.display())))?;
        let provider = match (raw.provider, raw.deepseek) {
            (Some(p), _) => p,
            // The legacy section never carried `kind`; it means DeepSeek.
            (None, Some(mut d)) => {
                d.kind = "deepseek".to_string();
                d
            }
            (None, None) => ProviderConfig::default(),
        };
        let mut config = Config {
            provider,
            providers: raw.providers.unwrap_or_default(),
            server: raw.server.unwrap_or_default(),
        };
        // 具名 provider 校验：名字非空、不得占用 "default"、不得重复。
        let mut seen = std::collections::HashSet::new();
        for p in &config.providers {
            if p.name.is_empty() || p.name == DEFAULT_PROVIDER || p.name.contains('/') {
                return Err(Error::Config(format!(
                    "invalid provider name {:?} (empty, \"{DEFAULT_PROVIDER}\" and \"/\" are not allowed)",
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
        config.provider.api_key = resolve_value(&config.provider.api_key)
            .map_err(|e| Error::Config(format!("failed to resolve provider.api_key: {e}")))?;
        for p in &mut config.providers {
            p.provider.api_key = resolve_value(&p.provider.api_key).map_err(|e| {
                Error::Config(format!("failed to resolve api_key of provider {:?}: {e}", p.name))
            })?;
        }
        Ok(config)
    }

    /// 全部 provider：默认 provider 在前（名 `"default"`），后跟具名列表。
    pub fn all_providers(&self) -> Vec<(String, ProviderConfig)> {
        let mut out = vec![(DEFAULT_PROVIDER.to_string(), self.provider.clone())];
        out.extend(
            self.providers
                .iter()
                .map(|p| (p.name.clone(), p.provider.clone())),
        );
        out
    }

    pub fn resolved_api_key(&self) -> Result<String> {
        resolve_value(&self.provider.api_key)
    }
}

fn ensure_default_config(path: &std::path::Path) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;
    let default_contents = r#"# rua configuration file

[provider]
# Provider kind: "openai" = any OpenAI-compatible endpoint (default: LM
# Studio local server); "deepseek" = DeepSeek API.
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
model = "qwen3.8-27b-uncensored-mlx"
# Provider-specific params passed through verbatim, e.g.:
# [provider.additional_params]
# thinking = { type = "enabled" }

# Extra named providers (a model ref is then "name/model"; bare model names
# resolve to the default [provider] above):
# [[providers]]
# name = "deepseek"
# kind = "deepseek"
# api_key = "$DEEPSEEK_API_KEY"
# base_url = "https://api.deepseek.com"
# model = "deepseek-v4-pro"

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
    fn named_providers_are_parsed_and_resolved() {
        unsafe { std::env::set_var("RUA_TEST_DS_KEY", "sk-ds-env") };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[provider]
kind = "openai"
api_key = "lm-studio"
base_url = "http://127.0.0.1:8000/v1"
model = "local-model"

[[providers]]
name = "deepseek"
kind = "deepseek"
api_key = "$RUA_TEST_DS_KEY"
base_url = "https://api.deepseek.com"
model = "deepseek-v4-pro"

[[providers]]
name = "backup"
kind = "openai"
api_key = "sk-literal"
base_url = "http://127.0.0.1:9999/v1"
model = "small-model"
"#,
        )
        .unwrap();
        let cfg = Config::load_from(path).unwrap();
        let all = cfg.all_providers();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, DEFAULT_PROVIDER);
        assert_eq!(all[0].1.model, "local-model");
        assert_eq!(all[1].0, "deepseek");
        assert_eq!(all[1].1.api_key, "sk-ds-env");
        assert_eq!(all[1].1.kind, "deepseek");
        assert_eq!(all[2].0, "backup");
        assert_eq!(all[2].1.api_key, "sk-literal");
    }

    #[test]
    fn duplicate_provider_name_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[[providers]]\nname = \"a\"\n[[providers]]\nname = \"a\"\n").unwrap();
        assert!(Config::load_from(path).is_err());
    }

    #[test]
    fn reserved_provider_name_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[[providers]]\nname = \"default\"\n").unwrap();
        assert!(Config::load_from(path).is_err());
    }

    #[test]
    fn model_ref_parsing() {
        assert_eq!(
            parse_model_ref("deepseek/deepseek-v4-pro"),
            ("deepseek", "deepseek-v4-pro")
        );
        assert_eq!(parse_model_ref("local-model"), (DEFAULT_PROVIDER, "local-model"));
        // 模型名本身含 / 时按第一个 / 切（provider/model/sub）
        assert_eq!(parse_model_ref("p/a/b"), ("p", "a/b"));
    }

    #[test]
    fn legacy_deepseek_section_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[deepseek]
api_key = "sk-legacy"
base_url = "https://example.com"
model = "legacy-model"
"#,
        )
        .unwrap();
        let cfg = Config::load_from(path).unwrap();
        assert_eq!(cfg.provider.kind, "deepseek");
        assert_eq!(cfg.provider.api_key, "sk-legacy");
        assert_eq!(cfg.provider.base_url, "https://example.com");
        assert_eq!(cfg.provider.model, "legacy-model");
        assert_eq!(cfg.server.port, 3080);
    }
}
