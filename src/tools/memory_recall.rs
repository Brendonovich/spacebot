//! Memory recall tool for branches.

use crate::error::Result;
use crate::memory::MemorySearch;
use crate::memory::search::{SearchConfig, curate_results};
use crate::memory::types::Memory;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Tool for recalling memories using hybrid search.
#[derive(Debug, Clone)]
pub struct MemoryRecallTool {
    memory_search: Arc<MemorySearch>,
}

impl MemoryRecallTool {
    /// Create a new memory recall tool.
    pub fn new(memory_search: Arc<MemorySearch>) -> Self {
        Self { memory_search }
    }
}

/// Error type for memory recall tool.
#[derive(Debug, thiserror::Error)]
#[error("Memory recall failed: {0}")]
pub struct MemoryRecallError(String);

impl From<crate::error::Error> for MemoryRecallError {
    fn from(e: crate::error::Error) -> Self {
        MemoryRecallError(format!("{e}"))
    }
}

/// Arguments for memory recall tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct MemoryRecallArgs {
    /// The search query to find relevant memories.
    pub query: String,
    /// Maximum number of results to return.
    #[serde(default = "default_max_results")]
    pub max_results: usize,
    /// Optional memory type filter (fact, preference, decision, identity, event, observation).
    pub memory_type: Option<String>,
}

fn default_max_results() -> usize {
    10
}

/// Output from memory recall tool.
#[derive(Debug, Serialize)]
pub struct MemoryRecallOutput {
    /// The memories found by the search.
    pub memories: Vec<MemoryOutput>,
    /// Total number of results found before curation.
    pub total_found: usize,
    /// Formatted summary of the memories.
    pub summary: String,
}

/// Simplified memory output for serialization.
#[derive(Debug, Serialize)]
pub struct MemoryOutput {
    /// The memory ID.
    pub id: String,
    /// The memory content.
    pub content: String,
    /// The memory type.
    pub memory_type: String,
    /// The importance score.
    pub importance: f32,
    /// When the memory was created.
    pub created_at: String,
    /// The relevance score from the search.
    pub relevance_score: f32,
}

impl Tool for MemoryRecallTool {
    const NAME: &'static str = "memory_recall";

    type Error = MemoryRecallError;
    type Args = MemoryRecallArgs;
    type Output = MemoryRecallOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Search and recall relevant memories from the memory store. This performs a hybrid search combining vector similarity (semantic meaning), full-text search (exact words), and graph traversal (connected memories) to find the most relevant information.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query. Describe what you're looking for in natural language. The more specific, the better the results."
                    },
                    "max_results": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "default": 10,
                        "description": "Maximum number of memories to return (1-50)"
                    },
                    "memory_type": {
                        "type": "string",
                        "enum": ["fact", "preference", "decision", "identity", "event", "observation"],
                        "description": "Optional filter to only return memories of a specific type"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> std::result::Result<Self::Output, Self::Error> {
        // Perform hybrid search
        let config = SearchConfig {
            max_results_per_source: args.max_results * 2,
            ..Default::default()
        };

        let search_results = self
            .memory_search
            .hybrid_search(&args.query, &config)
            .await
            .map_err(|e| MemoryRecallError(format!("Search failed: {e}")))?;

        // Apply memory_type filter if specified
        let filtered_results: Vec<_> = if let Some(ref type_filter) = args.memory_type {
            search_results
                .into_iter()
                .filter(|r| r.memory.memory_type.to_string() == *type_filter)
                .collect()
        } else {
            search_results
        };

        // Curate results to get the most relevant
        let curated = curate_results(&filtered_results, args.max_results);

        // Record access for found memories and convert to output format
        let store = self.memory_search.store();
        let mut memories = Vec::new();

        for (idx, memory) in curated.iter().enumerate() {
            if let Err(error) = store.record_access(&memory.id).await {
                tracing::warn!(
                    memory_id = %memory.id,
                    %error,
                    "failed to record memory access"
                );
            }

            memories.push(MemoryOutput {
                id: memory.id.clone(),
                content: memory.content.clone(),
                memory_type: memory.memory_type.to_string(),
                importance: memory.importance,
                created_at: memory.created_at.to_rfc3339(),
                relevance_score: filtered_results.get(idx).map(|r| r.score).unwrap_or(0.0),
            });
        }

        let total_found = filtered_results.len();
        let summary = format_memories(&memories);

        Ok(MemoryRecallOutput {
            memories,
            total_found,
            summary,
        })
    }
}

/// Format memories for display to an agent.
pub fn format_memories(memories: &[MemoryOutput]) -> String {
    if memories.is_empty() {
        return "No relevant memories found.".to_string();
    }

    let mut output = String::from("## Relevant Memories\n\n");

    for (i, memory) in memories.iter().enumerate() {
        let preview = memory.content.lines().next().unwrap_or(&memory.content);
        output.push_str(&format!(
            "{}. [{}] (importance: {:.2}, relevance: {:.2})\n   {}\n\n",
            i + 1,
            memory.memory_type,
            memory.importance,
            memory.relevance_score,
            preview
        ));
    }

    output
}

/// Legacy convenience function for direct memory recall.
pub async fn memory_recall(
    memory_search: Arc<MemorySearch>,
    query: &str,
    max_results: usize,
) -> Result<Vec<Memory>> {
    let tool = MemoryRecallTool::new(Arc::clone(&memory_search));
    let args = MemoryRecallArgs {
        query: query.to_string(),
        max_results,
        memory_type: None,
    };

    let output = tool.call(args).await.map_err(|e| crate::error::AgentError::Other(anyhow::anyhow!(e)))?;

    // Convert back to Memory type for backward compatibility
    let store = memory_search.store();
    let mut memories = Vec::new();

    for mem_out in output.memories {
        if let Ok(Some(memory)) = store.load(&mem_out.id).await {
            memories.push(memory);
        }
    }

    Ok(memories)
}
