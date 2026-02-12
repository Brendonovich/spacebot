//! Worker: Independent task execution process.

use crate::error::Result;
use crate::llm::SpacebotModel;
use crate::{WorkerId, ChannelId, ProcessId, ProcessType, AgentDeps};
use crate::hooks::SpacebotHook;
use rig::agent::AgentBuilder;
use rig::completion::{CompletionModel, Prompt};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

/// Worker state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    /// Worker is running and processing.
    Running,
    /// Worker is waiting for follow-up input (interactive only).
    WaitingForInput,
    /// Worker has completed successfully.
    Done,
    /// Worker has failed.
    Failed,
}

/// A worker process that executes tasks independently.
pub struct Worker {
    pub id: WorkerId,
    pub channel_id: Option<ChannelId>,
    pub task: String,
    pub state: WorkerState,
    pub deps: AgentDeps,
    pub hook: SpacebotHook,
    /// System prompt loaded from prompts/WORKER.md.
    pub system_prompt: String,
    /// Input channel for interactive workers.
    pub input_rx: Option<mpsc::Receiver<String>>,
    /// Status updates.
    pub status_tx: watch::Sender<String>,
    pub status_rx: watch::Receiver<String>,
}

impl Worker {
    /// Create a new fire-and-forget worker.
    pub fn new(
        channel_id: Option<ChannelId>,
        task: impl Into<String>,
        system_prompt: impl Into<String>,
        deps: AgentDeps,
    ) -> Self {
        let id = Uuid::new_v4();
        let process_id = ProcessId::Worker(id);
        let hook = SpacebotHook::new(deps.agent_id.clone(), process_id, ProcessType::Worker, deps.event_tx.clone());
        let (status_tx, status_rx) = watch::channel("starting".to_string());
        
        Self {
            id,
            channel_id,
            task: task.into(),
            state: WorkerState::Running,
            deps,
            hook,
            system_prompt: system_prompt.into(),
            input_rx: None,
            status_tx,
            status_rx,
        }
    }
    
    /// Create a new interactive worker.
    pub fn new_interactive(
        channel_id: Option<ChannelId>,
        task: impl Into<String>,
        system_prompt: impl Into<String>,
        deps: AgentDeps,
    ) -> (Self, mpsc::Sender<String>) {
        let id = Uuid::new_v4();
        let process_id = ProcessId::Worker(id);
        let hook = SpacebotHook::new(deps.agent_id.clone(), process_id, ProcessType::Worker, deps.event_tx.clone());
        let (status_tx, status_rx) = watch::channel("starting".to_string());
        let (input_tx, input_rx) = mpsc::channel(32);
        
        let worker = Self {
            id,
            channel_id,
            task: task.into(),
            state: WorkerState::Running,
            deps,
            hook,
            system_prompt: system_prompt.into(),
            input_rx: Some(input_rx),
            status_tx,
            status_rx,
        };
        
        (worker, input_tx)
    }
    
    /// Check if the worker can transition to a new state.
    pub fn can_transition_to(&self, target: WorkerState) -> bool {
        use WorkerState::*;
        
        matches!(
            (self.state, target),
            (Running, WaitingForInput)
                | (Running, Done)
                | (Running, Failed)
                | (WaitingForInput, Running)
                | (WaitingForInput, Failed)
        )
    }
    
    /// Transition to a new state.
    pub fn transition_to(&mut self, new_state: WorkerState) -> Result<()> {
        if !self.can_transition_to(new_state) {
            return Err(crate::error::AgentError::InvalidStateTransition(
                format!("can't transition from {:?} to {:?}", self.state, new_state)
            ).into());
        }
        
        self.state = new_state;
        Ok(())
    }
    
    /// Run the worker's LLM agent loop until completion.
    ///
    /// Creates a per-worker ToolServer with shell, file, exec, and set_status tools.
    /// For fire-and-forget workers, runs once and returns. For interactive workers,
    /// waits for follow-up messages on input_rx and runs additional turns.
    pub async fn run(mut self) -> Result<String> {
        self.status_tx.send_modify(|s| *s = "running".to_string());
        self.hook.send_status("running");
        
        tracing::info!(worker_id = %self.id, task = %self.task, "worker starting");

        // Create per-worker ToolServer with task tools
        let worker_tool_server = crate::tools::create_worker_tool_server(
            self.deps.agent_id.clone(),
            self.id,
            self.channel_id.clone(),
            self.deps.event_tx.clone(),
        );

        let model_name = self.deps.routing.resolve(ProcessType::Worker, None).to_string();
        let model = SpacebotModel::make(&self.deps.llm_manager, &model_name)
            .with_routing(self.deps.routing.clone());

        let agent = AgentBuilder::new(model)
            .preamble(&self.system_prompt)
            .default_max_turns(100)
            .tool_server_handle(worker_tool_server)
            .build();

        // Fresh history for the worker (no channel context)
        let mut history = Vec::new();

        // Run the initial task
        let result = match agent.prompt(&self.task)
            .with_history(&mut history)
            .with_hook(self.hook.clone())
            .await
        {
            Ok(response) => response,
            Err(rig::completion::PromptError::MaxTurnsError { .. }) => {
                self.state = WorkerState::Done;
                self.hook.send_status("completed (hit turn limit)");
                let partial = extract_last_assistant_text(&history)
                    .unwrap_or_else(|| "Worker exhausted its turns.".into());
                tracing::warn!(worker_id = %self.id, "worker hit max turns");
                return Ok(partial);
            }
            Err(rig::completion::PromptError::PromptCancelled { reason, .. }) => {
                self.state = WorkerState::Failed;
                self.hook.send_status("cancelled");
                tracing::info!(worker_id = %self.id, %reason, "worker cancelled");
                return Ok(format!("Worker cancelled: {reason}"));
            }
            Err(error) => {
                self.state = WorkerState::Failed;
                self.hook.send_status("failed");
                tracing::error!(worker_id = %self.id, %error, "worker LLM call failed");
                return Err(crate::error::AgentError::Other(error.into()).into());
            }
        };

        // For interactive workers, enter a follow-up loop
        if let Some(mut input_rx) = self.input_rx.take() {
            self.state = WorkerState::WaitingForInput;
            self.hook.send_status("waiting for input");

            while let Some(follow_up) = input_rx.recv().await {
                self.state = WorkerState::Running;
                self.hook.send_status("processing follow-up");

                match agent.prompt(&follow_up)
                    .with_history(&mut history)
                    .with_hook(self.hook.clone())
                    .await
                {
                    Ok(_response) => {
                        self.state = WorkerState::WaitingForInput;
                        self.hook.send_status("waiting for input");
                    }
                    Err(error) => {
                        tracing::error!(worker_id = %self.id, %error, "worker follow-up failed");
                        self.state = WorkerState::Failed;
                        self.hook.send_status("failed");
                        break;
                    }
                }
            }
        }

        self.state = WorkerState::Done;
        self.hook.send_status("summarizing");

        // Summary phase: ask the worker to produce a concise result for the channel.
        // The full tool history is in `history` so the LLM can reference everything.
        // Single turn, no tools — just text.
        let summary = match agent.prompt(SUMMARY_PROMPT)
            .with_history(&mut history)
            .max_turns(1)
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(worker_id = %self.id, %error, "summary phase failed, using raw result");
                result
            }
        };

        self.hook.send_status("completed");
        tracing::info!(worker_id = %self.id, "worker completed");
        Ok(summary)
    }
    
    /// Check if worker is in a terminal state.
    pub fn is_done(&self) -> bool {
        matches!(self.state, WorkerState::Done | WorkerState::Failed)
    }
    
    /// Check if worker is interactive.
    pub fn is_interactive(&self) -> bool {
        self.input_rx.is_some()
    }
}

/// Prompt sent after the task phase to get a concise summary for the channel.
const SUMMARY_PROMPT: &str = "\
Your task is complete. Now produce a concise summary of the result for the channel \
that spawned you. The channel has limited context — do not dump raw file contents or \
full command output. Instead, summarize what you did, what you found, and any key \
details the channel needs. If the task was to retrieve specific information, include \
the relevant parts — not the entire output. Keep it focused and useful. Do not use tools.";

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
