# Compaction

LLM context windows are finite. A long conversation fills up. In OpenClaw, when context hits a threshold, the session freezes for 20 seconds while it summarizes and compacts. The user stares at a typing indicator wondering if the bot died. Spacebot never blocks.

## How It Works

The compactor is a programmatic monitor — not an LLM process. It watches a channel's context size (estimated token count) and triggers compaction workers in the background. The channel keeps responding to messages the entire time.

Every turn, after the channel's LLM call completes, the compactor checks context usage:

```
estimated_tokens / context_window = usage ratio
```

Token estimation uses a `chars / 4` heuristic across all message content (text, tool calls, tool results). It's intentionally rough — we only need to know "are we getting close?" not "exactly how many tokens." Overestimating is the safe direction.

## Thresholds

Three tiers, configurable per agent:

| Threshold | Default | Action |
|-----------|---------|--------|
| Background | 80% | Compact oldest 30% of messages |
| Aggressive | 85% | Compact oldest 50% of messages |
| Emergency | 95% | Drop oldest 50%, no LLM |

```toml
[defaults.compaction]
background_threshold = 0.80
aggressive_threshold = 0.85
emergency_threshold = 0.95
```

Only one compaction runs at a time per channel. If context is already being compacted and a new threshold is hit, it's ignored until the current compaction finishes.

## Background and Aggressive Compaction

These are the normal path. A compaction worker runs in `tokio::spawn` alongside the channel:

1. **Drain** — Write-lock the channel's history, remove the oldest N messages (30% for background, 50% for aggressive). Release the lock. The channel can immediately continue with the remaining history.

2. **Archive** — Serialize the removed messages as JSON and write them to the `conversation_archives` table. This is the raw audit trail — every message that was ever compacted can be recovered.

3. **Summarize** — Build a transcript from the removed messages and run a Rig agent with `prompts/COMPACTOR.md` as the system prompt. The agent produces a condensed summary preserving key decisions, active topics, commitments, and emotional context. It discards greetings, tool call mechanics, and intermediate reasoning.

4. **Extract memories** — The compaction agent has access to the `memory_save` tool. While summarizing, it identifies facts, preferences, decisions, and observations worth keeping long-term and saves them directly to the memory store. These persist independently of the conversation.

5. **Inject summary** — Write-lock the history again, insert the summary at position 0 as `[Compaction Summary]: ...`. Release the lock. The channel sees this summary on its next turn.

6. **Persist** — Save the summary to the `compaction_summaries` table in SQLite.

The compaction agent runs with `max_turns(3)` — enough for the LLM to produce the summary and call `memory_save` a few times for extracted memories.

## Emergency Truncation

At 95% context usage, there's no time for an LLM call. Emergency truncation is synchronous:

1. Write-lock history
2. Remove oldest 50% of messages
3. Archive them to `conversation_archives`
4. Insert a marker: `[System: N older messages were truncated due to context limits]`
5. Release lock

This should rarely fire. If it does, it means the background/aggressive compaction didn't keep up — either the thresholds are too high, or the conversation is extremely fast-paced.

## Summaries Stack

Compaction summaries accumulate at the top of the context window. A long-running conversation might have several:

```
[Compaction Summary]: Earlier today, discussed project architecture...
[Compaction Summary]: Moved on to auth implementation, decided on JWT...
[Recent conversation messages]
```

This gives the channel rolling awareness of what happened without carrying the full raw history. Each summary covers the messages it replaced.

## What the Compaction LLM Sees

The compaction agent receives a rendered transcript of the removed messages. User messages, assistant responses, tool calls, and tool results — all formatted as readable text. The agent's system prompt (`prompts/COMPACTOR.md`) tells it to:

**Preserve:** Key decisions, active topics, commitments, emotional context, active workers/tasks.

**Discard:** Greetings, small talk, tool call details (results matter, not mechanics), intermediate reasoning, repeated information.

**Extract as memories:** Facts, preferences, decisions, observations — anything that should outlive the conversation.

## Storage

Two SQLite tables:

**`compaction_summaries`** — The summaries that get injected into channel context. Each has a `channel_id`, the summary text, how many turns it replaced, and a timestamp. Loaded when resuming a channel across restarts.

**`conversation_archives`** — Raw JSON of every message removed by compaction. Append-only audit trail. Never loaded at runtime — only useful for debugging or if you need to reconstruct what happened.

## Configuration

Thresholds are set in `config.toml` at the defaults level and can be overridden per agent:

```toml
[defaults.compaction]
background_threshold = 0.80
aggressive_threshold = 0.85
emergency_threshold = 0.95

# An agent with a smaller context window might want tighter thresholds
[[agents]]
id = "small-model-bot"

[agents.compaction]
background_threshold = 0.70
aggressive_threshold = 0.75
emergency_threshold = 0.90
```

The `context_window` setting (default 128,000 tokens) determines the denominator for usage calculation. Set this to match your model's actual context window.

## What OpenClaw Does Differently

| Concern | OpenClaw | Spacebot |
|---------|----------|----------|
| When it runs | Blocks the session | Background tokio task |
| User experience | Typing indicator, 20s freeze | No interruption |
| Summarization | Same session's LLM | Dedicated compaction worker |
| Memory extraction | Separate pass | Same LLM call as summarization |
| Raw transcript | Lost | Archived in `conversation_archives` |
| Multiple summaries | One summary replaces all | Summaries stack chronologically |
| Emergency fallback | None (just hope it fits) | Hard truncation at 95% |

## Implementation

- `src/agent/compactor.rs` — The `Compactor` struct, threshold checking, token estimation, compaction worker spawning, emergency truncation
- `src/conversation/history.rs` — `ConversationLogger` methods for summary CRUD and transcript archiving
- `src/agent/channel.rs` — Channel owns a `Compactor`, calls `check_and_compact()` after each turn
- `prompts/COMPACTOR.md` — System prompt for the compaction LLM
- `migrations/20260211000004_compaction.sql` — Schema for `compaction_summaries` and `conversation_archives`
