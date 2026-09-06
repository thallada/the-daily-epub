CREATE TABLE rating_imports (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    url          TEXT NOT NULL,
    label        TEXT NOT NULL CHECK (label IN ('loved', 'good', 'not_for_me')),
    note         TEXT,
    status       TEXT NOT NULL CHECK (status IN ('pending', 'ok', 'failed')),
    message      TEXT,
    article_id   INTEGER REFERENCES articles(id) ON DELETE SET NULL,
    requested_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    requested_at TEXT NOT NULL,
    finished_at  TEXT
);

CREATE INDEX idx_rating_imports_status_id ON rating_imports(status, id);
