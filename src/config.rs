use std::env;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Config file at ~/.config/rua/config.toml
#[derive(Debug, Deserialize, Default, Clone)]
pub struct Config {
    #[serde(default)]
    pub deepseek: DeepSeekConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct DeepSeekConfig {
    #[serde(default = "default_api_key")]
    pub api_key: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_model")]
    pub model: String,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct UiConfig {
    #[serde(default = "default_true")]
    pub streaming: bool,
}

fn default_api_key() -> String {
    String::new()
}

fn default_base_url() -> String {
    "https://api.deepseek.com".to_string()
}

fn default_model() -> String {
    "deepseek-v4-pro".to_string()
}

fn default_true() -> bool {
    true
}

/// Resolve a config value using pi-style rules:
/// - If starts with "!", execute the rest as a shell command and use stdout (cached)
/// - Otherwise, try as env var name first, then treat as literal
pub fn resolve_value(value: &str) -> Result<String> {
    if value.starts_with('!') {
        let command = &value[1..];
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .with_context(|| format!("failed to execute shell command: {}", command))?;
        if !output.status.success() {
            anyhow::bail!(
                "shell command failed (exit {}): {}",
                output.status.code().unwrap_or(-1),
                command
            );
        }
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(stdout)
    } else {
        // Try environment variable first, then literal
        Ok(env::var(value).unwrap_or_else(|_| value.to_string()))
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            // Auto-create directory and default config file on first run
            ensure_default_config(&path)?;
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config from {}", path.display()))?;
        let mut config: Config = toml::from_str(&contents)
            .with_context(|| format!("failed to parse config from {}", path.display()))?;

        // Resolve api_key with !command / env var support
        config.deepseek.api_key = resolve_value(&config.deepseek.api_key)
            .with_context(|| "failed to resolve deepseek.api_key")?;

        Ok(config)
    }
}

/// Ensure the config directory and default config file exist.
fn ensure_default_config(path: &std::path::Path) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create config directory: {}", dir.display()))?;

    let default_contents = r#"# rua configuration file

[deepseek]
# API key supports three formats:
# 1. Literal (not recommended): api_key = "sk-xxxx"
# 2. Env var name: api_key = "DEEPSEEK_API_KEY"
# 3. Shell command (pi-style ! prefix):
#    api_key = "!echo $DEEPSEEK_API_KEY"
#    api_key = "!security find-generic-password -s deepseek-api-key -w"
api_key = "!echo $DEEPSEEK_API_KEY"

base_url = "https://api.deepseek.com"
model = "deepseek-v4-pro"

[ui]
streaming = true
"#;

    std::fs::write(path, default_contents)
        .with_context(|| format!("failed to write default config to {}", path.display()))?;

    Ok(())
}

pub fn config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("rua")
        .join("config.toml")
}
