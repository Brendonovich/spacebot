//! Spacebot CLI entry point.

use anyhow::Context as _;
use arc_swap::ArcSwap;
use clap::{Parser, Subcommand};
use futures::StreamExt as _;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "spacebot")]
#[command(about = "A Rust agentic system with dedicated processes for every task")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to config file (optional)
    #[arg(short, long, global = true)]
    config: Option<std::path::PathBuf>,

    /// Enable debug logging
    #[arg(short, long, global = true)]
    debug: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Start the daemon (default when no subcommand is given)
    Start {
        /// Run in the foreground instead of daemonizing
        #[arg(short, long)]
        foreground: bool,
    },
    /// Stop the running daemon
    Stop,
    /// Restart the daemon (stop + start)
    Restart {
        /// Run in the foreground instead of daemonizing
        #[arg(short, long)]
        foreground: bool,
    },
    /// Show status of the running daemon
    Status,
}

/// Tracks an active conversation channel and its message sender.
struct ActiveChannel {
    message_tx: mpsc::Sender<spacebot::InboundMessage>,
    /// Retained so the outbound routing task stays alive.
    _outbound_handle: tokio::task::JoinHandle<()>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Start { foreground: false });

    match command {
        Command::Start { foreground } => cmd_start(cli.config, cli.debug, foreground),
        Command::Stop => cmd_stop(),
        Command::Restart { foreground } => {
            cmd_stop_if_running();
            cmd_start(cli.config, cli.debug, foreground)
        }
        Command::Status => cmd_status(),
    }
}

fn cmd_start(
    config_path: Option<std::path::PathBuf>,
    debug: bool,
    foreground: bool,
) -> anyhow::Result<()> {
    let paths = spacebot::daemon::DaemonPaths::from_default();

    // Bail if already running
    if let Some(pid) = spacebot::daemon::is_running(&paths) {
        eprintln!("spacebot is already running (pid {pid})");
        std::process::exit(1);
    }

    // Run onboarding interactively before daemonizing
    let resolved_config_path = if config_path.is_some() {
        config_path.clone()
    } else if spacebot::config::Config::needs_onboarding() {
        let path = spacebot::config::run_onboarding()
            .with_context(|| "onboarding failed")?;
        Some(path)
    } else {
        None
    };

    // Validate config loads successfully before forking
    let config = load_config(&resolved_config_path)?;

    if foreground {
        spacebot::daemon::init_foreground_tracing(debug);
    } else {
        // Derive paths from the loaded config's instance dir
        let paths = spacebot::daemon::DaemonPaths::new(&config.instance_dir);
        spacebot::daemon::daemonize(&paths)?;
        spacebot::daemon::init_background_tracing(&paths, debug);
    }

    // Build and run the tokio runtime for the async main
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?
        .block_on(run(config, foreground))
}

fn cmd_stop() -> anyhow::Result<()> {
    let paths = spacebot::daemon::DaemonPaths::from_default();

    let Some(pid) = spacebot::daemon::is_running(&paths) else {
        eprintln!("spacebot is not running");
        std::process::exit(1);
    };

    // Use a small runtime for the IPC call
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    runtime.block_on(async {
        match spacebot::daemon::send_command(&paths, spacebot::daemon::IpcCommand::Shutdown).await {
            Ok(spacebot::daemon::IpcResponse::Ok) => {
                eprintln!("stopping spacebot (pid {pid})...");
            }
            Ok(spacebot::daemon::IpcResponse::Error { message }) => {
                eprintln!("shutdown failed: {message}");
                std::process::exit(1);
            }
            Ok(_) => {
                eprintln!("unexpected response from daemon");
                std::process::exit(1);
            }
            Err(error) => {
                eprintln!("failed to send shutdown command: {error}");
                std::process::exit(1);
            }
        }
    });

    if spacebot::daemon::wait_for_exit(pid) {
        eprintln!("spacebot stopped");
    } else {
        eprintln!("spacebot did not stop within 10 seconds (pid {pid})");
        std::process::exit(1);
    }

    Ok(())
}

/// Stop if running, don't error if not.
fn cmd_stop_if_running() {
    let paths = spacebot::daemon::DaemonPaths::from_default();

    let Some(pid) = spacebot::daemon::is_running(&paths) else {
        return;
    };

    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };

    runtime.block_on(async {
        if let Ok(spacebot::daemon::IpcResponse::Ok) =
            spacebot::daemon::send_command(&paths, spacebot::daemon::IpcCommand::Shutdown).await
        {
            eprintln!("stopping spacebot (pid {pid})...");
            spacebot::daemon::wait_for_exit(pid);
        }
    });
}

fn cmd_status() -> anyhow::Result<()> {
    let paths = spacebot::daemon::DaemonPaths::from_default();

    let Some(_pid) = spacebot::daemon::is_running(&paths) else {
        eprintln!("spacebot is not running");
        std::process::exit(1);
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    runtime.block_on(async {
        match spacebot::daemon::send_command(&paths, spacebot::daemon::IpcCommand::Status).await {
            Ok(spacebot::daemon::IpcResponse::Status { pid, uptime_seconds }) => {
                let hours = uptime_seconds / 3600;
                let minutes = (uptime_seconds % 3600) / 60;
                let seconds = uptime_seconds % 60;
                eprintln!("spacebot is running");
                eprintln!("  pid:    {pid}");
                eprintln!("  uptime: {hours}h {minutes}m {seconds}s");
            }
            Ok(spacebot::daemon::IpcResponse::Error { message }) => {
                eprintln!("status query failed: {message}");
                std::process::exit(1);
            }
            Ok(_) => {
                eprintln!("unexpected response from daemon");
                std::process::exit(1);
            }
            Err(error) => {
                eprintln!("failed to query daemon status: {error}");
                std::process::exit(1);
            }
        }
    });

    Ok(())
}

fn load_config(config_path: &Option<std::path::PathBuf>) -> anyhow::Result<spacebot::config::Config> {
    if let Some(path) = config_path {
        spacebot::config::Config::load_from_path(path)
            .with_context(|| format!("failed to load config from {}", path.display()))
    } else {
        spacebot::config::Config::load()
            .with_context(|| "failed to load configuration")
    }
}

async fn run(config: spacebot::config::Config, foreground: bool) -> anyhow::Result<()> {
    let paths = spacebot::daemon::DaemonPaths::new(&config.instance_dir);

    tracing::info!("starting spacebot");
    tracing::info!(instance_dir = %config.instance_dir.display(), "configuration loaded");

    // Start the IPC server for stop/status commands
    let (mut shutdown_rx, _ipc_handle) = spacebot::daemon::start_ipc_server(&paths)
        .await
        .context("failed to start IPC server")?;

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
    // Collected for the file watcher: (agent_id, workspace, runtime_config)
    let mut watcher_agents: Vec<(String, std::path::PathBuf, Arc<spacebot::config::RuntimeConfig>)> = Vec::new();

    // Create the PromptEngine with bundled templates (no file watching, no user overrides)
    let prompt_engine = spacebot::prompts::PromptEngine::new("en")
        .with_context(|| "failed to initialize prompt engine")?;

    for agent_config in &resolved_agents {
        tracing::info!(agent_id = %agent_config.id, "initializing agent");

        // Ensure agent directories exist
        std::fs::create_dir_all(&agent_config.workspace)
            .with_context(|| format!("failed to create workspace: {}", agent_config.workspace.display()))?;
        std::fs::create_dir_all(&agent_config.data_dir)
            .with_context(|| format!("failed to create data dir: {}", agent_config.data_dir.display()))?;
        std::fs::create_dir_all(&agent_config.archives_dir)
            .with_context(|| format!("failed to create archives dir: {}", agent_config.archives_dir.display()))?;
        std::fs::create_dir_all(&agent_config.ingest_dir())
            .with_context(|| format!("failed to create ingest dir: {}", agent_config.ingest_dir().display()))?;
        std::fs::create_dir_all(&agent_config.logs_dir())
            .with_context(|| format!("failed to create logs dir: {}", agent_config.logs_dir().display()))?;

        // Per-agent database connections
        let db = spacebot::db::Db::connect(&agent_config.data_dir)
            .await
            .with_context(|| format!("failed to connect databases for agent '{}'", agent_config.id))?;

        // Per-agent memory system
        let memory_store = spacebot::memory::MemoryStore::new(db.sqlite.clone());
        let embedding_table = spacebot::memory::EmbeddingTable::open_or_create(&db.lance)
            .await
            .with_context(|| format!("failed to init embeddings for agent '{}'", agent_config.id))?;

        // Ensure FTS index exists for full-text search queries
        if let Err(error) = embedding_table.ensure_fts_index().await {
            tracing::warn!(%error, agent = %agent_config.id, "failed to create FTS index");
        }

        let memory_search = Arc::new(spacebot::memory::MemorySearch::new(
            memory_store,
            embedding_table,
            embedding_model.clone(),
        ));

        // Per-agent event bus (broadcast for fan-out to multiple channels)
        let (event_tx, _event_rx) = tokio::sync::broadcast::channel(256);

        let agent_id: spacebot::AgentId = Arc::from(agent_config.id.as_str());

        // Scaffold identity templates if missing, then load
        spacebot::identity::scaffold_identity_files(&agent_config.workspace)
            .await
            .with_context(|| format!("failed to scaffold identity files for agent '{}'", agent_config.id))?;
        let identity = spacebot::identity::Identity::load(&agent_config.workspace).await;

        // Load skills (instance-level, then workspace overrides)
        let skills = spacebot::skills::SkillSet::load(
            &config.skills_dir(),
            &agent_config.skills_dir(),
        ).await;

        // Build the RuntimeConfig with all hot-reloadable values
        let runtime_config = Arc::new(spacebot::config::RuntimeConfig::new(
            agent_config,
            &config.defaults,
            prompt_engine.clone(),
            identity,
            skills,
        ));

        watcher_agents.push((
            agent_config.id.clone(),
            agent_config.workspace.clone(),
            runtime_config.clone(),
        ));

        let deps = spacebot::AgentDeps {
            agent_id: agent_id.clone(),
            memory_search,
            llm_manager: llm_manager.clone(),
            heartbeat_tool: None,
            runtime_config,
            event_tx,
            sqlite_pool: db.sqlite.clone(),
        };

        let agent = spacebot::Agent {
            id: agent_id.clone(),
            config: agent_config.clone(),
            db,
            deps,
        };

        tracing::info!(agent_id = %agent_config.id, "agent initialized");
        agents.insert(agent_id, agent);
    }

    tracing::info!(agent_count = agents.len(), "all agents initialized");

    // Initialize messaging adapters
    let mut messaging_manager = spacebot::messaging::MessagingManager::new();

    // Shared Discord permissions (hot-reloadable via file watcher)
    let discord_permissions = config.messaging.discord.as_ref().map(|discord_config| {
        let perms = spacebot::config::DiscordPermissions::from_config(discord_config, &config.bindings);
        Arc::new(ArcSwap::from_pointee(perms))
    });

    if let Some(discord_config) = &config.messaging.discord {
        if discord_config.enabled {
            let adapter = spacebot::messaging::discord::DiscordAdapter::new(
                &discord_config.token,
                discord_permissions.clone().expect("discord permissions initialized when discord is enabled"),
            );
            messaging_manager.register(adapter);
        }
    }

    // Shared Slack permissions (hot-reloadable via file watcher)
    let slack_permissions = config.messaging.slack.as_ref().map(|slack_config| {
        let perms = spacebot::config::SlackPermissions::from_config(slack_config, &config.bindings);
        Arc::new(ArcSwap::from_pointee(perms))
    });

    if let Some(slack_config) = &config.messaging.slack {
        if slack_config.enabled {
            let adapter = spacebot::messaging::slack::SlackAdapter::new(
                &slack_config.bot_token,
                &slack_config.app_token,
                slack_permissions.clone().expect("slack permissions initialized when slack is enabled"),
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

    // Initialize heartbeat schedulers for each agent
    let mut heartbeat_schedulers = Vec::new();
    for (agent_id, agent) in &mut agents {
        let store = Arc::new(spacebot::heartbeat::HeartbeatStore::new(agent.db.sqlite.clone()));

        // Seed heartbeats from config into the database
        for heartbeat_def in &agent.config.heartbeats {
            let hb_config = spacebot::heartbeat::HeartbeatConfig {
                id: heartbeat_def.id.clone(),
                prompt: heartbeat_def.prompt.clone(),
                interval_secs: heartbeat_def.interval_secs,
                delivery_target: heartbeat_def.delivery_target.clone(),
                active_hours: heartbeat_def.active_hours,
                enabled: heartbeat_def.enabled,
            };
            if let Err(error) = store.save(&hb_config).await {
                tracing::warn!(
                    agent_id = %agent_id,
                    heartbeat_id = %heartbeat_def.id,
                    %error,
                    "failed to seed heartbeat config"
                );
            }
        }

        // Load all enabled heartbeats and start the scheduler
        let heartbeat_context = spacebot::heartbeat::HeartbeatContext {
            deps: agent.deps.clone(),
            screenshot_dir: agent.config.screenshot_dir(),
            logs_dir: agent.config.logs_dir(),
            messaging_manager: messaging_manager.clone(),
            store: store.clone(),
        };

        let scheduler = Arc::new(spacebot::heartbeat::Scheduler::new(heartbeat_context));

        // Make heartbeat store and scheduler available via RuntimeConfig
        agent.deps.runtime_config.set_heartbeat(store.clone(), scheduler.clone());

        match store.load_all().await {
            Ok(configs) => {
                for hb_config in configs {
                    if let Err(error) = scheduler.register(hb_config).await {
                        tracing::warn!(agent_id = %agent_id, %error, "failed to register heartbeat");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(agent_id = %agent_id, %error, "failed to load heartbeats from database");
            }
        }

        // Store heartbeat tool on deps so each channel can register it on its own tool server
        let heartbeat_tool = spacebot::tools::HeartbeatTool::new(store, scheduler.clone());
        agent.deps.heartbeat_tool = Some(heartbeat_tool);

        heartbeat_schedulers.push(scheduler);
        tracing::info!(agent_id = %agent_id, "heartbeat scheduler started");
    }

    // Start memory ingestion loops for each agent
    let mut _ingestion_handles = Vec::new();
    for (agent_id, agent) in &agents {
        let ingestion_config = **agent.deps.runtime_config.ingestion.load();
        if ingestion_config.enabled {
            let handle = spacebot::agent::ingestion::spawn_ingestion_loop(
                agent.config.ingest_dir(),
                agent.deps.clone(),
            );
            _ingestion_handles.push(handle);
            tracing::info!(agent_id = %agent_id, "memory ingestion loop started");
        }
    }

    // Start cortex bulletin loops for each agent
    let mut _cortex_handles = Vec::new();
    for (agent_id, agent) in &agents {
        let handle = spacebot::agent::cortex::spawn_bulletin_loop(agent.deps.clone());
        _cortex_handles.push(handle);
        tracing::info!(agent_id = %agent_id, "cortex bulletin loop started");
    }

    let default_agent_id = config.default_agent_id().to_string();
    let bindings: Arc<ArcSwap<Vec<spacebot::config::Binding>>> =
        Arc::new(ArcSwap::from_pointee(config.bindings.clone()));

    // Start file watcher for hot-reloading config, prompts, identity, skills, bindings, and permissions
    let config_path = config.instance_dir.join("config.toml");
    let _file_watcher = spacebot::config::spawn_file_watcher(
        config_path,
        config.instance_dir.clone(),
        watcher_agents,
        discord_permissions,
        slack_permissions,
        bindings.clone(),
    );

    if foreground {
        eprintln!("spacebot running in foreground (pid {})", std::process::id());
    } else {
        tracing::info!(pid = std::process::id(), "spacebot daemon started");
    }

    // Active conversation channels: conversation_id -> ActiveChannel
    let mut active_channels: HashMap<String, ActiveChannel> = HashMap::new();

    // Main event loop: route inbound messages to agent channels
    loop {
        tokio::select! {
            Some(mut message) = inbound_stream.next() => {
                // Resolve which agent handles this message (bindings hot-reload on config change)
                let current_bindings = bindings.load();
                let agent_id = spacebot::config::resolve_agent_for_message(
                    &current_bindings,
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
                        response_tx,
                        event_rx,
                        agent.config.screenshot_dir(),
                        agent.config.logs_dir(),
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
            _ = shutdown_rx.wait_for(|shutdown| *shutdown) => {
                tracing::info!("shutdown signal received via IPC");
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutdown signal received");
                break;
            }
        }
    }

    // Graceful shutdown
    drop(active_channels);

    for scheduler in &heartbeat_schedulers {
        scheduler.shutdown().await;
    }
    drop(heartbeat_schedulers);

    messaging_manager.shutdown().await;

    for (agent_id, agent) in agents {
        tracing::info!(%agent_id, "shutting down agent");
        agent.db.close().await;
    }

    tracing::info!("spacebot stopped");

    spacebot::daemon::cleanup(&paths);

    // Force exit — detached tasks (e.g. the serenity gateway client) may keep
    // the tokio runtime alive after all owned resources have been cleaned up.
    std::process::exit(0);
}
