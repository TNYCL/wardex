-- Keep an unverifiable old scope quarantined across daemon restarts.
ALTER TABLE sessions ADD COLUMN cleanup_failed INTEGER NOT NULL DEFAULT 0;
