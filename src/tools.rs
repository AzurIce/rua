use color_eyre::Result;
use serde_json::json;

/// All available tools.
#[derive(Debug, Clone)]
pub enum Tool {
    Bash(BashTool),
}

impl Tool {
    pub fn name(&self) -> &str {
        match self {
            Tool::Bash(_) => "bash",
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Tool::Bash(_) => {
                "Execute a shell command in the working directory. \
                 Use this for file operations, running tests, git commands, etc."
            }
        }
    }

    pub fn parameters_schema(&self) -> serde_json::Value {
        match self {
            Tool::Bash(_) => {
                json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to execute"
                        }
                    },
                    "required": ["command"]
                })
            }
        }
    }

    pub async fn execute(&self, params: serde_json::Value) -> Result<String> {
        match self {
            Tool::Bash(t) => t.execute(params).await,
        }
    }
}

/// Registry of available tools, serializable to the LLM API format.
#[derive(Debug, Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Tool>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn add(&mut self, tool: Tool) {
        self.tools.push(tool);
    }

    /// Convert to the API `tools` field format.
    pub fn to_api_schema(&self) -> Vec<serde_json::Value> {
        self.tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name(),
                        "description": t.description(),
                        "parameters": t.parameters_schema()
                    }
                })
            })
            .collect()
    }

    /// Execute a tool by name.
    pub async fn execute(
        &self,
        name: &str,
        params: serde_json::Value,
    ) -> Result<String> {
        let tool = self
            .tools
            .iter()
            .find(|t| t.name() == name)
            .ok_or_else(|| color_eyre::eyre::eyre!("Unknown tool: {}", name))?;
        tool.execute(params).await
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

// ===================================================================
// Bash tool
// ===================================================================

#[derive(Debug, Clone)]
pub struct BashTool;

impl BashTool {
    pub async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let command = params["command"]
            .as_str()
            .ok_or_else(|| color_eyre::eyre::eyre!("Missing 'command' parameter"))?;

        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .await?;

        let mut result = String::new();

        if !output.stdout.is_empty() {
            result.push_str(&String::from_utf8_lossy(&output.stdout));
        }

        if !output.stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str("stderr: ");
            result.push_str(&String::from_utf8_lossy(&output.stderr));
        }

        if !output.status.success() {
            result.push_str(&format!(
                "{}exit code: {}",
                if result.is_empty() { "" } else { "\n" },
                output.status.code().unwrap_or(-1)
            ));
        }

        Ok(result.trim().to_string())
    }
}
