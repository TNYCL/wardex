-- Where a session came from: 'wardex' (started by Wardex), 'adopted' (an
-- external CLI session linked as-is) or 'forked' (a fork of one).
ALTER TABLE sessions ADD COLUMN origin TEXT NOT NULL DEFAULT 'wardex';
-- Native id of the external session this one was forked from.
ALTER TABLE sessions ADD COLUMN forked_from TEXT;
-- When the session was adopted (NULL for Wardex-started sessions).
ALTER TABLE sessions ADD COLUMN adopted_ms INTEGER;
-- Not unique: older databases may hold duplicates. `adopt` keeps one Wardex
-- session per native id.
CREATE INDEX sessions_driver_session ON sessions (driver, driver_session_id);
