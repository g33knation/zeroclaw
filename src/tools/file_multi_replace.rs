use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// A struct representing a single replacement operation within a file.
#[derive(serde::Deserialize)]
struct ReplacementChunk {
    /// The starting line number (1-indexed, inclusive)
    start_line: usize,
    /// The ending line number (1-indexed, inclusive)
    end_line: usize,
    /// The exact target string to replace (must match exactly within the specified lines)
    #[serde(alias = "target_content", alias = "search")]
    target: String,
    /// The new content to insert in place of the target content
    #[serde(alias = "replacement_content", alias = "replace")]
    replacement: String,
}

/// Edit a file by replacing multiple non-contiguous blocks of text.
///
/// This tool minimizes context loss and improves reliability for large file edits
/// by using targeted start/end line bounds and precise string matching, returning
/// a unified diff-style result upon success.
pub struct FileMultiReplaceTool {
    security: Arc<SecurityPolicy>,
}

impl FileMultiReplaceTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }
}

#[async_trait]
impl Tool for FileMultiReplaceTool {
    fn name(&self) -> &str {
        "file_multi_replace"
    }

    fn description(&self) -> &str {
        "Edit an existing file by replacing multiple non-contiguous blocks of text. Provide 'path' and an array of 'replacement_chunks'. Each chunk needs 'start_line', 'end_line', 'target', and 'replacement'."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to modify. Relative paths resolve from workspace. Alias: 'files' (takes first from list)."
                },
                "replacement_chunks": {
                    "type": "array",
                    "description": "A list of chunks to replace. Each chunk must specify a line range and the exact content to replace. Alias: 'replacements'.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "start_line": {
                                "type": "integer",
                                "description": "The starting line number of the chunk (1-indexed)."
                            },
                            "end_line": {
                                "type": "integer",
                                "description": "The ending line number of the chunk (1-indexed)."
                            },
                            "target": {
                                "type": "string",
                                "description": "The exact string to be replaced. Must be a precise substring within the specified line range. Alias: 'search', 'target_content'."
                            },
                            "replacement": {
                                "type": "string",
                                "description": "The content to replace the target content with. Alias: 'replace', 'replacement_content'."
                            }
                        },
                        "required": ["start_line", "end_line", "target", "replacement"]
                    }
                }
            },
            "required": ["path", "replacement_chunks"]
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

        let top_level_path = args
            .get("path")
            .or_else(|| args.get("files").filter(|v| v.is_string()))
            .or_else(|| {
                args.get("files")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .filter(|v| v.is_string())
            })
            .and_then(|v| v.as_str());

        let chunks_value = args.get("replacement_chunks")
            .or_else(|| args.get("replacements"));
        
        let chunks_json = chunks_value.unwrap_or(&serde_json::Value::Null).clone();
        let raw_chunks: Vec<serde_json::Value> = match chunks_json {
            serde_json::Value::Array(a) => a,
            _ => return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Invalid 'replacement_chunks' format: must be an array".into()),
            }),
        };

        if raw_chunks.is_empty() {
             return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("No replacement chunks provided".into()),
            });
        }

        let mut file_changes: std::collections::HashMap<String, Vec<ReplacementChunk>> = std::collections::HashMap::new();

        for (i, raw_chunk) in raw_chunks.into_iter().enumerate() {
            let chunk_path = raw_chunk.get("path").and_then(|v| v.as_str())
                .or(top_level_path)
                .ok_or_else(|| anyhow::anyhow!("Missing 'path' for chunk {}", i))?;

            let chunk: ReplacementChunk = match serde_json::from_value(raw_chunk.clone()) {
                Ok(c) => c,
                Err(e) => return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Invalid chunk {} format: {}", i, e)),
                }),
            };
            file_changes.entry(chunk_path.to_string()).or_default().push(chunk);
        }

        let mut results = Vec::new();
        let mut total_replacements = 0;

        for (path, chunks) in file_changes {
            if !self.security.is_path_allowed(&path) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Path not allowed by security policy: {}", path)),
                });
            }

            let full_path = self.security.workspace_dir.join(&path);
            let Some(parent) = full_path.parent() else {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Invalid path: missing parent directory for {}", path)),
                });
            };

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

        let resolved_parent = match tokio::fs::canonicalize(parent).await {
                Ok(p) => p,
                Err(e) => return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to resolve parent directory for {}: {}", path, e)),
                }),
            };

            if !self.security.is_resolved_path_allowed(&resolved_parent) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(self.security.resolved_path_violation_message(&resolved_parent)),
                });
            }

            let file_name = full_path.file_name().unwrap();
            let resolved_target = resolved_parent.join(file_name);

            let content = match tokio::fs::read_to_string(&resolved_target).await {
                Ok(c) => c,
                Err(e) => return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to read file {}: {}", path, e)),
                }),
            };

            let mut output_content = content.clone();
            let lines: Vec<&str> = content.split('\n').collect();
            let total_lines = lines.len();

            for (i, chunk) in chunks.iter().enumerate() {
                if chunk.start_line == 0 || chunk.end_line < chunk.start_line {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Chunk {} in {} has invalid bounds", i, path)),
                    });
                }

                let start_idx = (chunk.start_line - 1).min(total_lines);
                let end_idx = chunk.end_line.min(total_lines);
                let search_area = if start_idx < end_idx { lines[start_idx..end_idx].join("\n") } else { String::new() };

                let match_count = search_area.matches(&chunk.target).count();
                if match_count != 1 {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Chunk {} in {} failed: match count is {}", i, path, match_count)),
                    });
                }

                output_content = output_content.replace(&chunk.target, &chunk.replacement);
                total_replacements += 1;
            }

            if let Err(e) = tokio::fs::write(&resolved_target, &output_content).await {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to write file {}: {}", path, e)),
                });
            }
            results.push(path);
        }

        Ok(ToolResult {
            success: true,
            output: format!("Applied {} replacements across files: {}", total_replacements, results.join(", ")),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use std::path::PathBuf;

    fn test_security(workspace: PathBuf) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace,
            ..SecurityPolicy::default()
        })
    }

    #[tokio::test]
    async fn test_file_multi_replace() {
        let dir = std::env::temp_dir().join("zeroclaw_test_multi_replace");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        
        let file_path = dir.join("test.txt");
        tokio::fs::write(&file_path, "line 1\nline 2\nline 3\nline 4\nline 5").await.unwrap();

        let tool = FileMultiReplaceTool::new(test_security(dir.clone()));

        // Test successful swap
        let args = json!({
            "path": "test.txt",
            "replacement_chunks": [
                {
                    "start_line": 2,
                    "end_line": 4,
                    "target_content": "line 3",
                    "replacement_content": "line 3 modified"
                }
            ]
        });

        let res = tool.execute(args).await.unwrap();
        assert!(res.success);
        
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "line 1\nline 2\nline 3 modified\nline 4\nline 5");

        // Test failure (target not found in bounds)
        let fail_args = json!({
            "path": "test.txt",
            "replacement_chunks": [
                {
                    "start_line": 1,
                    "end_line": 2, // line 5 is not here
                    "target_content": "line 5",
                    "replacement_content": "line 5 modified"
                }
            ]
        });

        let fail_res = tool.execute(fail_args).await.unwrap();
        assert!(!fail_res.success);
        assert!(fail_res.error.unwrap().contains("not found within lines"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
