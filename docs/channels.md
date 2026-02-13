# Channels

Every conversation happens in a channel. A Discord text channel, a Slack thread, a webhook endpoint, a heartbeat execution. Channels are created on demand when the first message arrives and tracked in SQLite so the system knows what exists, what's active, and what to show in a UI.

## Channel IDs

Channel IDs are colon-delimited strings with the platform as the first segment:

| Platform | Format | Example |
|----------|--------|---------|
| Discord (guild) | `discord:{guild_id}:{channel_id}` | `discord:1323900500600422472:1471388652562284626` |
| Discord (DM) | `discord:dm:{user_id}` | `discord:dm:302457623847329792` |
| Slack | `slack:{team_id}:{channel_id}` | `slack:T01ABC:C02DEF` |
| Slack (thread) | `slack:{team_id}:{channel_id}:{thread_ts}` | `slack:T01ABC:C02DEF:1234567890.123456` |
| Heartbeat | `heartbeat:{heartbeat_id}` | `heartbeat:daily-summary` |
| Webhook | `webhook:{endpoint}` | `webhook:github-ci` |

The ID is the primary key in the `channels` table and is used everywhere internally as the `ChannelId` type (`Arc<str>`).

## Lifecycle

Channels are lazy. There's no "create channel" step — the first message to a conversation ID creates both the runtime `Channel` struct and the database row.

```
Message arrives from Discord
  → Binding resolver picks the agent
  → main.rs checks active_channels HashMap
  → Not found: create Channel, spawn event loop
  → Channel.handle_message() runs
  → ChannelStore.upsert() fires (fire-and-forget)
  → channels table now has the row
```

On subsequent messages, the upsert runs again. It updates `display_name` and `platform_meta` if the metadata changed (e.g. a Discord channel rename) and bumps `last_activity_at`. The `COALESCE` in the upsert preserves existing values when the new message doesn't carry metadata — heartbeat messages have empty metadata, so they won't wipe out a display name set by an earlier Discord message.

Channels are never deleted. The `is_active` flag exists for soft archival in the future.

## Schema

```sql
CREATE TABLE channels (
    id TEXT PRIMARY KEY,
    platform TEXT NOT NULL,
    display_name TEXT,
    platform_meta TEXT,
    bulletin TEXT,
    permissions TEXT,
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_activity_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
```

| Column | Type | Description |
|--------|------|-------------|
| `id` | TEXT PK | The channel ID string (e.g. `discord:123:456`) |
| `platform` | TEXT | Extracted from the ID prefix: `discord`, `slack`, `heartbeat`, `webhook` |
| `display_name` | TEXT | Human-readable name from platform metadata (e.g. `#general`) |
| `platform_meta` | TEXT (JSON) | Platform-specific metadata blob |
| `bulletin` | TEXT | Reserved for per-channel memory bulletins |
| `permissions` | TEXT (JSON) | Reserved for per-channel permission overrides |
| `is_active` | INTEGER | 1 = active, 0 = archived |
| `created_at` | TIMESTAMP | When the channel was first seen |
| `last_activity_at` | TIMESTAMP | Updated on every user message |

## Platform Metadata

The `platform_meta` column stores a JSON blob of platform-specific fields extracted from message metadata. What gets stored depends on the platform:

**Discord:**
```json
{
  "discord_guild_id": "1323900500600422472",
  "discord_guild_name": "My Server",
  "discord_channel_id": "1471388652562284626",
  "discord_is_thread": false,
  "discord_parent_channel_id": null
}
```

**Slack:**
```json
{
  "slack_workspace_id": "T01ABC",
  "slack_channel_id": "C02DEF",
  "slack_thread_ts": "1234567890.123456"
}
```

**Heartbeat / Webhook:** No metadata stored (empty JSON or null).

## ChannelStore

`ChannelStore` is the interface to the `channels` table. It's constructed from a `SqlitePool` and lives on `ChannelState` (available to channel tools and branches).

All write operations are fire-and-forget — they spawn a tokio task and return immediately.

### Methods

| Method | Blocking | Description |
|--------|----------|-------------|
| `upsert(channel_id, metadata)` | No | Insert or update a channel. Extracts platform, display name, and platform meta from the message metadata. |
| `touch(channel_id)` | No | Bump `last_activity_at` without changing anything else. |
| `list_active()` | Yes (async) | All channels where `is_active = 1`, ordered by `last_activity_at` DESC. |
| `find_by_name(name)` | Yes (async) | Fuzzy match: exact name > prefix > contains > channel ID contains. Returns the best match. |
| `get(channel_id)` | Yes (async) | Exact ID lookup. |
| `resolve_name(channel_id)` | Yes (async) | Convenience — returns just the `display_name` for a channel ID. |

### Display Name Resolution

`find_by_name` powers the `channel_recall` tool's channel lookup. When a branch asks to recall from "general", the match priority is:

1. Exact match on `display_name` (case-insensitive)
2. Prefix match (e.g. "gen" matches "general")
3. Contains match (e.g. "ener" matches "general")
4. Substring match on the raw channel ID (e.g. "1471388" matches the Discord channel)

## Where It's Used

**`channel.rs`** — Every user message triggers `channel_store.upsert()` with the message's conversation ID and metadata. This keeps the channel row fresh.

**`channel_recall` tool** — Branches use `ChannelStore` to list available channels and resolve channel names when recalling transcripts from other conversations.

**`create_branch_tool_server`** — Each branch gets a `ChannelStore` reference so the `channel_recall` tool can query channels.

## Reserved Columns

Two columns exist in the schema but aren't populated yet:

**`bulletin`** — Per-channel memory bulletins. Currently the cortex generates a single agent-wide bulletin stored in `RuntimeConfig::memory_bulletin` via `ArcSwap`. The `bulletin` column is the hook for generating channel-specific bulletins that incorporate conversation-local context alongside the global memory graph.

**`permissions`** — Per-channel permission overrides as JSON. Intended for the UI layer — controlling which users can interact with the agent in specific channels, rate limits, tool restrictions, etc.

## Implementation

- `src/conversation/channels.rs` — `ChannelStore`, `ChannelInfo`, platform metadata extraction
- `src/agent/channel.rs` — `ChannelState` holds `ChannelStore`, upsert on each message
- `src/tools/channel_recall.rs` — uses `ChannelStore` for channel lookups
- `migrations/20260213000001_channels.sql` — table and indexes
