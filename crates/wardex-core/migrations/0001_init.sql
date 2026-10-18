CREATE TABLE sessions (
    id                TEXT PRIMARY KEY NOT NULL,
    driver            TEXT NOT NULL,
    driver_session_id TEXT,
    state             TEXT NOT NULL,
    agent             TEXT,
    task              TEXT,
    model             TEXT,
    reasoning         TEXT,
    cwd               TEXT NOT NULL,
    origin_cwd        TEXT,
    worktree          TEXT,
    branch            TEXT,
    remove_worktree   INTEGER NOT NULL DEFAULT 0,
    readonly          INTEGER NOT NULL DEFAULT 0,
    slice             TEXT,
    unit              TEXT,
    pid               INTEGER,
    created_ms        INTEGER NOT NULL,
    updated_ms        INTEGER NOT NULL,
    ended_ms          INTEGER,
    last_seq          INTEGER NOT NULL DEFAULT 0,
    last_result       TEXT,
    last_error        TEXT,
    spawn_count       INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX sessions_state ON sessions (state);
CREATE INDEX sessions_created ON sessions (created_ms);

CREATE TABLE events (
    session_id TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    ts_ms      INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    body       TEXT NOT NULL,
    PRIMARY KEY (session_id, seq)
) WITHOUT ROWID;

CREATE INDEX events_ts ON events (ts_ms);

CREATE TABLE questions (
    session_id  TEXT NOT NULL,
    question_id TEXT NOT NULL,
    kind        TEXT NOT NULL,
    body        TEXT NOT NULL,
    asked_ms    INTEGER NOT NULL,
    answered_ms INTEGER,
    answer      TEXT,
    cancelled   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, question_id)
);

CREATE INDEX questions_open ON questions (answered_ms, cancelled);
