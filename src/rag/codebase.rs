use std::path::{Path, PathBuf};
use crate::memory::{chunker, Memory, MemoryCategory};
use tracing::{info, warn};

use std::sync::Arc;

pub struct CodebaseRag {
    memory: Arc<dyn Memory>,
    workspace_dir: PathBuf,
}

impl CodebaseRag {
    pub fn new(memory: Arc<dyn Memory>, workspace_dir: PathBuf) -> Self {
        Self {
            memory,
            workspace_dir,
        }
    }

    /// Index the codebase. This is a heavy operation.
    pub async fn index_all(&self) -> anyhow::Result<()> {
        info!("Indexing codebase at {:?}", self.workspace_dir);
        
        // Simple file walker for now
        let mut files = Vec::new();
        self.collect_files(&self.workspace_dir, &mut files);

        for file in files {
            match std::fs::read_to_string(&file) {
                Ok(content) => {
                    let rel_path = file.strip_prefix(&self.workspace_dir).unwrap_or(&file);
                    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
                    
                    // Simple chunking for now, we can improve this later
                    let chunks = if ext == "md" {
                        chunker::chunk_markdown(&content, 512)
                    } else {
                        // Generic chunker for source code
                        self.chunk_source_code(&content, 512)
                    };

                    for chunk in chunks {
                        let key = format!("{}:{}", rel_path.display(), chunk.index);
                        let category = MemoryCategory::Custom("codebase".to_string());
                        
                        // Store in memory (Sqlite/Vector)
                        self.memory.store(&key, &chunk.content, category, None).await?;
                    }
                }
                Err(e) => warn!("Failed to read file {:?}: {}", file, e),
            }
        }

        Ok(())
    }

    fn collect_files(&self, dir: &Path, files: &mut Vec<PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if name == "target" || name == ".git" || name == ".gemini" || name == "node_modules" {
                        continue;
                    }
                    self.collect_files(&path, files);
                } else {
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if matches!(ext, "rs" | "cpp" | "h" | "sql" | "md" | "toml") {
                        files.push(path);
                    }
                }
            }
        }
    }

    fn chunk_source_code(&self, content: &str, _max_tokens: usize) -> Vec<chunker::Chunk> {
        // For now, let's just use a simple line-based chunker that tries to be somewhat sane.
        // We can improve this with tree-sitter or regex for better semantic splitting.
        let mut chunks = Vec::new();
        let lines: Vec<&str> = content.lines().collect();
        let chunk_size = 50; // lines
        
        for (i, lines_chunk) in lines.chunks(chunk_size).enumerate() {
            chunks.push(chunker::Chunk {
                index: i,
                content: lines_chunk.join("\n"),
                heading: None,
            });
        }
        
        chunks
    }

    pub async fn retrieve(&self, query: &str, limit: usize) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        self.memory.recall(query, limit, None).await
    }
}
