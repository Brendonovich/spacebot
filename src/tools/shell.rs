//! Shell tool for executing shell commands (task workers only).

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::process::Command;

/// Tool for executing shell commands.
#[derive(Debug, Clone)]
pub struct ShellTool;

impl ShellTool {
    /// Create a new shell tool.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Error type for shell tool.
#[derive(Debug, thiserror::Error)]
#[error("Shell command failed: {message}")]
pub struct ShellError {
    message: String,
    exit_code: i32,
}

/// Arguments for shell tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShellArgs {
    /// The shell command to execute.
    pub command: String,
    /// Optional working directory for the command.
    pub working_dir: Option<String>,
    /// Optional timeout in seconds (default: 60).
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_timeout() -> u64 {
    60
}

/// Output from shell tool.
#[derive(Debug, Serialize)]
pub struct ShellOutput {
    /// Whether the command succeeded.
    pub success: bool,
    /// The exit code (0 for success).
    pub exit_code: i32,
    /// Standard output from the command.
    pub stdout: String,
    /// Standard error from the command.
    pub stderr: String,
    /// Formatted summary for LLM consumption.
    pub summary: String,
}

impl Tool for ShellTool {
    const NAME: &'static str = "shell";

    type Error = ShellError;
    type Args = ShellArgs;
    type Output = ShellOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Execute a shell command. Use this for file operations, running scripts, building projects, git commands, and any system-level operations. Be careful with destructive operations. The command runs with a 60 second timeout by default.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute. This will be run with sh -c on Unix or cmd /C on Windows."
                    },
                    "working_dir": {
                        "type": "string",
                        "description": "Optional working directory where the command should run"
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 300,
                        "default": 60,
                        "description": "Maximum time to wait for the command to complete (1-300 seconds)"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // Reject commands that target protected workspace paths
        if let Some(ref dir) = args.working_dir {
            let path = std::path::Path::new(dir);
            if is_protected_working_dir(path) {
                return Err(ShellError {
                    message: format!(
                        "Cannot use protected directory as working_dir: {dir}. \
                         Agent data, archives, and ingestion directories are managed by the system."
                    ),
                    exit_code: -1,
                });
            }
        }

        let mut cmd = if cfg!(target_os = "windows") {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(&args.command);
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c").arg(&args.command);
            c
        };

        if let Some(dir) = args.working_dir {
            cmd.current_dir(dir);
        }

        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Set timeout
        let timeout = tokio::time::Duration::from_secs(args.timeout_seconds);

        let output = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| ShellError {
                message: "Command timed out".to_string(),
                exit_code: -1,
            })?
            .map_err(|e| ShellError {
                message: format!("Failed to execute command: {e}"),
                exit_code: -1,
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);
        let success = output.status.success();

        let summary = format_shell_output(exit_code, &stdout, &stderr);

        Ok(ShellOutput {
            success,
            exit_code,
            stdout,
            stderr,
            summary,
        })
    }
}

/// Format shell output for display.
fn format_shell_output(exit_code: i32, stdout: &str, stderr: &str) -> String {
    let mut output = String::new();

    output.push_str(&format!("Exit code: {}\n", exit_code));

    if !stdout.is_empty() {
        output.push_str("\n--- STDOUT ---\n");
        output.push_str(stdout);
    }

    if !stderr.is_empty() {
        output.push_str("\n--- STDERR ---\n");
        output.push_str(stderr);
    }

    if stdout.is_empty() && stderr.is_empty() {
        output.push_str("\n[No output]\n");
    }

    output
}

/// Legacy shell function for backward compatibility.
pub async fn shell(command: &str, working_dir: Option<&std::path::Path>) -> crate::error::Result<ShellResult> {
    let tool = ShellTool::new();
    let args = ShellArgs {
        command: command.to_string(),
        working_dir: working_dir.map(|p| p.to_string_lossy().to_string()),
        timeout_seconds: 60,
    };

    let output = tool.call(args).await.map_err(|e| crate::error::AgentError::Other(e.into()))?;

    Ok(ShellResult {
        success: output.success,
        exit_code: output.exit_code,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// Result of a shell command execution.
#[derive(Debug, Clone)]
pub struct ShellResult {
    pub success: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl ShellResult {
    /// Format as a readable string for LLM consumption.
    pub fn format(&self) -> String {
        format_shell_output(self.exit_code, &self.stdout, &self.stderr)
    }
}

/// Check if a directory path is in a protected workspace location.
fn is_protected_working_dir(path: &std::path::Path) -> bool {
    let path_str = path.to_string_lossy();
    path_str.contains("/data/lancedb")
        || path_str.contains("/archives/")
        || path_str.contains("/ingest/")
        || path_str.ends_with("/data")
}
