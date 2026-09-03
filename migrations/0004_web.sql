CREATE TABLE users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT NOT NULL,
    role          TEXT NOT NULL CHECK (role IN ('user', 'admin')),
    disabled      INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL,
    last_login_at TEXT
);

CREATE TABLE sessions (
    id         TEXT PRIMARY KEY,
    data       TEXT NOT NULL,
    expiry     INTEGER NOT NULL,
    user_id    INTEGER REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX idx_sessions_user ON sessions(user_id);
CREATE INDEX idx_sessions_expiry ON sessions(expiry);

ALTER TABLE rating_events ADD COLUMN user_id INTEGER REFERENCES users(id) ON DELETE SET NULL;

CREATE TABLE config_changes (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    INTEGER REFERENCES users(id) ON DELETE SET NULL,
    key        TEXT NOT NULL,
    old_value  TEXT,
    new_value  TEXT,
    changed_at TEXT NOT NULL
);
CREATE INDEX idx_config_changes_at ON config_changes(changed_at);

CREATE TABLE profile_versions (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    content  TEXT NOT NULL,
    saved_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    saved_at TEXT NOT NULL
);

CREATE TABLE jobs (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    name         TEXT NOT NULL,
    unit         TEXT NOT NULL,
    requested_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    requested_at TEXT NOT NULL,
    started_at   TEXT,
    finished_at  TEXT,
    status       TEXT NOT NULL CHECK (status IN ('requested', 'running', 'ok', 'failed')),
    message      TEXT,
    run_id       INTEGER REFERENCES runs(id) ON DELETE SET NULL
);
CREATE INDEX idx_jobs_requested_at ON jobs(requested_at);

ALTER TABLE runs ADD COLUMN report_json TEXT;
ALTER TABLE issues ADD COLUMN issue_json TEXT;

CREATE INDEX idx_candidate_runs_article_run ON candidate_runs(article_id, run_id DESC);
