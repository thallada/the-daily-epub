CREATE TABLE interests (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT NOT NULL COLLATE NOCASE UNIQUE,
    category       TEXT,                      -- NULL until categorized
    created_at     TEXT NOT NULL,
    categorized_at TEXT
);
CREATE INDEX idx_interests_category ON interests(category);

CREATE TABLE article_interests (
    article_id  INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    interest_id INTEGER NOT NULL REFERENCES interests(id) ON DELETE CASCADE,
    cos         REAL NOT NULL,
    z           REAL NOT NULL,
    run_id      INTEGER,                      -- NULL for backfilled rows
    PRIMARY KEY (article_id, interest_id)
);
CREATE INDEX idx_article_interests_interest ON article_interests(interest_id, cos DESC);
