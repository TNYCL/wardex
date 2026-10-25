-- Optional MCP Events. Generic webhook configuration is unchanged.
CREATE TABLE mcp_subscriptions (
    id TEXT PRIMARY KEY NOT NULL,
    principal TEXT NOT NULL,
    name TEXT NOT NULL,
    arguments TEXT NOT NULL,
    callback TEXT NOT NULL,
    secret TEXT NOT NULL,
    previous_secret TEXT,
    rotate_until_ms INTEGER NOT NULL DEFAULT 0,
    expires_ms INTEGER NOT NULL,
    verified_ms INTEGER NOT NULL,
    active INTEGER NOT NULL DEFAULT 1,
    version INTEGER NOT NULL DEFAULT 1,
    created_ms INTEGER NOT NULL,
    last_error TEXT
);
CREATE TABLE mcp_event_journal (
    position INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT UNIQUE NOT NULL,
    name TEXT NOT NULL,
    occurred_ms INTEGER NOT NULL,
    data TEXT NOT NULL
);
CREATE INDEX mcp_event_journal_time ON mcp_event_journal (occurred_ms);
CREATE TABLE mcp_outbox (
    subscription_id TEXT NOT NULL REFERENCES mcp_subscriptions(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    event_id TEXT NOT NULL,
    body TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',
    attempts INTEGER NOT NULL DEFAULT 0,
    created_ms INTEGER NOT NULL,
    next_attempt_ms INTEGER NOT NULL,
    lease_until_ms INTEGER,
    lease_token TEXT,
    last_error TEXT,
    PRIMARY KEY (subscription_id, event_id)
) WITHOUT ROWID;
CREATE INDEX mcp_outbox_due ON mcp_outbox (state, next_attempt_ms);
