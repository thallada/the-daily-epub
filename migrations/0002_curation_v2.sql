-- Personalized curation v2 schema (plan 2026-09-02 §7).
-- `scores` intentionally remains until migration 0003 (step 4).

CREATE TABLE rating_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    article_id  INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    issue_date  TEXT,
    kind        TEXT NOT NULL CHECK (kind IN ('explicit', 'implicit')),
    source      TEXT NOT NULL,
    label       TEXT NOT NULL,
    value       REAL NOT NULL,
    note        TEXT,
    event_at    TEXT NOT NULL
);

CREATE INDEX idx_rating_events_article ON rating_events(article_id, event_at);
CREATE INDEX idx_rating_events_at ON rating_events(event_at);

-- Filled in by step 3.
CREATE TABLE article_embeddings (
    article_id  INTEGER PRIMARY KEY REFERENCES articles(id) ON DELETE CASCADE,
    model       TEXT NOT NULL,
    dimension   INTEGER NOT NULL,
    input_hash  TEXT NOT NULL,
    embedding   BLOB NOT NULL,
    created_at  TEXT NOT NULL
);

-- Filled in by step 3.
CREATE TABLE interest_embeddings (
    interest    TEXT PRIMARY KEY,
    model       TEXT NOT NULL,
    dimension   INTEGER NOT NULL,
    embedding   BLOB NOT NULL,
    created_at  TEXT NOT NULL
);

-- Triage rows are filled in by step 4; deep rows by step 5.
CREATE TABLE article_assessments (
    article_id      INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    stage           TEXT NOT NULL CHECK (stage IN ('triage', 'deep')),
    model           TEXT NOT NULL,
    prompt_version  INTEGER NOT NULL,
    profile_version INTEGER,
    score           REAL,
    fit             REAL,
    kind            TEXT,
    facets_json     TEXT,
    rationale       TEXT,
    category        TEXT,
    paywalled_guess INTEGER NOT NULL DEFAULT 0,
    assessed_at     TEXT NOT NULL,
    PRIMARY KEY (article_id, stage)
);

CREATE INDEX idx_article_assessments_at ON article_assessments(assessed_at);

-- Filled in by step 3.
CREATE TABLE candidate_runs (
    run_id          INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    article_id      INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    stage           TEXT NOT NULL,
    excluded_reason TEXT,
    admitted_by     TEXT,
    signals_json    TEXT NOT NULL,
    utility         REAL,
    rank_utility    INTEGER,
    cluster_id      INTEGER,
    cluster_rank    INTEGER,
    editor_why      TEXT,
    PRIMARY KEY (run_id, article_id)
);

CREATE INDEX idx_candidate_runs_article ON candidate_runs(article_id);
CREATE INDEX idx_candidate_runs_run_stage ON candidate_runs(run_id, stage);

-- Filled in by later telemetry/provider steps.
ALTER TABLE runs ADD COLUMN config_json TEXT;
ALTER TABLE runs ADD COLUMN provider_costs_json TEXT;
-- Filled in by step 2.
ALTER TABLE issue_articles ADD COLUMN why TEXT;

INSERT INTO rating_events
    (article_id, issue_date, kind, source, label, value, note, event_at)
SELECT article_id,
       issue_date,
       'explicit',
       'migration',
       CASE vote WHEN 1 THEN 'loved' ELSE 'not_for_me' END,
       CASE vote WHEN 1 THEN 1.0 ELSE -1.0 END,
       NULL,
       rated_at
FROM ratings;

DROP TABLE ratings;
DROP TABLE feed_priors;
