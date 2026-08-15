-- The Daily EPUB — initial schema (spec §3.13).
-- All timestamps are stored as RFC3339 UTC strings (implementation notes §2).

-- Raw Miniflux entries, upserted by miniflux entry id (§3.1).
CREATE TABLE IF NOT EXISTS entries (
    id            INTEGER PRIMARY KEY, -- miniflux entry id
    feed_id       INTEGER NOT NULL,
    feed_title    TEXT,
    category      TEXT,
    title         TEXT,
    url           TEXT,
    canonical_url TEXT,
    author        TEXT,
    published_at  TEXT,
    comments_url  TEXT,
    raw_content   TEXT,
    fetched_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_entries_published_at ON entries (published_at);
CREATE INDEX IF NOT EXISTS idx_entries_feed_id ON entries (feed_id);
CREATE INDEX IF NOT EXISTS idx_entries_canonical_url ON entries (canonical_url);

-- Deduped article clusters (§3.2). One row per canonical url.
CREATE TABLE IF NOT EXISTS articles (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    canonical_url TEXT NOT NULL UNIQUE,
    title         TEXT,
    best_entry_id INTEGER REFERENCES entries (id),
    content_html  TEXT,
    word_count    INTEGER NOT NULL DEFAULT 0,
    excerpt_only  BOOLEAN NOT NULL DEFAULT 0,
    image_count   INTEGER NOT NULL DEFAULT 0,
    sources_json  TEXT NOT NULL DEFAULT '[]',
    first_seen    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_articles_first_seen ON articles (first_seen);
CREATE INDEX IF NOT EXISTS idx_articles_best_entry_id ON articles (best_entry_id);

-- Social proof cache (§3.4). Best-effort; refreshed by `backfill-social`.
CREATE TABLE IF NOT EXISTS social (
    article_id   INTEGER NOT NULL REFERENCES articles (id) ON DELETE CASCADE,
    source       TEXT NOT NULL CHECK (source IN ('hn', 'lobsters', 'reddit', 'x')),
    item_id      TEXT,
    score        INTEGER NOT NULL DEFAULT 0,
    num_comments INTEGER NOT NULL DEFAULT 0,
    item_url     TEXT,
    fetched_at   TEXT NOT NULL,
    PRIMARY KEY (article_id, source)
);

CREATE INDEX IF NOT EXISTS idx_social_fetched_at ON social (fetched_at);

-- Per-run scoring output (§3.5, §3.6).
CREATE TABLE IF NOT EXISTS scores (
    article_id      INTEGER NOT NULL REFERENCES articles (id) ON DELETE CASCADE,
    run_date        TEXT NOT NULL,
    prefilter_score REAL,
    llm_score       REAL,
    llm_category    TEXT,
    rationale       TEXT,
    PRIMARY KEY (article_id, run_date)
);

CREATE INDEX IF NOT EXISTS idx_scores_run_date ON scores (run_date);
CREATE INDEX IF NOT EXISTS idx_scores_llm_score ON scores (llm_score);

-- One published issue per date (§3.10, §3.11).
CREATE TABLE IF NOT EXISTS issues (
    date            TEXT PRIMARY KEY,
    issue_number    INTEGER NOT NULL,
    generated_at    TEXT NOT NULL,
    epub_path       TEXT,
    x4_path         TEXT,
    xtc_path        TEXT,
    front_page_html TEXT,
    report_json     TEXT
);

CREATE INDEX IF NOT EXISTS idx_issues_generated_at ON issues (generated_at);

-- The lineup: which articles landed in which issue/section (§3.6 stage B).
CREATE TABLE IF NOT EXISTS issue_articles (
    issue_date TEXT NOT NULL REFERENCES issues (date) ON DELETE CASCADE,
    article_id INTEGER NOT NULL REFERENCES articles (id) ON DELETE CASCADE,
    section    TEXT NOT NULL,
    position   INTEGER NOT NULL DEFAULT 0,
    is_lead    BOOLEAN NOT NULL DEFAULT 0,
    summary    TEXT,
    PRIMARY KEY (issue_date, article_id)
);

CREATE INDEX IF NOT EXISTS idx_issue_articles_article_id ON issue_articles (article_id);
CREATE INDEX IF NOT EXISTS idx_issue_articles_section ON issue_articles (issue_date, section, position);

-- 👍/👎 feedback collected by `serve` (§3.9).
CREATE TABLE IF NOT EXISTS ratings (
    issue_date TEXT NOT NULL,
    article_id INTEGER NOT NULL,
    vote       INTEGER NOT NULL CHECK (vote IN (-1, 1)),
    rated_at   TEXT NOT NULL,
    PRIMARY KEY (issue_date, article_id)
);

CREATE INDEX IF NOT EXISTS idx_ratings_rated_at ON ratings (rated_at);
CREATE INDEX IF NOT EXISTS idx_ratings_article_id ON ratings (article_id);

-- Beta-smoothed per-feed upvote rate used by the pre-filter (§3.9).
CREATE TABLE IF NOT EXISTS feed_priors (
    feed_id   INTEGER PRIMARY KEY,
    upvotes   INTEGER NOT NULL DEFAULT 0,
    downvotes INTEGER NOT NULL DEFAULT 0,
    included  INTEGER NOT NULL DEFAULT 0
);

-- One row per `generate` invocation; token/cost accounting (§3.6 guardrail).
CREATE TABLE IF NOT EXISTS runs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    date            TEXT NOT NULL,
    started_at      TEXT NOT NULL,
    finished_at     TEXT,
    entries_fetched INTEGER NOT NULL DEFAULT 0,
    candidates      INTEGER NOT NULL DEFAULT 0,
    selected        INTEGER NOT NULL DEFAULT 0,
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    cached_tokens   INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    cost_usd        REAL NOT NULL DEFAULT 0.0,
    status          TEXT NOT NULL DEFAULT 'running',
    error           TEXT
);

CREATE INDEX IF NOT EXISTS idx_runs_date ON runs (date);
CREATE INDEX IF NOT EXISTS idx_runs_started_at ON runs (started_at);

-- Misc singletons: ingest watermark, taste profile, profile version (§3.1, §3.6).
CREATE TABLE IF NOT EXISTS kv (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
