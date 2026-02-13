# Quick Start

Get Spacebot running from scratch.

## Prerequisites

- Rust toolchain (1.85+, edition 2024)
- An [OpenRouter](https://openrouter.ai) API key (or Anthropic/OpenAI key directly)
- Optional: a Discord bot token if you want to connect to Discord

## Build

```bash
git clone <repo-url> && cd spacebot
cargo build --release
```

The binary is at `target/release/spacebot`.

## Configure

Spacebot looks for config at `~/.spacebot/config.toml` (or `$SPACEBOT_DIR/config.toml`). Create the directory and config file:

```bash
mkdir -p ~/.spacebot
```

### Minimal config (OpenRouter)

```toml
[llm]
openrouter_key = "env:OPENROUTER_API_KEY"

[defaults.routing]
channel = "openrouter/anthropic/claude-sonnet-4-20250514"
worker = "openrouter/anthropic/claude-haiku-4.5-20250514"

[[agents]]
id = "main"
default = true
```

### Minimal config (Anthropic direct)

```toml
[llm]
anthropic_key = "env:ANTHROPIC_API_KEY"

[defaults.routing]
channel = "anthropic/claude-sonnet-4-20250514"
worker = "anthropic/claude-haiku-4.5-20250514"

[[agents]]
id = "main"
default = true
```

Set your API key:

```bash
# pick one
export OPENROUTER_API_KEY="sk-or-..."
export ANTHROPIC_API_KEY="sk-ant-..."
```

## LLM Providers

Three providers are supported. Model names include the provider as a prefix:

| Provider | Config key | Env var | Example model |
|----------|-----------|---------|---------------|
| OpenRouter | `openrouter_key` | `OPENROUTER_API_KEY` | `openrouter/anthropic/claude-sonnet-4-20250514` |
| Anthropic | `anthropic_key` | `ANTHROPIC_API_KEY` | `anthropic/claude-sonnet-4-20250514` |
| OpenAI | `openai_key` | `OPENAI_API_KEY` | `openai/gpt-4o` |

OpenRouter gives you access to all models through one key. Model names for OpenRouter use the format `openrouter/<provider>/<model>` — the part after `openrouter/` is the model ID as listed on openrouter.ai.

You can mix providers across process types:

```toml
[llm]
openrouter_key = "env:OPENROUTER_API_KEY"
anthropic_key = "env:ANTHROPIC_API_KEY"

[defaults.routing]
channel = "anthropic/claude-sonnet-4-20250514"
worker = "openrouter/anthropic/claude-haiku-4.5-20250514"
```

## Model Routing

Different process types use different models. Channels (user-facing) get the best model. Workers (task execution) can use something cheaper.

```toml
[defaults.routing]
channel = "openrouter/anthropic/claude-sonnet-4-20250514"    # user conversations
branch = "openrouter/anthropic/claude-sonnet-4-20250514"     # thinking forks
worker = "openrouter/anthropic/claude-haiku-4.5-20250514"    # task execution
compactor = "openrouter/anthropic/claude-haiku-4.5-20250514" # summarization
cortex = "openrouter/anthropic/claude-haiku-4.5-20250514"    # system observation
```

Task-type overrides let specific worker tasks use different models:

```toml
[defaults.routing.task_overrides]
coding = "openrouter/anthropic/claude-sonnet-4-20250514"
deep_reasoning = "openrouter/anthropic/claude-opus-4-20250514"
```

Fallback chains handle rate limits and outages:

```toml
[defaults.routing.fallbacks]
"openrouter/anthropic/claude-sonnet-4-20250514" = ["openrouter/anthropic/claude-haiku-4.5-20250514"]
```

## Run

```bash
spacebot
```

This starts Spacebot as a background daemon. Use `--foreground` to keep it attached to the terminal:

```bash
spacebot start --foreground
```

Other commands:

```bash
spacebot stop               # graceful shutdown
spacebot restart             # stop + start
spacebot status              # show pid and uptime
```

With a specific config path or debug logging:

```bash
spacebot --config /path/to/config.toml
spacebot --debug
spacebot start -f -d        # foreground + debug (useful during development)
```

See [docs/daemon.md](daemon.md) for details on background operation, logging, and the IPC protocol.

On first run, Spacebot creates the agent directory structure:

```
~/.spacebot/
├── config.toml
├── spacebot.pid              # daemon PID (when running in background)
├── spacebot.sock             # IPC socket (when running in background)
├── logs/                     # daemon logs (daily rotation)
│   └── spacebot.log
├── agents/
│   └── main/
│       ├── workspace/
│       │   ├── SOUL.md        # personality and values
│       │   ├── IDENTITY.md    # name and nature
│       │   └── USER.md        # info about the human
│       ├── data/
│       │   ├── spacebot.db    # SQLite (memories, conversations)
│       │   ├── lancedb/       # vector search + embeddings
│       │   └── config.redb    # agent settings
│       └── archives/          # compaction transcripts
└── prompts/
    ├── CHANNEL.md
    ├── BRANCH.md
    ├── WORKER.md
    ├── COMPACTOR.md
    └── CORTEX.md
```

Edit the identity files in `agents/main/workspace/` to give your agent a personality. These are injected into the system prompt.

## Connect to Discord

See [docs/discord-setup.md](discord-setup.md) for full Discord setup. The short version:

1. Create a bot at https://discord.com/developers/applications
2. Enable **Message Content Intent** under Privileged Gateway Intents
3. Invite the bot to your server
4. Add to your config:

```toml
[messaging.discord]
enabled = true
token = "env:DISCORD_BOT_TOKEN"

[[bindings]]
agent_id = "main"
channel = "discord"
guild_id = "YOUR_GUILD_ID"
```

```bash
export DISCORD_BOT_TOKEN="your-token"
```

## Multiple Agents

Run multiple agents with different identities on a single instance. Each gets isolated memory, conversations, and identity files.

```toml
[[agents]]
id = "main"
default = true

[[agents]]
id = "dev-bot"
[agents.routing]
channel = "openrouter/anthropic/claude-sonnet-4-20250514"

# Route different Discord servers to different agents
[[bindings]]
agent_id = "main"
channel = "discord"
guild_id = "111111111"

[[bindings]]
agent_id = "dev-bot"
channel = "discord"
guild_id = "222222222"
```

Per-agent config overrides merge with instance defaults. See [docs/agents.md](agents.md) for the full agent system.

## Architecture at a Glance

Spacebot replaces the monolithic "one LLM thread does everything" model with five specialized process types:

| Process | Role | Blocks the user? |
|---------|------|-------------------|
| **Channel** | User-facing conversation. Has personality. Delegates everything. | Never |
| **Branch** | Fork of channel context that goes off to think. Returns a conclusion. | No |
| **Worker** | Independent task executor. Gets a job, does the job, reports status. | No |
| **Compactor** | Monitors context size. Triggers summarization before the channel fills up. | No |
| **Cortex** | System-level observer. Consolidates memories, manages decay. | No |

The channel is always responsive. It never waits on branches, workers, or compaction. When it needs to think, it forks a branch. When it needs work done, it spawns a worker. When context gets full, the compactor handles it in the background.

## Current Status

Spacebot is in active development. The foundation is built (config, databases, memory system, LLM routing, Discord adapter, tools, hooks). The orchestration layer (message routing, channel lifecycle, real LLM calls) is the current focus. See [docs/roadmap.md](roadmap.md) for detailed progress.

## Reference Docs

- [docs/roadmap.md](roadmap.md) — build phases and progress
- [docs/daemon.md](daemon.md) — background operation, logging, IPC
- [docs/discord-setup.md](discord-setup.md) — Discord bot setup
- [docs/agents.md](agents.md) — multi-agent system
- [docs/routing.md](routing.md) — model routing and fallbacks
- [docs/messaging.md](messaging.md) — messaging adapter architecture
- [docs/memory.md](memory.md) — memory system design
- [docs/philosophy.md](philosophy.md) — why Rust
