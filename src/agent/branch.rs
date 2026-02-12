//! Branch: Fork context for thinking and delegation.

use crate::error::Result;
use crate::llm::SpacebotModel;
use crate::{BranchId, ChannelId, ProcessId, ProcessType, AgentDeps, ProcessEvent};
use crate::hooks::SpacebotHook;
use rig::agent::AgentBuilder;
use rig::completion::{CompletionModel, Prompt};
use uuid::Uuid;

/// A branch is a fork of a channel's context for thinking.
pub struct Branch {
    pub id: BranchId,
    pub channel_id: ChannelId,
    pub description: String,
    pub deps: AgentDeps,
    pub hook: SpacebotHook,
    /// System prompt loaded from prompts/BRANCH.md.
    pub system_prompt: String,
    /// Clone of the channel's history at fork time (Rig message format).
    pub history: Vec<rig::message::Message>,
}

impl Branch {
    /// Create a new branch from a channel.
    pub fn new(
        channel_id: ChannelId,
        description: impl Into<String>,
        deps: AgentDeps,
        system_prompt: impl Into<String>,
        history: Vec<rig::message::Message>,
    ) -> Self {
        let id = Uuid::new_v4();
        let process_id = ProcessId::Branch(id);
        let hook = SpacebotHook::new(deps.agent_id.clone(), process_id, ProcessType::Branch, deps.event_tx.clone());
        
        Self {
            id,
            channel_id,
            description: description.into(),
            deps,
            hook,
            system_prompt: system_prompt.into(),
            history,
        }
    }
    
    /// Run the branch's LLM agent loop and return a conclusion.
    ///
    /// The branch gets the channel's shared ToolServer which has memory_recall
    /// and memory_save pre-registered. It runs the agent loop with max_turns(10),
    /// then sends a BranchResult event with the conclusion.
    pub async fn run(mut self, prompt: impl Into<String>) -> Result<String> {
        let prompt = prompt.into();
        
        tracing::info!(
            branch_id = %self.id,
            channel_id = %self.channel_id,
            description = %self.description,
            "branch starting"
        );

        let model_name = self.deps.routing.resolve(ProcessType::Branch, None).to_string();
        let model = SpacebotModel::make(&self.deps.llm_manager, &model_name)
            .with_routing(self.deps.routing.clone());

        let agent = AgentBuilder::new(model)
            .preamble(&self.system_prompt)
            .default_max_turns(50)
            .tool_server_handle(self.deps.tool_server.clone())
            .build();

        let conclusion = match agent.prompt(&prompt)
            .with_history(&mut self.history)
            .with_hook(self.hook.clone())
            .await
        {
            Ok(response) => response,
            Err(rig::completion::PromptError::MaxTurnsError { .. }) => {
                // Extract the last assistant text from history as a partial conclusion
                let partial = extract_last_assistant_text(&self.history)
                    .unwrap_or_else(|| "Branch exhausted its turns without a final conclusion.".into());
                tracing::warn!(branch_id = %self.id, "branch hit max turns, returning partial result");
                partial
            }
            Err(rig::completion::PromptError::PromptCancelled { reason, .. }) => {
                tracing::info!(branch_id = %self.id, %reason, "branch cancelled");
                format!("Branch was cancelled: {reason}")
            }
            Err(error) => {
                tracing::error!(branch_id = %self.id, %error, "branch LLM call failed");
                return Err(crate::error::AgentError::Other(error.into()).into());
            }
        };

        // Send conclusion back to the channel
        let _ = self.deps.event_tx.send(ProcessEvent::BranchResult {
            agent_id: self.deps.agent_id.clone(),
            branch_id: self.id,
            channel_id: self.channel_id.clone(),
            conclusion: conclusion.clone(),
        });
        
        tracing::info!(branch_id = %self.id, "branch completed");
        
        Ok(conclusion)
    }
}

/// Extract the last assistant text message from a history.
fn extract_last_assistant_text(history: &[rig::message::Message]) -> Option<String> {
    for message in history.iter().rev() {
        if let rig::message::Message::Assistant { content, .. } = message {
            let texts: Vec<String> = content.iter()
                .filter_map(|c| {
                    if let rig::message::AssistantContent::Text(t) = c {
                        Some(t.text.clone())
                    } else {
                        None
                    }
                })
                .collect();
            if !texts.is_empty() {
                return Some(texts.join("\n"));
            }
        }
    }
    None
}
