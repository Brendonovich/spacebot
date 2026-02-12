//! Compactor: Programmatic context monitor that triggers background compaction.
//!
//! The compactor is NOT an LLM process. It watches a channel's context size and
//! spawns compaction workers when thresholds are crossed. The LLM work (summarization
//! + memory extraction) happens in the spawned worker, not here.

use crate::config::CompactionConfig;
use crate::conversation::ConversationLogger;
use crate::error::Result;
use crate::llm::SpacebotModel;
use crate::memory::MemorySearch;
use crate::{AgentDeps, ChannelId, ProcessType};
use rig::agent::AgentBuilder;
use rig::completion::{CompletionModel as _, Prompt as _};
use rig::message::{AssistantContent, Message, UserContent};
use rig::tool::server::{ToolServer, ToolServerHandle};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Programmatic monitor that watches channel context size and triggers compaction.
pub struct Compactor {
    pub channel_id: ChannelId,
    pub config: CompactionConfig,
    pub context_window: usize,
    pub deps: AgentDeps,
    pub history: Arc<RwLock<Vec<Message>>>,
    pub logger: ConversationLogger,
    pub compactor_prompt: String,
    /// Is a compaction currently running.
    is_compacting: Arc<RwLock<bool>>,
}

impl Compactor {
    /// Create a new compactor for a channel.
    pub fn new(
        channel_id: ChannelId,
        config: CompactionConfig,
        context_window: usize,
        deps: AgentDeps,
        history: Arc<RwLock<Vec<Message>>>,
        logger: ConversationLogger,
        compactor_prompt: String,
    ) -> Self {
        Self {
            channel_id,
            config,
            context_window,
            deps,
            history,
            logger,
            compactor_prompt,
            is_compacting: Arc::new(RwLock::new(false)),
        }
    }

    /// Check context size and trigger compaction if needed.
    ///
    /// Called by the channel after each turn. Returns the action taken, if any.
    pub async fn check_and_compact(&self) -> Result<Option<CompactionAction>> {
        let is_compacting = *self.is_compacting.read().await;
        if is_compacting {
            return Ok(None);
        }

        let usage = {
            let history = self.history.read().await;
            let estimated_tokens = estimate_history_tokens(&history);
            estimated_tokens as f32 / self.context_window as f32
        };

        let action = if usage >= self.config.emergency_threshold {
            Some(CompactionAction::EmergencyTruncate)
        } else if usage >= self.config.aggressive_threshold {
            Some(CompactionAction::Aggressive)
        } else if usage >= self.config.background_threshold {
            Some(CompactionAction::Background)
        } else {
            None
        };

        if let Some(action) = action {
            tracing::info!(
                channel_id = %self.channel_id,
                usage = %format!("{:.1}%", usage * 100.0),
                ?action,
                "compaction triggered"
            );

            match action {
                CompactionAction::EmergencyTruncate => {
                    // Emergency is synchronous — fast, no LLM
                    self.emergency_truncate().await?;
                }
                CompactionAction::Background | CompactionAction::Aggressive => {
                    // Background/aggressive spawn a worker
                    self.spawn_compaction_worker(action).await;
                }
            }

            Ok(Some(action))
        } else {
            Ok(None)
        }
    }

    /// Spawn a compaction worker in the background.
    ///
    /// The worker reads old messages, archives them, runs an LLM to produce a
    /// summary + extract memories, then swaps the summary into the channel's history.
    async fn spawn_compaction_worker(&self, action: CompactionAction) {
        let mut is_compacting = self.is_compacting.write().await;
        *is_compacting = true;
        drop(is_compacting);

        let fraction = match action {
            CompactionAction::Background => 0.3,
            CompactionAction::Aggressive => 0.5,
            CompactionAction::EmergencyTruncate => unreachable!(),
        };

        let history = self.history.clone();
        let is_compacting = self.is_compacting.clone();
        let channel_id = self.channel_id.clone();
        let deps = self.deps.clone();
        let logger = self.logger.clone();
        let compactor_prompt = self.compactor_prompt.clone();

        tokio::spawn(async move {
            let result = run_compaction(
                &channel_id,
                &deps,
                &logger,
                &compactor_prompt,
                &history,
                fraction,
            ).await;

            match result {
                Ok(turns_compacted) => {
                    tracing::info!(
                        channel_id = %channel_id,
                        turns_compacted,
                        "compaction completed"
                    );
                }
                Err(error) => {
                    tracing::error!(
                        channel_id = %channel_id,
                        %error,
                        "compaction failed"
                    );
                }
            }

            let mut flag = is_compacting.write().await;
            *flag = false;
        });
    }

    /// Emergency truncation: drop oldest messages without LLM summarization.
    ///
    /// Only fires at 95%+ context usage. Removes the oldest half of messages and
    /// inserts a marker. Fast and synchronous.
    async fn emergency_truncate(&self) -> Result<()> {
        let mut history = self.history.write().await;
        let total = history.len();
        if total <= 2 {
            return Ok(());
        }

        let remove_count = total / 2;

        // Archive the removed messages before dropping them
        let removed: Vec<Message> = history.drain(..remove_count).collect();
        if let Ok(json) = serde_json::to_string(&removed) {
            self.logger.archive_transcript(&self.channel_id, &json);
        }

        // Insert a marker at the beginning
        let marker = format!(
            "[System: {remove_count} older messages were truncated due to context limits. \
             Some conversation history has been lost.]"
        );
        history.insert(0, Message::from(marker));

        tracing::warn!(
            channel_id = %self.channel_id,
            removed = remove_count,
            remaining = history.len(),
            "emergency truncation performed"
        );

        Ok(())
    }
}

/// Run the actual compaction: archive, summarize via LLM, extract memories, swap.
async fn run_compaction(
    channel_id: &ChannelId,
    deps: &AgentDeps,
    logger: &ConversationLogger,
    compactor_prompt: &str,
    history: &Arc<RwLock<Vec<Message>>>,
    fraction: f32,
) -> Result<usize> {
    // 1. Read and remove the oldest messages from history
    let (removed_messages, remove_count) = {
        let mut hist = history.write().await;
        let total = hist.len();
        let remove_count = ((total as f32 * fraction) as usize).max(1).min(total.saturating_sub(2));
        if remove_count == 0 {
            return Ok(0);
        }
        let removed: Vec<Message> = hist.drain(..remove_count).collect();
        (removed, remove_count)
    };

    // 2. Archive raw transcript
    if let Ok(json) = serde_json::to_string(&removed_messages) {
        logger.archive_transcript(channel_id, &json);
    }

    // 3. Build the transcript text for the LLM
    let transcript = render_messages_as_transcript(&removed_messages);

    // 4. Run the compaction LLM to produce summary + extracted memories
    let model_name = deps.routing.resolve(ProcessType::Worker, None).to_string();
    let model = SpacebotModel::make(&deps.llm_manager, &model_name)
        .with_routing(deps.routing.clone());

    // Give the compaction worker memory_save so it can directly persist memories
    let tool_server: ToolServerHandle = ToolServer::new()
        .tool(crate::tools::MemorySaveTool::new(deps.memory_search.clone()))
        .run();

    let agent = AgentBuilder::new(model)
        .preamble(compactor_prompt)
        .default_max_turns(3)
        .tool_server_handle(tool_server)
        .build();

    let mut compaction_history = Vec::new();
    let response = agent.prompt(&transcript)
        .with_history(&mut compaction_history)
        .await;

    let summary = match response {
        Ok(text) => extract_summary_section(&text),
        Err(error) => {
            tracing::warn!(%error, "compaction LLM failed, using fallback summary");
            format!("[Compaction summary of {remove_count} messages — LLM summarization failed]")
        }
    };

    // 5. Insert the summary at the beginning of the channel's history
    {
        let mut hist = history.write().await;
        let summary_message = format!("[Compaction Summary]: {summary}");
        hist.insert(0, Message::from(summary_message));
    }

    // 6. Persist the summary to SQLite
    logger.save_compaction_summary(channel_id, &summary, remove_count);

    Ok(remove_count)
}

/// Estimate token count for a history using chars/4 heuristic.
///
/// This is intentionally rough — it's only used for threshold checks, not billing.
/// Overestimates slightly, which is the safe direction for compaction triggers.
pub fn estimate_history_tokens(history: &[Message]) -> usize {
    let mut chars = 0usize;

    for message in history {
        match message {
            Message::User { content } => {
                for item in content.iter() {
                    chars += estimate_user_content_chars(item);
                }
            }
            Message::Assistant { content, .. } => {
                for item in content.iter() {
                    chars += estimate_assistant_content_chars(item);
                }
            }
        }
    }

    // ~4 chars per token for English text. Slightly conservative.
    chars / 4
}

fn estimate_user_content_chars(content: &UserContent) -> usize {
    match content {
        UserContent::Text(t) => t.text.len(),
        UserContent::ToolResult(tr) => {
            let mut size = 0;
            for item in tr.content.iter() {
                match item {
                    rig::message::ToolResultContent::Text(t) => size += t.text.len(),
                    rig::message::ToolResultContent::Image(_) => size += 100,
                }
            }
            size
        }
        UserContent::Image(_) => 500,
        UserContent::Audio(_) => 500,
        UserContent::Video(_) => 500,
        UserContent::Document(_) => 1000,
    }
}

fn estimate_assistant_content_chars(content: &AssistantContent) -> usize {
    match content {
        AssistantContent::Text(t) => t.text.len(),
        AssistantContent::ToolCall(tc) => {
            tc.function.name.len() + tc.function.arguments.to_string().len()
        }
        AssistantContent::Reasoning(r) => {
            r.reasoning.iter().map(|s| s.len()).sum()
        }
        AssistantContent::Image(_) => 500,
    }
}

/// Render messages into a human-readable transcript for the compaction LLM.
fn render_messages_as_transcript(messages: &[Message]) -> String {
    let mut output = String::new();

    for message in messages {
        match message {
            Message::User { content } => {
                for item in content.iter() {
                    match item {
                        UserContent::Text(t) => {
                            output.push_str("User: ");
                            output.push_str(&t.text);
                            output.push('\n');
                        }
                        UserContent::ToolResult(tr) => {
                            output.push_str("[Tool Result]: ");
                            for c in tr.content.iter() {
                                if let rig::message::ToolResultContent::Text(t) = c {
                                    output.push_str(&t.text);
                                }
                            }
                            output.push('\n');
                        }
                        _ => {}
                    }
                }
            }
            Message::Assistant { content, .. } => {
                for item in content.iter() {
                    match item {
                        AssistantContent::Text(t) => {
                            output.push_str("Assistant: ");
                            output.push_str(&t.text);
                            output.push('\n');
                        }
                        AssistantContent::ToolCall(tc) => {
                            output.push_str(&format!(
                                "[Tool Call: {}({})]\n",
                                tc.function.name,
                                tc.function.arguments
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    output
}

/// Extract the summary section from the compaction LLM's response.
///
/// The COMPACTOR.md prompt asks for a `## Summary` section followed by
/// `## Extracted Memories`. We want just the summary text.
fn extract_summary_section(response: &str) -> String {
    // Look for "## Summary" header and extract until "## Extracted Memories" or end
    if let Some(start) = response.find("## Summary") {
        let after_header = &response[start + "## Summary".len()..];
        let trimmed = after_header.trim_start_matches(|c: char| c == '\n' || c == '\r');

        if let Some(end) = trimmed.find("## Extracted Memories") {
            trimmed[..end].trim().to_string()
        } else {
            trimmed.trim().to_string()
        }
    } else {
        // No structured output — use the whole response as the summary
        response.trim().to_string()
    }
}

/// Types of compaction actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionAction {
    /// Normal background compaction (~30% of oldest messages).
    Background,
    /// Aggressive compaction (~50% of oldest messages).
    Aggressive,
    /// Emergency truncation (no LLM, drop oldest 50%).
    EmergencyTruncate,
}
