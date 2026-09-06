CREATE TABLE account_requests (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    email        TEXT NOT NULL COLLATE NOCASE,
    reason       TEXT,
    status       TEXT NOT NULL CHECK (status IN ('open', 'done')) DEFAULT 'open',
    requested_at TEXT NOT NULL,
    handled_at   TEXT,
    handled_by   INTEGER REFERENCES users(id) ON DELETE SET NULL
);

CREATE UNIQUE INDEX idx_account_requests_open_email
    ON account_requests(email) WHERE status = 'open';
