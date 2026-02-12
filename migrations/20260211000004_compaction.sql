-- Compaction summaries that stack at the top of channel context
CREATE TABLE IF NOT EXISTS compaction_summaries (
    id TEXT PRIMARY KEY,
    channel_id TEXT NOT NULL,
    summary TEXT NOT NULL,
    turns_covered INTEGER NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_compaction_channel ON compaction_summaries(channel_id);
CREATE INDEX IF NOT EXISTS idx_compaction_channel_time ON compaction_summaries(channel_id, created_at);

-- Raw transcripts archived before compaction (audit trail)
CREATE TABLE IF NOT EXISTS conversation_archives (
    id TEXT PRIMARY KEY,
    channel_id TEXT NOT NULL,
    transcript TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_archives_channel ON conversation_archives(channel_id);
