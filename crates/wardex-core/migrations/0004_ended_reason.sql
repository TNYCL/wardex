-- Why a session's process ended (done, stopped, idle_shutdown, idle_evicted,
-- memory_pressure, error, drain, shutdown, daemon_lost). NULL while live and
-- for sessions that ended before this column existed.
ALTER TABLE sessions ADD COLUMN ended_reason TEXT;
