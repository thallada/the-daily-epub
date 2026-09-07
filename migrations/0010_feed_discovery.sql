-- Feed discovery: proposed Miniflux subscriptions found behind aggregator-only
-- articles (feed discovery plan §4 step 1).

CREATE TABLE feed_candidates (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    feed_url         TEXT NOT NULL UNIQUE,
    host             TEXT NOT NULL,
    title            TEXT,
    status           TEXT NOT NULL CHECK (status IN ('candidate', 'added', 'dismissed')),
    first_seen       TEXT NOT NULL,
    last_seen        TEXT NOT NULL,
    miniflux_feed_id INTEGER,
    decided_at       TEXT
);
CREATE INDEX idx_feed_candidates_status_host ON feed_candidates(status, host);

CREATE TABLE feed_candidate_articles (
    candidate_id INTEGER NOT NULL REFERENCES feed_candidates(id) ON DELETE CASCADE,
    article_id   INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    PRIMARY KEY (candidate_id, article_id)
);

CREATE TABLE feed_discovery_hosts (
    host       TEXT PRIMARY KEY,
    checked_at TEXT NOT NULL,
    candidates INTEGER NOT NULL
);
