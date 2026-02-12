//! Configuration loading and validation.

use crate::error::{ConfigError, Result};
use crate::llm::routing::RoutingConfig;
use anyhow::Context as _;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Top-level Spacebot configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Instance root directory (~/.spacebot or SPACEBOT_DIR).
    pub instance_dir: PathBuf,
    /// LLM provider credentials (shared across all agents).
    pub llm: LlmConfig,
    /// Default settings inherited by all agents.
    pub defaults: DefaultsConfig,
    /// Agent definitions.
    pub agents: Vec<AgentConfig>,
    /// Messaging platform credentials.
    pub messaging: MessagingConfig,
    /// Routing bindings (maps platform conversations to agents).
    pub bindings: Vec<Binding>,
}

/// LLM provider credentials (instance-level).
#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub anthropic_key: Option<String>,
    pub openai_key: Option<String>,
    pub openrouter_key: Option<String>,
}

/// Defaults inherited by all agents. Individual agents can override any field.
#[derive(Debug, Clone)]
pub struct DefaultsConfig {
    pub routing: RoutingConfig,
    pub max_concurrent_branches: usize,
    pub max_turns: usize,
    pub context_window: usize,
    pub compaction: CompactionConfig,
    pub cortex: CortexConfig,
    /// Number of messages to fetch from the platform when a new channel is created.
    pub history_backfill_count: usize,
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            routing: RoutingConfig::default(),
            max_concurrent_branches: 5,
            max_turns: 5,
            context_window: 128_000,
            compaction: CompactionConfig::default(),
            cortex: CortexConfig::default(),
            history_backfill_count: 50,
        }
    }
}

/// Compaction threshold configuration.
#[derive(Debug, Clone, Copy)]
pub struct CompactionConfig {
    pub background_threshold: f32,
    pub aggressive_threshold: f32,
    pub emergency_threshold: f32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            background_threshold: 0.80,
            aggressive_threshold: 0.85,
            emergency_threshold: 0.95,
        }
    }
}

/// Cortex configuration.
#[derive(Debug, Clone, Copy)]
pub struct CortexConfig {
    pub tick_interval_secs: u64,
    pub worker_timeout_secs: u64,
    pub branch_timeout_secs: u64,
    pub circuit_breaker_threshold: u8,
}

impl Default for CortexConfig {
    fn default() -> Self {
        Self {
            tick_interval_secs: 30,
            worker_timeout_secs: 300,
            branch_timeout_secs: 60,
            circuit_breaker_threshold: 3,
        }
    }
}

/// Per-agent configuration (raw, before resolution with defaults).
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub id: String,
    pub default: bool,
    /// Custom workspace path. If None, resolved to instance_dir/agents/{id}/workspace.
    pub workspace: Option<PathBuf>,
    /// Per-agent routing overrides. None inherits from defaults.
    pub routing: Option<RoutingConfig>,
    pub max_concurrent_branches: Option<usize>,
    pub max_turns: Option<usize>,
    pub context_window: Option<usize>,
    pub compaction: Option<CompactionConfig>,
    pub cortex: Option<CortexConfig>,
}

/// Fully resolved agent config (merged with defaults, paths resolved).
#[derive(Debug, Clone)]
pub struct ResolvedAgentConfig {
    pub id: String,
    pub workspace: PathBuf,
    pub data_dir: PathBuf,
    pub archives_dir: PathBuf,
    pub routing: RoutingConfig,
    pub max_concurrent_branches: usize,
    pub max_turns: usize,
    pub context_window: usize,
    pub compaction: CompactionConfig,
    pub cortex: CortexConfig,
    pub history_backfill_count: usize,
}

impl AgentConfig {
    /// Resolve this agent config against instance defaults and base paths.
    pub fn resolve(&self, instance_dir: &Path, defaults: &DefaultsConfig) -> ResolvedAgentConfig {
        let agent_root = instance_dir.join("agents").join(&self.id);

        ResolvedAgentConfig {
            id: self.id.clone(),
            workspace: self
                .workspace
                .clone()
                .unwrap_or_else(|| agent_root.join("workspace")),
            data_dir: agent_root.join("data"),
            archives_dir: agent_root.join("archives"),
            routing: self
                .routing
                .clone()
                .unwrap_or_else(|| defaults.routing.clone()),
            max_concurrent_branches: self
                .max_concurrent_branches
                .unwrap_or(defaults.max_concurrent_branches),
            max_turns: self.max_turns.unwrap_or(defaults.max_turns),
            context_window: self.context_window.unwrap_or(defaults.context_window),
            compaction: self.compaction.unwrap_or(defaults.compaction),
            cortex: self.cortex.unwrap_or(defaults.cortex),
            history_backfill_count: defaults.history_backfill_count,
        }
    }
}

impl ResolvedAgentConfig {
    pub fn sqlite_path(&self) -> PathBuf {
        self.data_dir.join("spacebot.db")
    }
    pub fn lancedb_path(&self) -> PathBuf {
        self.data_dir.join("lancedb")
    }
    pub fn redb_path(&self) -> PathBuf {
        self.data_dir.join("config.redb")
    }
    pub fn history_backfill_count(&self) -> usize {
        self.history_backfill_count
    }
}

/// Routes a messaging platform conversation to a specific agent.
#[derive(Debug, Clone)]
pub struct Binding {
    pub agent_id: String,
    pub channel: String,
    pub guild_id: Option<String>,
    pub chat_id: Option<String>,
    /// Discord channel IDs this binding applies to. If empty, all channels in the guild are allowed.
    pub channel_ids: Vec<String>,
}

impl Binding {
    /// Check if this binding matches an inbound message.
    fn matches(&self, message: &crate::InboundMessage) -> bool {
        if self.channel != message.source {
            return false;
        }

        if let Some(guild_id) = &self.guild_id {
            let message_guild = message
                .metadata
                .get("discord_guild_id")
                .and_then(|v| v.as_u64())
                .map(|v| v.to_string());
            if message_guild.as_deref() != Some(guild_id) {
                return false;
            }
        }

        if !self.channel_ids.is_empty() {
            let message_channel = message
                .metadata
                .get("discord_channel_id")
                .and_then(|v| v.as_u64())
                .map(|v| v.to_string());
            let parent_channel = message
                .metadata
                .get("discord_parent_channel_id")
                .and_then(|v| v.as_u64())
                .map(|v| v.to_string());

            let direct_match = message_channel
                .as_ref()
                .is_some_and(|id| self.channel_ids.contains(id));
            let parent_match = parent_channel
                .as_ref()
                .is_some_and(|id| self.channel_ids.contains(id));

            if !direct_match && !parent_match {
                return false;
            }
        }

        if let Some(chat_id) = &self.chat_id {
            let message_chat = message
                .metadata
                .get("telegram_chat_id")
                .and_then(|v| v.as_str());
            if message_chat != Some(chat_id) {
                return false;
            }
        }

        true
    }
}

/// Resolve which agent should handle an inbound message.
///
/// Checks bindings in order. First match wins. Falls back to the default
/// agent if no binding matches.
pub fn resolve_agent_for_message(
    bindings: &[Binding],
    message: &crate::InboundMessage,
    default_agent_id: &str,
) -> crate::AgentId {
    for binding in bindings {
        if binding.matches(message) {
            return std::sync::Arc::from(binding.agent_id.as_str());
        }
    }
    std::sync::Arc::from(default_agent_id)
}

/// Messaging platform credentials (instance-level).
#[derive(Debug, Clone, Default)]
pub struct MessagingConfig {
    pub discord: Option<DiscordConfig>,
    pub webhook: Option<WebhookConfig>,
}

#[derive(Debug, Clone)]
pub struct DiscordConfig {
    pub enabled: bool,
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct WebhookConfig {
    pub enabled: bool,
    pub port: u16,
    pub bind: String,
}

// -- TOML deserialization types --

#[derive(Deserialize)]
struct TomlConfig {
    #[serde(default)]
    llm: TomlLlmConfig,
    #[serde(default)]
    defaults: TomlDefaultsConfig,
    #[serde(default)]
    agents: Vec<TomlAgentConfig>,
    #[serde(default)]
    messaging: TomlMessagingConfig,
    #[serde(default)]
    bindings: Vec<TomlBinding>,
}

#[derive(Deserialize, Default)]
struct TomlLlmConfig {
    anthropic_key: Option<String>,
    openai_key: Option<String>,
    openrouter_key: Option<String>,
}

#[derive(Deserialize, Default)]
struct TomlDefaultsConfig {
    routing: Option<TomlRoutingConfig>,
    max_concurrent_branches: Option<usize>,
    max_turns: Option<usize>,
    context_window: Option<usize>,
    compaction: Option<TomlCompactionConfig>,
    cortex: Option<TomlCortexConfig>,
}

#[derive(Deserialize, Default)]
struct TomlRoutingConfig {
    channel: Option<String>,
    branch: Option<String>,
    worker: Option<String>,
    compactor: Option<String>,
    cortex: Option<String>,
    rate_limit_cooldown_secs: Option<u64>,
    #[serde(default)]
    task_overrides: HashMap<String, String>,
    #[serde(default)]
    fallbacks: HashMap<String, Vec<String>>,
}

#[derive(Deserialize)]
struct TomlCompactionConfig {
    background_threshold: Option<f32>,
    aggressive_threshold: Option<f32>,
    emergency_threshold: Option<f32>,
}

#[derive(Deserialize)]
struct TomlCortexConfig {
    tick_interval_secs: Option<u64>,
    worker_timeout_secs: Option<u64>,
    branch_timeout_secs: Option<u64>,
    circuit_breaker_threshold: Option<u8>,
}

#[derive(Deserialize)]
struct TomlAgentConfig {
    id: String,
    #[serde(default)]
    default: bool,
    workspace: Option<String>,
    routing: Option<TomlRoutingConfig>,
    max_concurrent_branches: Option<usize>,
    max_turns: Option<usize>,
    context_window: Option<usize>,
}

#[derive(Deserialize, Default)]
struct TomlMessagingConfig {
    discord: Option<TomlDiscordConfig>,
    webhook: Option<TomlWebhookConfig>,
}

#[derive(Deserialize)]
struct TomlDiscordConfig {
    #[serde(default)]
    enabled: bool,
    token: Option<String>,
}

#[derive(Deserialize)]
struct TomlWebhookConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_webhook_port")]
    port: u16,
    #[serde(default = "default_webhook_bind")]
    bind: String,
}

fn default_webhook_port() -> u16 {
    18789
}
fn default_webhook_bind() -> String {
    "127.0.0.1".into()
}

#[derive(Deserialize)]
struct TomlBinding {
    agent_id: String,
    channel: String,
    guild_id: Option<String>,
    chat_id: Option<String>,
    #[serde(default)]
    channel_ids: Vec<String>,
}

/// Resolve a value that might be an "env:VAR_NAME" reference.
fn resolve_env_value(value: &str) -> Option<String> {
    if let Some(var_name) = value.strip_prefix("env:") {
        std::env::var(var_name).ok()
    } else {
        Some(value.to_string())
    }
}

/// Resolve a TomlRoutingConfig against a base RoutingConfig.
fn resolve_routing(toml: Option<TomlRoutingConfig>, base: &RoutingConfig) -> RoutingConfig {
    let Some(t) = toml else { return base.clone() };

    let mut task_overrides = base.task_overrides.clone();
    task_overrides.extend(t.task_overrides);

    let mut fallbacks = base.fallbacks.clone();
    fallbacks.extend(t.fallbacks);

    RoutingConfig {
        channel: t.channel.unwrap_or_else(|| base.channel.clone()),
        branch: t.branch.unwrap_or_else(|| base.branch.clone()),
        worker: t.worker.unwrap_or_else(|| base.worker.clone()),
        compactor: t.compactor.unwrap_or_else(|| base.compactor.clone()),
        cortex: t.cortex.unwrap_or_else(|| base.cortex.clone()),
        task_overrides,
        fallbacks,
        rate_limit_cooldown_secs: t
            .rate_limit_cooldown_secs
            .unwrap_or(base.rate_limit_cooldown_secs),
    }
}

impl Config {
    /// Load configuration from the default config file, falling back to env vars.
    pub fn load() -> Result<Self> {
        let instance_dir = std::env::var("SPACEBOT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .map(|d| d.join(".spacebot"))
                    .unwrap_or_else(|| PathBuf::from("./.spacebot"))
            });

        let config_path = instance_dir.join("config.toml");
        if config_path.exists() {
            Self::load_from_path(&config_path)
        } else {
            Self::load_from_env(&instance_dir)
        }
    }

    /// Load from a specific TOML config file.
    pub fn load_from_path(path: &Path) -> Result<Self> {
        let instance_dir = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config from {}", path.display()))?;

        let toml_config: TomlConfig = toml::from_str(&content)
            .with_context(|| format!("failed to parse config from {}", path.display()))?;

        Self::from_toml(toml_config, instance_dir)
    }

    /// Load from environment variables only (no config file).
    pub fn load_from_env(instance_dir: &Path) -> Result<Self> {
        let llm = LlmConfig {
            anthropic_key: std::env::var("ANTHROPIC_API_KEY").ok(),
            openai_key: std::env::var("OPENAI_API_KEY").ok(),
            openrouter_key: std::env::var("OPENROUTER_API_KEY").ok(),
        };

        if llm.anthropic_key.is_none() && llm.openai_key.is_none() && llm.openrouter_key.is_none() {
            return Err(ConfigError::Invalid(
                "no LLM provider API key found — set ANTHROPIC_API_KEY, OPENAI_API_KEY, or OPENROUTER_API_KEY".into(),
            )
            .into());
        }

        // Env-only routing: check for env overrides on channel/worker models
        let mut routing = RoutingConfig::default();
        if let Ok(channel_model) = std::env::var("SPACEBOT_CHANNEL_MODEL") {
            routing.channel = channel_model;
        }
        if let Ok(worker_model) = std::env::var("SPACEBOT_WORKER_MODEL") {
            routing.worker = worker_model;
        }

        let agents = vec![AgentConfig {
            id: "main".into(),
            default: true,
            workspace: None,
            routing: Some(routing),
            max_concurrent_branches: None,
            max_turns: None,
            context_window: None,
            compaction: None,
            cortex: None,
        }];

        Ok(Self {
            instance_dir: instance_dir.to_path_buf(),
            llm,
            defaults: DefaultsConfig::default(),
            agents,
            messaging: MessagingConfig::default(),
            bindings: Vec::new(),
        })
    }

    fn from_toml(toml: TomlConfig, instance_dir: PathBuf) -> Result<Self> {
        let llm = LlmConfig {
            anthropic_key: toml
                .llm
                .anthropic_key
                .as_deref()
                .and_then(resolve_env_value)
                .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok()),
            openai_key: toml
                .llm
                .openai_key
                .as_deref()
                .and_then(resolve_env_value)
                .or_else(|| std::env::var("OPENAI_API_KEY").ok()),
            openrouter_key: toml
                .llm
                .openrouter_key
                .as_deref()
                .and_then(resolve_env_value)
                .or_else(|| std::env::var("OPENROUTER_API_KEY").ok()),
        };

        if llm.anthropic_key.is_none() && llm.openai_key.is_none() && llm.openrouter_key.is_none() {
            return Err(ConfigError::Invalid(
                "no LLM provider API key found — set ANTHROPIC_API_KEY, OPENAI_API_KEY, or OPENROUTER_API_KEY".into(),
            )
            .into());
        }

        let base_defaults = DefaultsConfig::default();
        let defaults = DefaultsConfig {
            routing: resolve_routing(toml.defaults.routing, &base_defaults.routing),
            max_concurrent_branches: toml
                .defaults
                .max_concurrent_branches
                .unwrap_or(base_defaults.max_concurrent_branches),
            max_turns: toml.defaults.max_turns.unwrap_or(base_defaults.max_turns),
            context_window: toml
                .defaults
                .context_window
                .unwrap_or(base_defaults.context_window),
            compaction: toml
                .defaults
                .compaction
                .map(|c| CompactionConfig {
                    background_threshold: c
                        .background_threshold
                        .unwrap_or(base_defaults.compaction.background_threshold),
                    aggressive_threshold: c
                        .aggressive_threshold
                        .unwrap_or(base_defaults.compaction.aggressive_threshold),
                    emergency_threshold: c
                        .emergency_threshold
                        .unwrap_or(base_defaults.compaction.emergency_threshold),
                })
                .unwrap_or(base_defaults.compaction),
            cortex: toml
                .defaults
                .cortex
                .map(|c| CortexConfig {
                    tick_interval_secs: c
                        .tick_interval_secs
                        .unwrap_or(base_defaults.cortex.tick_interval_secs),
                    worker_timeout_secs: c
                        .worker_timeout_secs
                        .unwrap_or(base_defaults.cortex.worker_timeout_secs),
                    branch_timeout_secs: c
                        .branch_timeout_secs
                        .unwrap_or(base_defaults.cortex.branch_timeout_secs),
                    circuit_breaker_threshold: c
                        .circuit_breaker_threshold
                        .unwrap_or(base_defaults.cortex.circuit_breaker_threshold),
                })
                .unwrap_or(base_defaults.cortex),
            history_backfill_count: base_defaults.history_backfill_count,
        };

        let mut agents: Vec<AgentConfig> = toml
            .agents
            .into_iter()
            .map(|a| {
                // Per-agent routing resolves against instance defaults
                let agent_routing = a
                    .routing
                    .map(|r| resolve_routing(Some(r), &defaults.routing));

                AgentConfig {
                    id: a.id,
                    default: a.default,
                    workspace: a.workspace.map(PathBuf::from),
                    routing: agent_routing,
                    max_concurrent_branches: a.max_concurrent_branches,
                    max_turns: a.max_turns,
                    context_window: a.context_window,
                    compaction: None,
                    cortex: None,
                }
            })
            .collect();

        if agents.is_empty() {
            agents.push(AgentConfig {
                id: "main".into(),
                default: true,
                workspace: None,
                routing: None,
                max_concurrent_branches: None,
                max_turns: None,
                context_window: None,
                compaction: None,
                cortex: None,
            });
        }

        if !agents.iter().any(|a| a.default) {
            if let Some(first) = agents.first_mut() {
                first.default = true;
            }
        }

        let messaging = MessagingConfig {
            discord: toml.messaging.discord.and_then(|d| {
                let token = d
                    .token
                    .as_deref()
                    .and_then(resolve_env_value)
                    .or_else(|| std::env::var("DISCORD_BOT_TOKEN").ok())?;
                Some(DiscordConfig {
                    enabled: d.enabled,
                    token,
                })
            }),
            webhook: toml.messaging.webhook.map(|w| WebhookConfig {
                enabled: w.enabled,
                port: w.port,
                bind: w.bind,
            }),
        };

        let bindings = toml
            .bindings
            .into_iter()
            .map(|b| Binding {
                agent_id: b.agent_id,
                channel: b.channel,
                guild_id: b.guild_id,
                chat_id: b.chat_id,
                channel_ids: b.channel_ids,
            })
            .collect();

        Ok(Config {
            instance_dir,
            llm,
            defaults,
            agents,
            messaging,
            bindings,
        })
    }

    /// Get the default agent ID.
    pub fn default_agent_id(&self) -> &str {
        self.agents
            .iter()
            .find(|a| a.default)
            .map(|a| a.id.as_str())
            .unwrap_or("main")
    }

    /// Resolve all agent configs against defaults.
    pub fn resolve_agents(&self) -> Vec<ResolvedAgentConfig> {
        self.agents
            .iter()
            .map(|a| a.resolve(&self.instance_dir, &self.defaults))
            .collect()
    }

    /// Path to shared prompts directory.
    pub fn prompts_dir(&self) -> PathBuf {
        self.instance_dir.join("prompts")
    }
}
