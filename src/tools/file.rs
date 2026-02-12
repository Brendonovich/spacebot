//! File tool for reading/writing/listing files (task workers only).

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Tool for file operations.
#[derive(Debug, Clone)]
pub struct FileTool;

impl FileTool {
    /// Create a new file tool.
    pub fn new() -> Self {
        Self
    }
}

impl Default for FileTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Error type for file tool.
#[derive(Debug, thiserror::Error)]
#[error("File operation failed: {0}")]
pub struct FileError(String);

/// Arguments for file tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileArgs {
    /// The operation to perform.
    pub operation: String,
    /// The file or directory path.
    pub path: String,
    /// Content to write (required for write operation).
    pub content: Option<String>,
    /// Whether to create parent directories if they don't exist (for write operations).
    #[serde(default = "default_create_dirs")]
    pub create_dirs: bool,
}

fn default_create_dirs() -> bool {
    true
}

/// Output from file tool.
#[derive(Debug, Serialize)]
pub struct FileOutput {
    /// Whether the operation succeeded.
    pub success: bool,
    /// The operation performed.
    pub operation: String,
    /// The file/directory path.
    pub path: String,
    /// File content (for read operations).
    pub content: Option<String>,
    /// Directory entries (for list operations).
    pub entries: Option<Vec<FileEntryOutput>>,
    /// Error message if operation failed.
    pub error: Option<String>,
}

/// File entry for serialization.
#[derive(Debug, Serialize)]
pub struct FileEntryOutput {
    /// Entry name.
    pub name: String,
    /// Entry type (file, directory, or other).
    pub entry_type: String,
    /// File size in bytes (0 for directories).
    pub size: u64,
}

impl Tool for FileTool {
    const NAME: &'static str = "file";

    type Error = FileError;
    type Args = FileArgs;
    type Output = FileOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Perform file operations: read, write, or list files. Use this to examine code, read documentation, write files, or explore directory structures. Protected paths (prompts/, identity/, data/, SOUL.md, IDENTITY.md, USER.md) cannot be accessed - use memory_save instead for that content.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "operation": {
                        "type": "string",
                        "enum": ["read", "write", "list"],
                        "description": "The file operation to perform"
                    },
                    "path": {
                        "type": "string",
                        "description": "The file or directory path. Relative paths are resolved from the workspace root."
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file (required for write operation)"
                    },
                    "create_dirs": {
                        "type": "boolean",
                        "default": true,
                        "description": "For write operations: create parent directories if they don't exist"
                    }
                },
                "required": ["operation", "path"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = Path::new(&args.path);

        // Check protected paths
        if is_protected_path(path) {
            return Err(FileError(format!(
                "Cannot access protected path: {}. Use memory_save tool for identity/memory data.",
                args.path
            )));
        }

        match args.operation.as_str() {
            "read" => do_file_read(path).await,
            "write" => {
                let content = args.content.ok_or_else(|| {
                    FileError("Content is required for write operation".to_string())
                })?;
                do_file_write(path, content, args.create_dirs).await
            }
            "list" => do_file_list(path).await,
            _ => Err(FileError(format!("Unknown operation: {}", args.operation))),
        }
    }
}

async fn do_file_read(path: &Path) -> Result<FileOutput, FileError> {
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| FileError(format!("Failed to read file: {e}")))?;

    Ok(FileOutput {
        success: true,
        operation: "read".to_string(),
        path: path.to_string_lossy().to_string(),
        content: Some(content),
        entries: None,
        error: None,
    })
}

async fn do_file_write(
    path: &Path,
    content: String,
    create_dirs: bool,
) -> Result<FileOutput, FileError> {
    // Ensure parent directory exists if requested
    if create_dirs {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| FileError(format!("Failed to create directory: {e}")))?;
        }
    }

    tokio::fs::write(path, content)
        .await
        .map_err(|e| FileError(format!("Failed to write file: {e}")))?;

    Ok(FileOutput {
        success: true,
        operation: "write".to_string(),
        path: path.to_string_lossy().to_string(),
        content: None,
        entries: None,
        error: None,
    })
}

async fn do_file_list(path: &Path) -> Result<FileOutput, FileError> {
    let mut entries = Vec::new();

    let mut reader = tokio::fs::read_dir(path)
        .await
        .map_err(|e| FileError(format!("Failed to read directory: {e}")))?;

    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|e| FileError(format!("Failed to read entry: {e}")))?
    {
        let metadata = entry
            .metadata()
            .await
            .map_err(|e| FileError(format!("Failed to read metadata: {e}")))?;

        let entry_type = if metadata.is_file() {
            "file".to_string()
        } else if metadata.is_dir() {
            "directory".to_string()
        } else {
            "other".to_string()
        };

        entries.push(FileEntryOutput {
            name: entry.file_name().to_string_lossy().to_string(),
            entry_type,
            size: metadata.len(),
        });
    }

    // Sort entries: directories first, then files, both alphabetically
    entries.sort_by(|a, b| {
        let a_is_dir = a.entry_type == "directory";
        let b_is_dir = b.entry_type == "directory";
        match (a_is_dir, b_is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        }
    });

    Ok(FileOutput {
        success: true,
        operation: "list".to_string(),
        path: path.to_string_lossy().to_string(),
        content: None,
        entries: Some(entries),
        error: None,
    })
}

/// Check if a path is in a protected workspace location.
///
/// Workers should not write to identity files, agent databases, compaction
/// archives, or the ingestion directory. These are managed by other parts
/// of the system (identity loader, memory store, compactor, ingestion pipeline).
fn is_protected_path(path: &Path) -> bool {
    let path_str = path.to_string_lossy();

    // Identity and prompt files
    path_str.ends_with("SOUL.md")
        || path_str.ends_with("IDENTITY.md")
        || path_str.ends_with("USER.md")
        || path_str.contains("/prompts/")
        // Agent data directories (databases, embeddings, config)
        || path_str.contains("/data/spacebot.db")
        || path_str.contains("/data/lancedb/")
        || path_str.contains("/data/config.redb")
        // Compaction archives and ingestion pipeline
        || path_str.contains("/archives/")
        || path_str.contains("/ingest/")
}

/// File entry metadata (legacy).
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub file_type: FileType,
    pub size: u64,
}

/// File type classification (legacy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Directory,
    Other,
}

/// Legacy convenience functions for backward compatibility.

pub async fn file_read(path: impl AsRef<Path>) -> crate::error::Result<String> {
    let tool = FileTool::new();
    let args = FileArgs {
        operation: "read".to_string(),
        path: path.as_ref().to_string_lossy().to_string(),
        content: None,
        create_dirs: false,
    };

    let output = tool
        .call(args)
        .await
        .map_err(|e| crate::error::AgentError::Other(e.into()))?;

    output
        .content
        .ok_or_else(|| crate::error::AgentError::Other(anyhow::anyhow!("No content in file read result").into()))
        .map_err(|e| e.into())
}

pub async fn file_write(
    path: impl AsRef<Path>,
    content: impl AsRef<[u8]>,
) -> crate::error::Result<()> {
    let tool = FileTool::new();
    let args = FileArgs {
        operation: "write".to_string(),
        path: path.as_ref().to_string_lossy().to_string(),
        content: Some(String::from_utf8_lossy(content.as_ref()).to_string()),
        create_dirs: true,
    };

    tool
        .call(args)
        .await
        .map_err(|e| crate::error::AgentError::Other(e.into()))?;

    Ok(())
}

pub async fn file_list(path: impl AsRef<Path>) -> crate::error::Result<Vec<FileEntry>> {
    let tool = FileTool::new();
    let args = FileArgs {
        operation: "list".to_string(),
        path: path.as_ref().to_string_lossy().to_string(),
        content: None,
        create_dirs: false,
    };

    let output = tool
        .call(args)
        .await
        .map_err(|e| crate::error::AgentError::Other(e.into()))?;

    let entries = output.entries.ok_or_else(|| {
        crate::error::AgentError::Other(anyhow::anyhow!("No entries in list result"))
    })?;

    Ok(entries
        .into_iter()
        .map(|e| FileEntry {
            name: e.name,
            file_type: match e.entry_type.as_str() {
                "file" => FileType::File,
                "directory" => FileType::Directory,
                _ => FileType::Other,
            },
            size: e.size,
        })
        .collect())
}

use anyhow::Context as _;
