//! Spacebot CLI entry point.

use anyhow::Context as _;
use clap::Parser;
use futures::StreamExt as _;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "spacebot")]
#[command(about = "A Rust agentic system with dedicated processes for every task")]
struct Cli {
    /// Path to config file (optional)
    #[arg(short, long)]
    config: Option<std::path::PathBuf>,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,
}

/// Tracks an active conversation channel and its message sender.
struct ActiveChannel {
    message_tx: mpsc::Sender<spacebot::InboundMessage>,
    /// Retained so the outbound routing task stays alive.
    _outbound_handle: tokio::task::JoinHandle<()>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let filter = if cli.debug {
        EnvFilter::new("debug")
    } else {
        EnvFilter::new("info")
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();

    tracing::info!("starting spacebot");

    // Load configuration
    let config = if let Some(config_path) = cli.config {
        spacebot::config::Config::load_from_path(&config_path)
            .with_context(|| format!("failed to load config from {}", config_path.display()))?
    } else {
        spacebot::config::Config::load()
            .with_context(|| "failed to load configuration")?
    };

    tracing::info!(instance_dir = %config.instance_dir.display(), "configuration loaded");

    // Shared LLM manager (same API keys for all agents)
    let llm_manager = Arc::new(
        spacebot::llm::LlmManager::new(config.llm.clone())
            .await
            .with_context(|| "failed to initialize LLM manager")?
    );

    // Shared embedding model (stateless, agent-agnostic)
    let embedding_cache_dir = config.instance_dir.join("embedding_cache");
    let embedding_model = Arc::new(
        spacebot::memory::EmbeddingModel::new(&embedding_cache_dir)
            .context("failed to initialize embedding model")?
    );

    tracing::info!("shared resources initialized");

    // Resolve agent configs and initialize each agent
    let resolved_agents = config.resolve_agents();
    let mut agents: HashMap<spacebot::AgentId, spacebot::Agent> = HashMap::new();

    let shared_prompts_dir = config.prompts_dir();

    spacebot::identity::scaffold_default_prompts(&shared_prompts_dir)
        .await
        .with_context(|| "failed to scaffold default prompts")?;

    for agent_config in &resolved_agents {
        tracing::info!(agent_id = %agent_config.id, "initializing agent");

        // Ensure agent directories exist
        std::fs::create_dir_all(&agent_config.workspace)
            .with_context(|| format!("failed to create workspace: {}", agent_config.workspace.display()))?;
        std::fs::create_dir_all(&agent_config.data_dir)
            .with_context(|| format!("failed to create data dir: {}", agent_config.data_dir.display()))?;
        std::fs::create_dir_all(&agent_config.archives_dir)
            .with_context(|| format!("failed to create archives dir: {}", agent_config.archives_dir.display()))?;

        // Per-agent database connections
        let db = spacebot::db::Db::connect(&agent_config.data_dir)
            .await
            .with_context(|| format!("failed to connect databases for agent '{}'", agent_config.id))?;

        // Per-agent memory system
        let memory_store = spacebot::memory::MemoryStore::new(db.sqlite.clone());
        let embedding_table = spacebot::memory::EmbeddingTable::open_or_create(&db.lance)
            .await
            .with_context(|| format!("failed to init embeddings for agent '{}'", agent_config.id))?;

        let memory_search = Arc::new(spacebot::memory::MemorySearch::new(
            memory_store,
            embedding_table,
            embedding_model.clone(),
        ));

        // Per-agent event bus (broadcast for fan-out to multiple channels)
        let (event_tx, _event_rx) = tokio::sync::broadcast::channel(256);

        // Per-agent tool server with memory tools pre-registered
        let tool_server = spacebot::tools::create_channel_tool_server(memory_search.clone());

        let agent_id: spacebot::AgentId = Arc::from(agent_config.id.as_str());

        let deps = spacebot::AgentDeps {
            agent_id: agent_id.clone(),
            memory_search,
            llm_manager: llm_manager.clone(),
            tool_server,
            routing: agent_config.routing.clone(),
            event_tx,
            sqlite_pool: db.sqlite.clone(),
        };

        // Scaffold identity templates if missing, then load
        spacebot::identity::scaffold_identity_files(&agent_config.workspace)
            .await
            .with_context(|| format!("failed to scaffold identity files for agent '{}'", agent_config.id))?;
        let identity = spacebot::identity::Identity::load(&agent_config.workspace).await;

        // Load prompts (agent overrides, then shared)
        let prompts = spacebot::identity::Prompts::load(
            &agent_config.workspace,
            &shared_prompts_dir,
        ).await.with_context(|| format!("failed to load prompts for agent '{}'", agent_config.id))?;

        let agent = spacebot::Agent {
            id: agent_id.clone(),
            config: agent_config.clone(),
            db,
            deps,
            prompts,
            identity,
        };

        tracing::info!(agent_id = %agent_config.id, "agent initialized");
        agents.insert(agent_id, agent);
    }

    tracing::info!(agent_count = agents.len(), "all agents initialized");

    // Initialize messaging adapters
    let mut messaging_manager = spacebot::messaging::MessagingManager::new();

    if let Some(discord_config) = &config.messaging.discord {
        if discord_config.enabled {
            let discord_bindings: Vec<&spacebot::config::Binding> = config
                .bindings
                .iter()
                .filter(|b| b.channel == "discord")
                .collect();

            let guild_filter: Option<Vec<u64>> = {
                let guild_ids: Vec<u64> = discord_bindings
                    .iter()
                    .filter_map(|b| b.guild_id.as_ref()?.parse::<u64>().ok())
                    .collect();

                if guild_ids.is_empty() {
                    None
                } else {
                    Some(guild_ids)
                }
            };

            let channel_filter: HashMap<u64, Vec<u64>> = {
                let mut filter: HashMap<u64, Vec<u64>> = HashMap::new();
                for binding in &discord_bindings {
                    if let Some(guild_id) = binding.guild_id.as_ref().and_then(|g| g.parse::<u64>().ok()) {
                        if !binding.channel_ids.is_empty() {
                            let channel_ids: Vec<u64> = binding
                                .channel_ids
                                .iter()
                                .filter_map(|id| id.parse::<u64>().ok())
                                .collect();
                            filter.entry(guild_id).or_default().extend(channel_ids);
                        }
                    }
                }
                filter
            };

            let adapter = spacebot::messaging::discord::DiscordAdapter::new(
                &discord_config.token,
                guild_filter,
                channel_filter,
            );
            messaging_manager.register(adapter);
        }
    }

    let messaging_manager = Arc::new(messaging_manager);

    // Start all messaging adapters and get the merged inbound stream
    let mut inbound_stream = messaging_manager
        .start()
        .await
        .context("failed to start messaging adapters")?;

    tracing::info!("messaging adapters started");

    let default_agent_id = config.default_agent_id().to_string();
    let bindings = config.bindings.clone();

    // Active conversation channels: conversation_id -> ActiveChannel
    let mut active_channels: HashMap<String, ActiveChannel> = HashMap::new();

    // Main event loop: route inbound messages to agent channels
    loop {
        tokio::select! {
            Some(mut message) = inbound_stream.next() => {
                // Resolve which agent handles this message
                let agent_id = spacebot::config::resolve_agent_for_message(
                    &bindings,
                    &message,
                    &default_agent_id,
                );
                message.agent_id = Some(agent_id.clone());

                let conversation_id = message.conversation_id.clone();

                // Find or create a channel for this conversation
                if !active_channels.contains_key(&conversation_id) {
                    let Some(agent) = agents.get(&agent_id) else {
                        tracing::warn!(
                            agent_id = %agent_id,
                            conversation_id = %conversation_id,
                            "message routed to unknown agent, dropping"
                        );
                        continue;
                    };

                    // Create outbound response channel
                    let (response_tx, mut response_rx) = mpsc::channel::<spacebot::OutboundResponse>(32);

                    // Subscribe to the agent's event bus
                    let event_rx = agent.deps.event_tx.subscribe();

                    let channel_id: spacebot::ChannelId = Arc::from(conversation_id.as_str());

                    let (channel, channel_tx) = spacebot::agent::channel::Channel::new(
                        channel_id,
                        agent.deps.clone(),
                        spacebot::agent::channel::ChannelConfig {
                            max_concurrent_branches: agent.config.max_concurrent_branches,
                            max_turns: agent.config.max_turns,
                            context_window: agent.config.context_window,
                            compaction: agent.config.compaction,
                        },
                        &agent.prompts.channel,
                        agent.identity.render(),
                        &agent.prompts.branch,
                        &agent.prompts.worker,
                        &agent.prompts.compactor,
                        response_tx,
                        event_rx,
                    );

                    // Backfill recent message history from the platform
                    let backfill_count = agent.config.history_backfill_count();
                    if backfill_count > 0 {
                        match messaging_manager.fetch_history(&message, backfill_count).await {
                            Ok(history_messages) if !history_messages.is_empty() => {
                                let mut transcript = String::from("[Previous conversation in this channel]\n\n");
                                for entry in &history_messages {
                                    let label = if entry.is_bot { "(you)" } else { &entry.author };
                                    transcript.push_str(&format!("{}: {}\n", label, entry.content));
                                }
                                transcript.push_str("\n[End of previous conversation]");

                                let mut history = channel.state.history.write().await;
                                history.push(rig::message::Message::from(transcript));
                                drop(history);

                                tracing::info!(
                                    conversation_id = %conversation_id,
                                    message_count = history_messages.len(),
                                    "backfilled channel history"
                                );
                            }
                            Err(error) => {
                                tracing::warn!(%error, "failed to backfill channel history");
                            }
                            _ => {}
                        }
                    }

                    // Spawn the channel's event loop
                    tokio::spawn(async move {
                        if let Err(error) = channel.run().await {
                            tracing::error!(%error, "channel event loop failed");
                        }
                    });

                    // Spawn outbound response routing: reads from response_rx,
                    // sends to the messaging adapter
                    let messaging_for_outbound = messaging_manager.clone();
                    let outbound_message = message.clone();
                    let outbound_conversation_id = conversation_id.clone();
                    let outbound_handle = tokio::spawn(async move {
                        while let Some(response) = response_rx.recv().await {
                            match response {
                                spacebot::OutboundResponse::Status(status) => {
                                    if let Err(error) = messaging_for_outbound
                                        .send_status(&outbound_message, status)
                                        .await
                                    {
                                        tracing::warn!(%error, "failed to send status update");
                                    }
                                }
                                response => {
                                    tracing::info!(
                                        conversation_id = %outbound_conversation_id,
                                        "routing outbound response to messaging adapter"
                                    );
                                    if let Err(error) = messaging_for_outbound
                                        .respond(&outbound_message, response)
                                        .await
                                    {
                                        tracing::error!(%error, "failed to send outbound response");
                                    }
                                }
                            }
                        }
                    });

                    active_channels.insert(conversation_id.clone(), ActiveChannel {
                        message_tx: channel_tx,
                        _outbound_handle: outbound_handle,
                    });

                    tracing::info!(
                        conversation_id = %conversation_id,
                        agent_id = %agent_id,
                        "new channel created"
                    );
                }

                // Forward the message to the channel
                if let Some(active) = active_channels.get(&conversation_id) {
                    if let Err(error) = active.message_tx.send(message).await {
                        tracing::error!(
                            conversation_id = %conversation_id,
                            %error,
                            "failed to forward message to channel"
                        );
                        active_channels.remove(&conversation_id);
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutdown signal received");
                break;
            }
        }
    }

    // Graceful shutdown
    drop(active_channels);
    messaging_manager.shutdown().await;

    for (agent_id, agent) in agents {
        tracing::info!(%agent_id, "shutting down agent");
        agent.db.close().await;
    }

    tracing::info!("spacebot stopped");
    Ok(())
}
