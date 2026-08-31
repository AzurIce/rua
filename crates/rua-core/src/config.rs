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
    #[serde(default)]
    pub server: ServerConfig,
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
/// - otherwise: try as an environment variable name, then treat as literal
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
            server: raw.server.unwrap_or_default(),
        };
        config.provider.api_key = resolve_value(&config.provider.api_key)
            .map_err(|e| Error::Config(format!("failed to resolve provider.api_key: {e}")))?;
        Ok(config)
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
# API key supports three formats (LM Studio accepts any non-empty string):
# 1. Literal: api_key = "lm-studio"
# 2. Env var name: api_key = "DEEPSEEK_API_KEY"
# 3. Shell command (pi-style ! prefix):
#    api_key = "!echo $DEEPSEEK_API_KEY"
#    api_key = "!security find-generic-password -s deepseek-api-key -w"
api_key = "lm-studio"
base_url = "http://127.0.0.1:1234/v1"
model = "qwen3.8-27b-uncensored-mlx"
# Provider-specific params passed through verbatim, e.g.:
# [provider.additional_params]
# thinking = { type = "enabled" }

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
    fn resolves_shell_command_with_cache() {
        let v = resolve_value("!echo hello-cache").unwrap();
        assert_eq!(v, "hello-cache");
        // second call hits the cache (same value, no re-execution)
        assert_eq!(resolve_value("!echo hello-cache").unwrap(), "hello-cache");
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
