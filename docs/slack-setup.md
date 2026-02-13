# Slack Setup Guide

How to connect Spacebot to Slack via Socket Mode so it can receive messages and respond through the Slack adapter.

## Slack App Configuration

### 1. Create a Slack App

Go to https://api.slack.com/apps and click "Create New App". Choose "From scratch", name it, and select the workspace.

### 2. Enable Socket Mode

Under "Socket Mode" in the sidebar, toggle it on. This creates an **app-level token** (`xapp-...`). Copy it — you'll need this for `app_token` in the config.

Socket Mode means Spacebot connects to Slack over WebSocket. No public URL, no ngrok, no load balancer. The binary opens an outbound connection and receives events over it.

### 3. Configure Event Subscriptions

Under "Event Subscriptions", toggle on. Add these **bot events**:

- `message.channels` — messages in public channels the bot is in
- `message.groups` — messages in private channels the bot is in
- `message.im` — direct messages to the bot
- `message.mpim` — messages in group DMs the bot is in

### 4. Set Bot Token Scopes

Under "OAuth & Permissions", add these **Bot Token Scopes**:

| Scope | Why |
|-------|-----|
| `chat:write` | Send messages and edit streaming placeholders |
| `reactions:write` | Add/remove thinking indicator reaction |
| `reactions:read` | Read reactions (for future use) |
| `channels:history` | Fetch message history for context backfill |
| `groups:history` | Same, for private channels |
| `im:history` | Same, for DMs |
| `mpim:history` | Same, for group DMs |
| `users:read` | Resolve user display names |

### 5. Install to Workspace

Click "Install to Workspace" and authorize. Copy the **Bot User OAuth Token** (`xoxb-...`). This is your `bot_token`.

### 6. Invite the Bot

In Slack, invite the bot to channels where it should respond: `/invite @YourBot` in each channel.

## Spacebot Configuration

Add the following to your `config.toml`:

```toml
[messaging.slack]
enabled = true
bot_token = "env:SLACK_BOT_TOKEN"
app_token = "env:SLACK_APP_TOKEN"

[[bindings]]
agent_id = "main"
channel = "slack"
workspace_id = "T01234ABCDE"
```

Set the environment variables:

```bash
export SLACK_BOT_TOKEN="xoxb-your-bot-token"
export SLACK_APP_TOKEN="xapp-your-app-token"
```

### Finding Your Workspace ID

The workspace ID (team ID) is the `T`-prefixed string. You can find it in the Slack URL when using the web client (`https://app.slack.com/client/T01234ABCDE/...`) or via the `auth.test` API.

### Channel Filtering

Restrict the bot to specific channels within a workspace:

```toml
[[bindings]]
agent_id = "main"
channel = "slack"
workspace_id = "T01234ABCDE"
channel_ids = ["C0001", "C0002"]
```

If `channel_ids` is empty or omitted, the bot responds in all channels it's been invited to.

### DM Filtering

Restrict which users can DM the bot:

```toml
[messaging.slack]
enabled = true
bot_token = "env:SLACK_BOT_TOKEN"
app_token = "env:SLACK_APP_TOKEN"
dm_allowed_users = ["U01234", "U56789"]
```

If `dm_allowed_users` is empty, all DMs are ignored. This is the default.

### Multiple Workspaces

Route different Slack workspaces to different agents:

```toml
[[bindings]]
agent_id = "main"
channel = "slack"
workspace_id = "T01234ABCDE"

[[bindings]]
agent_id = "dev-bot"
channel = "slack"
workspace_id = "T99999ZZZZZ"
```

### Running Alongside Discord

Both adapters can run simultaneously. Bindings route messages to the right agent regardless of platform:

```toml
[messaging.discord]
enabled = true
token = "env:DISCORD_BOT_TOKEN"

[messaging.slack]
enabled = true
bot_token = "env:SLACK_BOT_TOKEN"
app_token = "env:SLACK_APP_TOKEN"

[[bindings]]
agent_id = "main"
channel = "discord"
guild_id = "123456789"

[[bindings]]
agent_id = "main"
channel = "slack"
workspace_id = "T01234ABCDE"
```

## How Messages Flow

```
User types in #general on Slack
    |
    v
Socket Mode WebSocket receives message event
    |
    v
Push event callback extracts SlackMessageEvent:
    - origin.channel -> slack_channel_id
    - origin.ts -> message timestamp
    - origin.thread_ts -> thread context (if threaded)
    - sender.user -> user ID
    - content.text -> message text
    - content.files -> attachments
    |
    v
Permission check (workspace, channel, DM filters)
    |
    v
Builds InboundMessage:
    - id: message timestamp (Slack's unique ID)
    - source: "slack"
    - conversation_id: "slack:TEAM_ID:CHANNEL_ID" (or ":THREAD_TS" suffix)
    - metadata: { slack_workspace_id, slack_channel_id, slack_message_ts, ... }
    |
    v
mpsc channel -> InboundStream -> MessagingManager
    |
    v
Router resolves binding -> sets agent_id
    |
    v
Channel processes -> OutboundResponse -> SlackAdapter.respond()
    |
    v
Slack Web API (chat.postMessage) -> User sees reply
```

### Conversation Mapping

| Slack Context | conversation_id | Spacebot Behavior |
|---------------|----------------|-------------------|
| Public channel `#general` | `slack:TEAM:CHANNEL` | One conversation per channel |
| Thread in `#general` | `slack:TEAM:CHANNEL:THREAD_TS` | One conversation per thread |
| DM with bot | `slack:TEAM:DM_CHANNEL` | One conversation per DM |

Threads are first-class. When a user replies in a thread, the thread gets its own Spacebot channel with isolated history. Top-level channel messages share one conversation.

### Typing Indicator

Slack doesn't have a bot typing API. Instead, the adapter adds a `:thinking_face:` reaction to the user's message when the agent starts processing, and removes it when a response is sent or a different status arrives. This is a visual cue without the flicker of a typing bubble.

### Message Length

Slack allows up to ~40k characters per message, but the adapter splits at 4000 chars for readability. Splits happen at newlines first, then spaces, then hard-cut.

### Streaming

When streaming is enabled, the adapter sends a zero-width space placeholder on `StreamStart`, then edits it in-place with accumulated text on each `StreamChunk`. On `StreamEnd`, the active message reference is cleaned up. Edits should be rate-limited to ~1/sec to respect Slack's API limits (the adapter currently sends on every chunk — a future improvement would add coalescing).

### Thread Replies

`OutboundResponse::ThreadReply` posts in a thread. If the source message was already in a thread, it replies there. If not, it starts a new thread from the source message. Falls back to a regular message if no thread context is available.

### Reactions

`OutboundResponse::Reaction` adds an emoji reaction to the source message. The emoji string is sanitized (stripped of colons, lowercased) to match Slack's reaction name format.

### History Backfill

When a new channel is created for a conversation, the adapter fetches up to 100 recent messages from Slack (via `conversations.history` or `conversations.replies` for threads) and injects them as context so the LLM has conversational continuity.

## Hot Reloading

Slack permissions (workspace filter, channel filter, DM allowed users) are hot-reloadable. Edit `config.toml` and the changes take effect within 2 seconds — no restart needed. The Socket Mode connection stays up.

Token changes require a restart.

## Differences from Discord Adapter

| Concern | Discord | Slack |
|---------|---------|-------|
| Connection | Gateway WebSocket (serenity) | Socket Mode WebSocket (slack-morphism) |
| Typing indicator | Native `broadcast_typing()` | `:thinking_face:` reaction |
| Message limit | 2000 chars | 4000 chars (practical) |
| Thread model | Created explicitly | Reply with `thread_ts` |
| Permissions | Guild/channel ID filters | Workspace/channel ID filters |
| Library | serenity 0.12 | slack-morphism 2.17 |
| Auth tokens | 1 bot token | 2 tokens (bot + app-level for Socket Mode) |
