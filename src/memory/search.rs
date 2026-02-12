//! Hybrid search: vector + FTS + RRF + graph traversal.

use crate::error::Result;
use crate::memory::types::{Memory, MemorySearchResult, RelationType};
use crate::memory::{EmbeddingModel, EmbeddingTable, MemoryStore};
use std::collections::HashMap;
use std::sync::Arc;

/// Bundles all memory search dependencies.
pub struct MemorySearch {
    store: Arc<MemoryStore>,
    embedding_table: EmbeddingTable,
    embedding_model: Arc<EmbeddingModel>,
}

impl Clone for MemorySearch {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            embedding_table: self.embedding_table.clone(),
            embedding_model: Arc::clone(&self.embedding_model),
        }
    }
}

impl std::fmt::Debug for MemorySearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySearch")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl MemorySearch {
    /// Create a new MemorySearch instance.
    pub fn new(
        store: Arc<MemoryStore>,
        embedding_table: EmbeddingTable,
        embedding_model: Arc<EmbeddingModel>,
    ) -> Self {
        Self {
            store,
            embedding_table,
            embedding_model,
        }
    }
    
    /// Get a reference to the memory store.
    pub fn store(&self) -> &MemoryStore {
        &self.store
    }
    
    /// Get a reference to the embedding table.
    pub fn embedding_table(&self) -> &EmbeddingTable {
        &self.embedding_table
    }
    
    /// Get a reference to the embedding model.
    pub fn embedding_model(&self) -> &EmbeddingModel {
        &self.embedding_model
    }

    /// Get a shared handle to the embedding model (for async embed_one).
    pub fn embedding_model_arc(&self) -> &Arc<EmbeddingModel> {
        &self.embedding_model
    }
    
    /// Perform hybrid search across all memory sources.
    pub async fn hybrid_search(
        &self,
        query: &str,
        config: &SearchConfig,
    ) -> Result<Vec<MemorySearchResult>> {
        // Collect results from different sources
        let mut vector_results = Vec::new();
        let mut fts_results = Vec::new();
        let mut graph_results = Vec::new();
        
        // 1. Full-text search via LanceDB
        // FTS requires an inverted index. If the index doesn't exist yet (empty
        // table, first run) this will fail — fall back to vector + graph search.
        match self.embedding_table.text_search(query, config.max_results_per_source).await {
            Ok(fts_matches) => {
                for (memory_id, score) in fts_matches {
                    if let Some(memory) = self.store.load(&memory_id).await? {
                        fts_results.push(ScoredMemory { memory, score: score as f64 });
                    }
                }
            }
            Err(error) => {
                tracing::debug!(%error, "FTS search unavailable, falling back to vector + graph");
            }
        }
        
        // 2. Vector similarity search via LanceDB
        let query_embedding = self.embedding_model.embed_one(query).await?;
        match self.embedding_table.vector_search(&query_embedding, config.max_results_per_source).await {
            Ok(vector_matches) => {
                for (memory_id, distance) in vector_matches {
                    let similarity = 1.0 - distance;
                    if let Some(memory) = self.store.load(&memory_id).await? {
                        vector_results.push(ScoredMemory { memory, score: similarity as f64 });
                    }
                }
            }
            Err(error) => {
                tracing::debug!(%error, "vector search unavailable, falling back to graph only");
            }
        }
        
        // 3. Graph traversal from high-importance memories
        // Get identity and high-importance memories as starting points
        let seed_memories = self.store.get_high_importance(0.8, 20).await?;
        
        for seed in seed_memories {
            // Check if seed is semantically related to query via simple keyword matching
            if query.to_lowercase().split_whitespace().any(|term| {
                seed.content.to_lowercase().contains(term)
            }) {
                graph_results.push(ScoredMemory { 
                    memory: seed.clone(), 
                    score: seed.importance as f64
                });
                
                // Traverse graph to find related memories
                self.traverse_graph(&seed.id, config.max_graph_depth, &mut graph_results).await?;
            }
        }
        
        // 4. Merge results using Reciprocal Rank Fusion (RRF)
        let fused_results = reciprocal_rank_fusion(
            &vector_results,
            &fts_results,
            &graph_results,
            config.rrf_k,
        );
        
        // Convert to MemorySearchResult with ranks
        let results: Vec<MemorySearchResult> = fused_results
            .into_iter()
            .enumerate()
            .map(|(rank, scored)| MemorySearchResult {
                memory: scored.memory,
                score: scored.score as f32,
                rank: rank + 1,
            })
            .filter(|r| r.score >= config.min_score)
            .take(config.max_results_per_source)
            .collect();
        
        Ok(results)
    }
    
    /// Traverse the memory graph to find related memories (iterative to avoid async recursion).
    async fn traverse_graph(
        &self,
        start_id: &str,
        max_depth: usize,
        results: &mut Vec<ScoredMemory>,
    ) -> Result<()> {
        use std::collections::VecDeque;
        
        // Queue of (memory_id, current_depth)
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        
        queue.push_back((start_id.to_string(), 0));
        visited.insert(start_id.to_string());
        
        while let Some((current_id, depth)) = queue.pop_front() {
            if depth > max_depth {
                continue;
            }
            
            let associations = self.store.get_associations(&current_id).await?;
            
            for assoc in associations {
                // Get the related memory
                let related_id = if assoc.source_id == current_id {
                    &assoc.target_id
                } else {
                    &assoc.source_id
                };
                
                if visited.contains(related_id) {
                    continue;
                }
                visited.insert(related_id.clone());
                
                if let Some(memory) = self.store.load(related_id).await? {
                    // Score based on relation type and weight
                    let type_multiplier = match assoc.relation_type {
                        RelationType::Updates => 1.5,
                        RelationType::CausedBy | RelationType::ResultOf => 1.3,
                        RelationType::RelatedTo => 1.0,
                        RelationType::Contradicts => 0.5,
                        RelationType::PartOf => 0.8,
                    };
                    
                    let score = memory.importance as f64 * assoc.weight as f64 * type_multiplier;
                    
                    results.push(ScoredMemory { memory: memory.clone(), score });
                    
                    // Add to queue for RelatedTo and PartOf relations
                    if matches!(assoc.relation_type, RelationType::RelatedTo | RelationType::PartOf) {
                        queue.push_back((related_id.clone(), depth + 1));
                    }
                }
            }
        }
        
        Ok(())
    }
}

/// Hybrid search configuration.
#[derive(Debug, Clone)]
pub struct SearchConfig {
    /// Maximum number of results from each source (vector, fts, graph).
    pub max_results_per_source: usize,
    /// RRF k parameter (typically 60).
    pub rrf_k: f64,
    /// Minimum score threshold for results.
    pub min_score: f32,
    /// Maximum graph traversal depth.
    pub max_graph_depth: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            max_results_per_source: 50,
            rrf_k: 60.0,
            // RRF scores are 1/(k+rank), so with k=60 the max single-source
            // score is ~0.016. Set threshold low enough to not discard everything.
            min_score: 0.0,
            max_graph_depth: 2,
        }
    }
}

/// Simple scored memory for internal use.
#[derive(Debug, Clone)]
struct ScoredMemory {
    memory: Memory,
    score: f64,
}

/// Reciprocal Rank Fusion to combine results from multiple sources.
/// RRF score = sum(1 / (k + rank)) for each list where the item appears.
fn reciprocal_rank_fusion(
    vector_results: &[ScoredMemory],
    fts_results: &[ScoredMemory],
    graph_results: &[ScoredMemory],
    k: f64,
) -> Vec<ScoredMemory> {
    // Build a map of memory ID to RRF score
    let mut rrf_scores: HashMap<String, (f64, Memory)> = HashMap::new();
    
    // Add vector results
    for (rank, scored) in vector_results.iter().enumerate() {
        let rrf_score = 1.0 / (k + (rank as f64 + 1.0));
        let entry = rrf_scores.entry(scored.memory.id.clone())
            .or_insert((0.0, scored.memory.clone()));
        entry.0 += rrf_score;
    }
    
    // Add FTS results
    for (rank, scored) in fts_results.iter().enumerate() {
        let rrf_score = 1.0 / (k + (rank as f64 + 1.0));
        let entry = rrf_scores.entry(scored.memory.id.clone())
            .or_insert((0.0, scored.memory.clone()));
        entry.0 += rrf_score;
    }
    
    // Add graph results
    for (rank, scored) in graph_results.iter().enumerate() {
        let rrf_score = 1.0 / (k + (rank as f64 + 1.0));
        let entry = rrf_scores.entry(scored.memory.id.clone())
            .or_insert((0.0, scored.memory.clone()));
        entry.0 += rrf_score;
    }
    
    // Convert to vec and sort by RRF score
    let mut fused: Vec<ScoredMemory> = rrf_scores
        .into_iter()
        .map(|(_, (score, memory))| ScoredMemory { memory, score })
        .collect();
    
    fused.sort_by(|a, b| b.score.total_cmp(&a.score));
    
    fused
}

/// Curate search results to return only the most relevant.
pub fn curate_results(results: &[MemorySearchResult], max_results: usize) -> Vec<&Memory> {
    results
        .iter()
        .take(max_results)
        .map(|r| &r.memory)
        .collect()
}
