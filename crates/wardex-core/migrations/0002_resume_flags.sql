-- Set when a session is orphaned (daemon stop or crash); cleared on resume.
ALTER TABLE sessions ADD COLUMN resume_pending INTEGER NOT NULL DEFAULT 0;
-- The session was mid-turn when orphaned: an automatic resume sends
-- `sessions.resume_prompt` so the agent continues.
ALTER TABLE sessions ADD COLUMN resume_with_prompt INTEGER NOT NULL DEFAULT 0;
