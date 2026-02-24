use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Run `cargo check --message-format=json` and parse the output into a concise summary
/// for the LLM to easily consume without blowing out its context window.
pub struct CargoCheckTool {
    security: Arc<SecurityPolicy>,
}

impl CargoCheckTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }
}

#[async_trait]
impl Tool for CargoCheckTool {
    fn name(&self) -> &str {
        "cargo_check"
    }

    fn description(&self) -> &str {
        "Run 'cargo check' on a Rust workspace or package and return a concisely parsed list of compiler errors and warnings. Use this immediately after any file modification in a Rust project to verify syntax and types."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "manifest_path": {
                    "type": "string",
                    "description": "Optional path to the Cargo.toml file. If not provided, runs in the workspace root."
                },
                "package": {
                    "type": "string",
                    "description": "Optional package name to check (e.g., if checking a specific crate in a workspace)."
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: too many actions in the last hour".into()),
            });
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: action budget exhausted".into()),
            });
        }

        let mut cmd = Command::new("cargo");
        cmd.arg("check").arg("--message-format=json");

        if let Some(manifest_path) = args.get("manifest_path").and_then(|v| v.as_str()) {
            if !self.security.is_path_allowed(manifest_path) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Manifest path not allowed by security policy: {}", manifest_path)),
                });
            }
            cmd.arg("--manifest-path").arg(self.security.workspace_dir.join(manifest_path));
        } else {
             cmd.current_dir(&self.security.workspace_dir);
        }

        if let Some(package) = args.get("package").and_then(|v| v.as_str()) {
            cmd.arg("-p").arg(package);
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to spawn cargo check: {}", e)),
                });
            }
        };

        let stdout = child.stdout.take().expect("Failed to open stdout");
        let stderr = child.stderr.take().expect("Failed to open stderr");

        let mut reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();
        
        // Spawn a task to read stderr asynchronously to prevent blocking
        let stderr_handle = tokio::spawn(async move {
             let mut err_output = String::new();
             while let Ok(Some(line)) = stderr_reader.next_line().await {
                 err_output.push_str(&line);
                 err_output.push('\n');
             }
             err_output
        });

        let mut parsed_errors = Vec::new();
        let mut warnings_count = 0;

        while let Ok(Some(line)) = reader.next_line().await {
            if let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) {
                if message.get("reason").and_then(|v| v.as_str()) == Some("compiler-message") {
                    if let Some(msg) = message.get("message") {
                        if let Some(level) = msg.get("level").and_then(|v| v.as_str()) {
                            if level == "error" {
                                let error_text = msg.get("rendered").and_then(|v| v.as_str()).unwrap_or("Unknown error");
                                parsed_errors.push(error_text.to_string());
                            } else if level == "warning" {
                                warnings_count += 1;
                            }
                        }
                    }
                }
            }
        }

        let status = match child.wait().await {
            Ok(s) => s,
            Err(e) => {
                 return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to wait for cargo check: {}", e)),
                });
            }
        };

        let _ = stderr_handle.await; // We don't strictly need stderr unless it failed outside of compiler errors

        if status.success() {
            Ok(ToolResult {
                success: true,
                output: format!("cargo check passed. Warnings: {}", warnings_count),
                error: None,
            })
        } else {
            if parsed_errors.is_empty() {
                 Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("cargo check failed, but no structured 'error' messages were found. It may be a configuration error.".into()),
                })
            } else {
                 let error_summary = parsed_errors.join("\n---\n");
                 Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("cargo check failed with {} error(s):\n\n{}", parsed_errors.len(), error_summary)),
                })
            }
        }
    }
}
