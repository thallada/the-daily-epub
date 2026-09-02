# Personalized Ranking, Embeddings, Facets, and Feedback — Implementation Plan (v8)

**Date:** 2026-08-17 (revised 2026-08-19)
**Repository:** `thallada/the-daily-epub`
**Status:** implementation plan, revision 8
**Supersedes:** `2026-08-17-personalized-ranking-and-facets.v1-superseded.md`
**Reviews addressed:** the two 2026-08-18 reviews (R1, R2) and the six 2026-08-19 re-reviews of v2–v7 (R3–R8), all in `docs/reviews/`. §36 maps every finding from all eight to its resolution.

**Scope:** replace the current mostly heuristic candidate funnel with a high-recall, semantically personalized, facet-aware ranking pipeline while preserving the LLM as the final editor — and make it behave correctly on day one, when there are almost no ratings to learn from.

This plan is implementation-grade. An implementation agent should be able to execute it without rediscovering the current architecture or making major product decisions. Read these first:

- `docs/plans/2026-08-15-the-daily-epub.md` — original system design.
- `docs/plans/2026-08-15-implementation-notes.md` — implementation conventions and verified environment facts.

Then read the current curation implementation (exact paths):

- `src/pipeline.rs`
- `src/curate/mod.rs`
- `src/curate/prefilter.rs`
- `src/curate/score.rs`
- `src/curate/select.rs`
- `src/curate/llm.rs`
- `src/curate/editorial.rs`
- `src/curate/profile/mod.rs`
- `src/curate/profile/themes.rs`
- `src/types.rs`
- `src/db.rs`
- `src/config.rs`
- `src/main.rs`
- `migrations/0001_init.sql`

---

## 1. Why this change is needed

The current curation pipeline is:

```text
~400 daily articles
  -> deterministic heuristic prefilter (~120)
  -> DeepSeek Stage A scores those ~120
  -> combined_score() ranks them
  -> top ~40 are shown to DeepSeek Stage B
  -> Stage B chooses ~20 and arranges the issue
```

The last two stages are reasonably personalized; the first irreversible cut is not. `prefilter::score_article` decides which articles reach the personalized LLM using word count, HN/Reddit/Lobsters social proof, Scour/HN provenance, feed multiplicity, a per-feed rating prior, excerpt/paywall status, title-pattern penalties, and hard block/always-include rules.

Verified problems in the current code:

1. **Personalization happens too late.** A personally ideal article can be dropped before the reader profile, semantic interests, or learned rating patterns are considered.
2. **Correlated signals are counted repeatedly.** Social proof, long-form bias, and discovery-source provenance each influence multiple stages.
3. **Ratings are coarse and mis-attributed.** `PrefilterContext::prior_for` (`src/curate/prefilter.rs:118-129`) takes the **maximum** prior across every feed in the cluster, while rating credit is attributed only to `best_entry_id`'s feed. Optimistic on read, narrow on write.
4. **The LLM sees too little article text.** Stage A receives `EXCERPT_WORDS = 200` (`src/curate/score.rs:21`); Stage B sees ~45 words plus Stage A's rationale.
5. **The final shortlist can already be homogeneous.** Diversity is delegated to Stage B after a top-40 score cut.
6. **The code contradicts its own editorial philosophy.** `assemble()` in `src/curate/select.rs` tops a short lineup back up to `target - 5` ("Too few: top up from the best unpicked candidates"), padding an issue the editor did not want.
7. **There is no exploration mechanism.** A source or topic that never survives the funnel cannot generate the ratings that would improve its odds.
8. **There is no persisted ranking telemetry.** `scores` keeps `prefilter_score`/`llm_score` for survivors only, so no one can answer "why did this article disappear?"

The target architecture:

```text
all daily feed entries
  -> hard hygiene + dedupe + extraction + social
  -> embeddings for all eligible articles
  -> evidence-gated preference state from ratings
  -> high-recall union admission (~400 -> 120) with per-retriever quotas
  -> Stage A: editorial quality + reader fit + descriptive facets, on representative samples
  -> utility score over *present* signals, weights renormalized
  -> cluster-capped diversified shortlist (~120 -> ~60)
  -> Stage B LLM editor chooses the issue
  -> no forced filler
  -> ratings immediately update embedding/facet/feed preference models
```

The core principle is unchanged: **heuristics may cheaply propose candidates, but they must no longer decide what the personalized system is allowed to see.**

---

## 2. Reality check: this system starts cold, and that is the design centre

Verified on 2026-08-18:

- The repository's initial commit is `9e30c1d`; the first published issue is `out/The Daily EPUB - 2026-08-15.epub`. **The service has published on the order of one issue.**
- `data/scour-interests.opml` contains **230** standing interests.
- The production database (`/var/lib/daily-epub/daily-epub.db`) is not readable from the development account, so the exact rating count is unverified. It is bounded above by roughly one issue's worth of articles.

Two consequences drive this revision:

1. **Rating-derived signals have no evidence yet and must contribute nothing until they do.** A signal computed from ~0 ratings is noise, and v1 of this plan gave that noise 28% of the pre-Stage-A blend and 15% of utility. Every rating-derived signal in this plan is gated behind an explicit evidence ladder (§14) and is dropped from the blend — with weights renormalized — until it clears the gate (§12.3).
2. **Standing-interest semantic matching is the only personalization signal that works on day one.** 230 curated interests exist right now and require zero ratings. This is where the immediate win is, and §11 spends its complexity budget there (z-scoring across the day's pool) rather than on rating-derived machinery that cannot fire yet.

Before implementing, the operator should record the actual counts so the ladder thresholds can be sanity-checked:

```sh
sqlite3 /var/lib/daily-epub/daily-epub.db \
  "SELECT (SELECT COUNT(*) FROM ratings) AS ratings,
          (SELECT COUNT(*) FROM articles) AS articles,
          (SELECT COUNT(*) FROM issues) AS issues;"
```

Write the result into `docs/plans/2026-08-15-implementation-notes.md` as a verified fact with its date.

---

## 3. Product goals and non-goals

### Goals

1. Increase recall of articles that closely match the reader's interests or learned taste even when they are short, quiet, or from obscure feeds.
2. Learn preferences at the article-feature level rather than primarily at the feed level — **once there is evidence to learn from**.
3. Preserve topic semantic similarity while separately modeling non-topic preferences such as format, depth, and evidence style.
4. Make the final shortlist diverse before it reaches the LLM editor.
5. Give the LLM better evidence about article quality by sampling the beginning, middle, and end.
6. Keep the service robust: missing Voyage/DeepSeek keys or API failures must degrade to existing heuristic behavior rather than prevent an issue.
7. Keep every ranking decision **explainable from persisted per-run features**, and keep the *scalar* ranking path replayable from persisted raw values. (Narrowed from v1's "exact replay" — see §27.3 for what is and is not reproducible.)
8. Keep infrastructure simple. At this scale, SQLite plus in-process dot products is sufficient; do not add a vector database.
9. Make the new system tunable through configuration and offline evaluation rather than burying another generation of hard-coded weights in code.
10. **Degrade visibly, never invisibly.** A signal with no evidence must be absent from the blend and recorded as absent, not silently equal to a constant or to article-ID order.

### Non-goals

- Do not train a custom neural recommender in this iteration.
- Do not add collaborative filtering; this is a single-reader system.
- Do not treat the absence of a rating as a downvote.
- Do not infer political ideology or sensitive personal attributes from article content.
- Do not remove the weekly natural-language taste-profile mechanism; make it a prior rather than a constitution.
- Do not replace DeepSeek Stage B.
- Do not introduce Qdrant, pgvector, or Elasticsearch for a few hundred vectors a day.
- Do not add a second LLM round-trip stage for facets in V1 (§15.1).

---

## 4. Verified Voyage AI facts and chosen defaults

Use **Voyage AI `voyage-4-lite`** for article and interest embeddings.

Verified against Voyage AI's official documentation on 2026-08-17 and independently re-verified by both reviewers on 2026-08-18:

- REST endpoint: `POST https://api.voyageai.com/v1/embeddings`
- Authentication: `Authorization: Bearer <API key>`
- Context length: 32,000 tokens per input.
- Supported dimensions: 256, 512, 1024 (API default), 2048.
- At most 1,000 inputs per request and, for `voyage-4-lite`, at most 1M input tokens per request.
- `input_type` supports `query` and `document`. `query` causes Voyage to prepend its own retrieval instruction server-side.
- Voyage embeddings are unit-normalized, so dot product and cosine similarity are equivalent.
- Published pricing is $0.02 / 1M tokens after the free allocation; the first 200M text-embedding tokens are currently free per account.

Official references:

- <https://docs.voyageai.com/reference/embeddings-api>
- <https://docs.voyageai.com/docs/embeddings>
- <https://docs.voyageai.com/docs/faq>
- <https://docs.voyageai.com/docs/pricing>

> **Re-verification rule:** this block is operational metadata with a shelf life. Re-check it whenever `voyage.model` changes, and stamp the new verification date here — the same convention the 2026-08-15 implementation notes use.

### 4.1 V1 choices

```toml
[voyage]
enabled = true
base_url = "https://api.voyageai.com/v1"
model = "voyage-4-lite"
output_dimension = 512
batch_size = 32
max_concurrent_requests = 4
max_input_chars_per_article = 60000
max_input_chars_per_batch = 900000
price_per_mtok = 0.02
max_daily_usd = 0.25
```

**`output_dimension = 512` is the V1 default**, changed from v1's 1024. Voyage's Matryoshka training makes 512 near-lossless for retrieval on general text; it halves BLOB storage (~2 KB vs ~4 KB per article, i.e. ~300 MB/year rather than ~600 MB at ~400 articles/day) and halves every dot product, and this database shares a small VPS with the EPUB output directory. 1024 remains one config line away and is the thing to evaluate *into* (§35).

`max_daily_usd = 0.25` is a **runaway guard, not a bill**: at $0.02/Mtok it trips at 12.5M tokens/day, roughly 25× expected volume. It exists to bound a bug, and it cannot know whether the 200M free allocation is exhausted. Do not tune it as if it were a spending limit.

The API key must come only from:

```text
DAILY_EPUB_VOYAGE__API_KEY
```

Never put an API key in `config.toml`, `config.example.toml`, tests, fixtures, logs, run reports, or the database.

Use `output_dtype = "float"`. Do not quantize until there is measured pressure. Use the REST API directly through `reqwest`; do not add a Python runtime or a Voyage SDK.

---

## 5. Target pipeline

```text
 0. take the generation file lock; open run + provisional manifest
 1. Miniflux ingest
 2. normalize/dedupe
 3. content extraction
 4. persist articles
 5. social enrichment
 6. hard hygiene filter (as-of bounded)              -> eligible set (~400)
    ├─ provider policy split: protected articles bypass every external call
 7. embeddings for all eligible articles (cached)
 8. interest query embeddings (cached)
 9. preference state from ratings, evidence-gated
10. per-candidate signal computation over the whole eligible set
11. signal normalization (mid-rank percentiles over present values)
    └─ manifest → `ranking_fixed` (candidate rows become interpretable)
12. union admission with per-retriever quotas         (~400 -> 120)
13. Stage A: quality + reader fit + descriptive facets (120)
14. utility score over present signals                 (120)
15. leader-clustered diversified shortlist              (120 -> ~60)
16. Stage B final editorial selection (soft target 20, hard max 25, no minimum)
    └─ reinsert protected auto-includes, subject to hard_max
17. comments/world/editorial/EPUB/publish; issue + lineup + publication events in one
    transaction; manifest → `final` with `runs.status`; the lock releases with the process
```

Two structural changes from v1 of this plan:

- **The intermediate 240-article "recall pool" is gone.** In v1 it existed only to bound the cost of a separate facet-extraction LLM stage. With facets folded into Stage A (§15.1), nothing between hygiene and Stage A has a per-item cost, so the union operates directly at the 400 → 120 boundary. Each retriever gets a *guaranteed quota of Stage A slots*, which is a stronger and simpler recall guarantee than v1's two-stage cap-and-protect scheme.
- **MMR is replaced by cluster-capped diversification** (§20). The actual problem — "six articles about the same news cycle" — is a discrete cluster-cap problem, and cluster caps have one interpretable parameter and render usefully in `explain`.

`prefilter.rs` is refactored, not deleted. It keeps hard hygiene and the cheap heuristic score; that score becomes one retriever and one weak utility component instead of the sole gate.

---

## 6. Determinism: `as_of`, run identity, and modes

v1 of this plan had no notion of simulated time, and the existing code anchors history to wall-clock now (`profile/mod.rs:312` uses `Timestamp::now()`; `db::previously_published_ids` at `src/db.rs:317-322` returns article IDs from *every* issue, including issues dated after a historical target date). Replaying 2026-08-01 on 2026-08-18 would therefore train on two weeks of future votes and exclude articles for being published in the future. Fix this before building anything that reads history.

### 6.1 `as_of`

Every run and every evaluation carries exactly one `as_of: jiff::Timestamp`. All of the following must be bounded by it, with no exceptions and no hidden `Timestamp::now()`:

- ratings window — the latest `rating_events` row per article with `event_at <= as_of` and `>= as_of - rating_lookback_days` (§7.9), never the overwriting `ratings` projection,
- previously-published exclusion — `publication_events` with `published_at <= as_of` and `issue_date < run_date` (§7.9), never `issue_articles`, which is deleted and replaced on republish,
- recently-rejected churn rule — the latest `candidate_rankings` observation joined to `runs.started_at <= as_of` (§7.8), never `scores`, which is overwritten per nominal date,
- feed priors — derived in memory from those bounded rating events and the feed set each event recorded (§7.7), never from a shared mutable aggregate and never from current `sources_json`,
- profile version selection (§7.4b),
- embedding/facet cache reads, subject to the **feature-time policy** below.

Thread `as_of` through `PrefilterContext::load`, the new `PreferenceState::load`, and every new `db` helper. Do not default it inside `db`; make callers pass it.

### 6.2 Modes, and the feature-time policy

| Mode | CLI | `as_of` | Features | Meaning |
|---|---|---|---|---|
| `live` | `generate` (no `--date`) | now | current | Normal daily run. |
| `recurate` | `generate --date D` | now | current | Re-curate day D using **today's** knowledge. Existing behavior; stays the default for `--date`. |
| `fidelity` | `generate --date D --as-of-date`, `evaluate` | end of day D | `created_at <= as_of` only | Reconstruct what the system could have known on day D. |
| `counterfactual` | `evaluate --counterfactual-features` | end of day D | any, including later backfills | How would today's algorithm have ranked day D's candidates, given features computed since? |

`dry_run` and `shadow` are orthogonal flags recorded in the manifest, not modes. The mode's feature rule is recorded as `run_manifests.feature_time_policy` (`current` | `as_of_only` | `counterfactual`) so results from different policies can never be pooled by accident.

**Why the split exists.** v3 collapsed these into one `replay` mode and then asserted two incompatible things: that features created after `as_of` are ignored, and that backfilling embeddings/facets now enables replaying historical dates (§27.1). Backfilled rows are *by construction* created after a historical `as_of`, so a strict fidelity replay must ignore precisely the features the backfill was meant to supply, and would silently report that the new ranker had no semantic signal on any pre-migration date. Both operations are useful; they simply answer different questions and cannot share a label.

Consequences, stated plainly:

- **Fidelity replay is only meaningful for dates after this system shipped**, when features were generated live. Applied to an earlier date it will honestly report near-total feature absence — that is the correct answer to "what could the system have known?", not a bug to work around.
- **Counterfactual evaluation is the mode for tuning on history**, and it is approximate for a second reason beyond backfill timing: `db::upsert_article` overwrites `content_html` on re-ingest, so the text a historical embedding described may no longer be the text on file (§27.2).
- The prose profile follows the same rule: in `fidelity`, it is the `taste_profile_versions` row with the greatest `built_at <= as_of`, or none at all with `profile_version = NULL` recorded. In `counterfactual`, the latest profile may be used, and the manifest says so. Profile *text* must be retained for either to work — a version number and a hash cannot reconstruct a prompt.

### 6.3 Run identity

Everything a run persists is keyed by `runs.id`, never by date alone. `runs` already allows multiple invocations per date; v1 of this plan keyed `candidate_rankings` by `(run_date, article_id)`, which would have made a shadow run and a live run overwrite each other.

### 6.4 Determinism rules

- Sorting is always stable, with `article_id` ascending as the final tiebreak — **in output ordering only, never inside a normalizer** (§12.1).
- Exploration selection uses `blake3`/SHA-256 over `(exploration_salt, run_date, article_id)`, where `exploration_salt` is recorded in the manifest and changes when the selection algorithm changes, so an implementation change cannot masquerade as reproducible behavior.
- `algorithm_version: u32` is a constant bumped on any change to admission, normalization, utility, or diversification semantics, and is recorded in the manifest.

---

## 7. Data model and migrations

New migration `migrations/0002_personalized_ranking.sql`. Do not edit `0001_init.sql`.

### 7.1 `article_embeddings`

```sql
CREATE TABLE article_embeddings (
    article_id      INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    model           TEXT NOT NULL,
    dimension       INTEGER NOT NULL,
    input_hash      TEXT NOT NULL,
    embedding       BLOB NOT NULL,
    input_tokens    INTEGER,
    created_at      TEXT NOT NULL,
    PRIMARY KEY (article_id, model, dimension)
);

CREATE INDEX idx_article_embeddings_model ON article_embeddings(model, dimension);
CREATE INDEX idx_article_embeddings_created ON article_embeddings(created_at);
```

- Store f32 values as a compact little-endian BLOB with explicit encode/decode helpers and round-trip tests.
- Decode validates `blob.len() == dimension * 4` **and** that every value is finite. A corrupt or non-finite row is ignored with a warning; never panic.
- After decode, verify the norm is within `1e-3` of 1.0; if not, normalize and log once per run. Downstream dot products may then be treated as cosine and clamped to `[-1, 1]`.
- `input_hash` is SHA-256 over `EMBEDDING_DOCUMENT_VERSION` plus the exact normalized text sent to Voyage.
- Model and dimension are part of the cache key; never compare vectors across model/dimension pairs. (Voyage states 4-series embeddings are mutually compatible; this isolation is a deliberate reproducibility choice, relaxable only after explicit evaluation.)
- Rows are **overwritten in place** when the input hash changes. This is a deliberate storage-simplicity choice with a replay consequence documented in §27.3.

**Retention** (new): `features prune` (and the existing prune path) deletes embeddings for articles that are neither rated nor ever published in an issue and whose `articles.first_seen` is older than `voyage.embedding_retention_days` (default 120). Without this the table grows without bound; nothing in the current codebase prunes `articles`.

### 7.2 `interest_embeddings`

```sql
CREATE TABLE interest_embeddings (
    interest        TEXT NOT NULL,
    model           TEXT NOT NULL,
    dimension       INTEGER NOT NULL,
    text_version    INTEGER NOT NULL,
    input_hash      TEXT NOT NULL,
    embedding       BLOB NOT NULL,
    created_at      TEXT NOT NULL,
    PRIMARY KEY (interest, model, dimension, text_version)
);
```

`text_version` is part of the key so the two candidate interest-text formats (§11.1) can coexist and be compared without a cache wipe.

### 7.3 `article_facets`

```sql
CREATE TABLE article_facets (
    article_id       INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    schema_version   INTEGER NOT NULL,
    model            TEXT NOT NULL,
    prompt_version   INTEGER NOT NULL,
    input_hash       TEXT NOT NULL,
    facets_json      TEXT NOT NULL,
    source           TEXT NOT NULL CHECK (source IN ('stage_a', 'dedicated')),
    profile_version  INTEGER,                -- provenance only, not cache identity
    extracted_at     TEXT NOT NULL,
    PRIMARY KEY (article_id, schema_version, model, prompt_version, input_hash)
);

CREATE INDEX idx_article_facets_article ON article_facets(article_id, schema_version);
```

The primary key **is** the cache identity, fixing v1's contradiction between a `(article_id, schema_version)` key and a stated `(article_id, schema_version, input_hash)` cache rule. A prompt or model change now produces a new row instead of silently reusing or clobbering the old one.

`input_hash` is SHA-256 over the **exact effective facet input**: excerpt-format version, the title/author/source/word-count fields actually sent, and the representative text — not merely the article body.

`profile_version` is recorded for provenance but is deliberately *not* part of the cache key. See §15.3 for that tradeoff and the stability check that guards it.

### 7.4 `run_manifests`

```sql
CREATE TABLE run_manifests (
    run_id                     INTEGER PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE,
    run_date                   TEXT NOT NULL,
    as_of                      TEXT NOT NULL,
    mode                       TEXT NOT NULL
                               CHECK (mode IN ('live','recurate','fidelity','counterfactual')),
    feature_time_policy        TEXT NOT NULL
                               CHECK (feature_time_policy IN ('current','as_of_only','counterfactual')),
    shadow                     INTEGER NOT NULL DEFAULT 0 CHECK (shadow IN (0,1)),
    dry_run                    INTEGER NOT NULL DEFAULT 0 CHECK (dry_run IN (0,1)),
    manifest_status            TEXT NOT NULL DEFAULT 'provisional'
                               CHECK (manifest_status IN ('provisional','ranking_fixed','final')),
    algorithm_version          INTEGER NOT NULL,
    ranking_config_json        TEXT NOT NULL,   -- every weight, quota, threshold, gate
    embedding_model            TEXT,
    embedding_dimension        INTEGER,
    embedding_document_version INTEGER,
    interest_text_version      INTEGER,
    excerpt_format_version     INTEGER,
    facet_schema_version       INTEGER,
    facet_prompt_version       INTEGER,
    facet_model                TEXT,
    profile_version            INTEGER,         -- NULL ⇒ prose profile not used (§6.2)
    profile_hash               TEXT,
    exploration_salt           TEXT NOT NULL,
    evidence_weights_json      TEXT,            -- the four W values (§14); NULL while provisional
    stage_completeness_json    TEXT,            -- versioned per-stage coverage; required when final
    finalized_at               TEXT,
    created_at                 TEXT NOT NULL
);

CREATE INDEX idx_run_manifests_date ON run_manifests(run_date);
```

Without this, a `candidate_rankings` row from three weeks ago cannot be interpreted, because the weights that produced it are gone.

**Three states, because "ranking inputs fixed" and "run outcome known" happen at different times.** v4 conflated them: it set `manifest_status = 'final'` right after preference state loaded — before admission, Stage A, Stage B, and publication — while also requiring every final manifest to carry complete stage coverage. The only way to satisfy both is to write placeholder zeroes into a record declared authoritative and mutate it later, with nothing making that later mutation atomic with `runs.status`. A crash between the two writes leaves an `ok` run advertising stale completeness.

| State | Written | Meaning |
|---|---|---|
| `provisional` | at run start | Mode, feature-time policy, `as_of`, algorithm version, config, salt. "A run started." |
| `ranking_fixed` | once preference state and profile selection complete | Adds evidence weights, model/schema/profile versions. **Every `candidate_rankings` row is interpretable from this point on**, which is all the ranking snapshot needs. |
| `final` | in the same transaction as `finish_run` | Adds `stage_completeness_json` and `finalized_at`, written together with `runs.status`. "This run's outcome is known." |

Consequences:

- Candidate rows are **not** coupled to manifest finalization. They are written as stages complete, interpretable as soon as the manifest reaches `ranking_fixed`.
- A zero-candidate run — empty ingest window, all-hygiene-excluded day — reaches `ranking_fixed` and then `final` normally at end of run, so the R4 fix survives: such a run is evaluable and reports "0 eligible" rather than sitting provisional forever (§31.8).
- Evaluation requires `final` **and** an eligible run status (§7.6). `explain` accepts `ranking_fixed`, since debugging why an article ranked as it did should not require the run to have finished.
- Because completeness and `runs.status` are written in one transaction, they cannot disagree.

**Failure transitions, by failure point** (v5 said every mid-run failure stays `provisional`, which cannot hold once `ranking_fixed` exists, and would throw away usable completeness data):

| Failure point | Manifest ends at | Rationale |
|---|---|---|
| Before preference/profile capture | `provisional` | The ranking was never defined; there is nothing to interpret. |
| After `ranking_fixed`, before `finish_run` | `ranking_fixed` | Candidate rows written so far remain interpretable. |
| At `finish_run` on an error path | `final`, with terminal completeness recording which stages did not complete | `finish_run` already runs on error paths today, so this is where the truth is known. |

`evaluate` excludes `status = 'failed'` in all three cases. `explain --run-id` accepts all three states and shows whatever exists — debugging a failed run is a normal reason to reach for it.

`stage_completeness_json` is a **versioned typed structure**, not free-form JSON, because per-metric eligibility (§7.6) depends on its contents — an unversioned blob that silently changes shape would silently change metric denominators:

```json
{
  "completeness_version": 1,
  "embeddings":     { "attempted": 417, "succeeded": 412 },
  "admission":      { "eligible": 417, "admitted": 120, "completed": true },
  "stage_a":        { "attempted": 120, "succeeded": 96, "skipped_budget": 24 },
  "facets":         { "attempted": 120, "succeeded": 94 },
  "utility":        { "scored": 120, "completed": true },
  "diversification":{ "shortlisted": 60, "completed": true },
  "selection":      { "attempted": 1, "succeeded": 1, "fallback_used": false },
  "publication":    { "completed": true, "dry_run": false }
}
```

The stage list is not decorative: §7.6 gates admission metrics on the admission stage completing, so an `admission` field must exist to gate them *with*. Every stage a metric depends on appears here, including selection and publication.

Rules: it is required (non-NULL and parseable) whenever `manifest_status = 'final'`, enforced in application logic inside the finalize transaction; every key in the current version must be present, with zeros and `completed: false` where a stage did not run; a parse failure or unknown `completeness_version` makes the run **ineligible for every metric**, reported with an explicit diagnostic rather than treated as "no restrictions". All filtering happens after typed decoding in Rust — no ad-hoc SQL JSON path expressions.

### 7.4b `taste_profile_versions`

Replay selects the profile that was effective at `as_of` (§6.1), but the current implementation overwrites the `taste_profile`, `taste_profile_learned`, and `profile_version` singletons in `kv` (`profile::store`, `src/curate/profile/mod.rs:223-231`). After the next weekly rebuild, the text that was effective for an earlier date is gone, and `profile_hash` cannot reconstruct it. Version selection would silently depend on whether an old value happened to survive.

```sql
CREATE TABLE taste_profile_versions (
    version        INTEGER PRIMARY KEY,
    built_at       TEXT NOT NULL,
    profile_hash   TEXT NOT NULL,
    profile_text   TEXT NOT NULL,
    learned_text   TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_taste_profile_versions_built_at ON taste_profile_versions(built_at);
```

- `profile::store` writes a new row **transactionally with** the `kv` singleton update; `kv` remains the fast "current" pointer.
- The first historical row is seeded by a **Rust bootstrap**, not by the migration SQL (see below).
- Fidelity replay selects `MAX(built_at) <= as_of`. If no row qualifies, the prose profile is disabled for that run and `run_manifests.profile_version` is `NULL` — a recorded, testable state rather than an accident.
- Profile text is a few kilobytes rewritten weekly; retention is not a concern.

**Seeding must be a Rust bootstrap, not migration SQL.** Seeding the first row requires parsing the JSON `kv[profile_version]` payload for `version`/`built_at`, and computing SHA-256 over the existing profile text for `profile_hash`. SQLite has no built-in SHA-256, and this repository's migrations are plain SQL — so the shown DDL cannot produce a valid row, and a fake or empty hash would break the manifest identity contract it exists to serve.

**Two independent bootstraps, each with a durable marker.** Profile history and observation history are unrelated migrations and must not share an early-return condition — a database with issues and ratings but no taste profile still needs its events seeded. Both run immediately after `sqlx::migrate!`, inside the migration critical section (§24.2), and both record completion in `kv` under a versioned marker key (`bootstrap:profile_history:v1`, `bootstrap:observation_history:v1`) written **in the same transaction as the seeding**. The marker, not a row-count heuristic, decides whether the work has been done: "seed if the table is empty" is a state guess that a legitimate first event or a rolled-back attempt can both defeat.

`db::bootstrap_profile_history()`:

1. open one transaction; return immediately if the marker is present;
2. read `kv[taste_profile]`, `kv[taste_profile_learned]`, `kv[profile_version]`;
3. if `taste_profile` is absent or empty, **set the marker and commit** (fresh database — the first `profile::store` writes row 1);
4. if `kv[profile_version]` is absent or unparseable, use `version = 1` and `built_at = now`, logging a warning — matching `stored_version`'s existing tolerance of a malformed payload;
5. compute the hash in Rust and `INSERT … ON CONFLICT(version) DO NOTHING`;
6. **repair `kv[profile_version]` to the canonical payload just seeded**, in the same transaction;
7. set the marker and commit before any profile load or rebuild can run.

`db::bootstrap_observation_history()` seeds §7.9's event tables from the projections in one transaction: one `rating_events` row per `ratings` row (using its `rated_at`, with `feed_credits_json` computed from the article's sources as they stand at migration time), one `publication_events` row per `issue_articles` row (using its issue's `generated_at`), then the marker. It runs regardless of whether a taste profile exists.

A crashed or rolled-back seeding attempt leaves no marker and no rows, so the retry is clean.

**Cutover requires stopping `serve` first — this is a deployment step, not an implementation detail.** An already-running `serve` is the one writer that does not take the lock (§24.2), and during the `0002` rollout it is still the *old* binary, which writes only the `ratings` projection. If it accepts a vote after seeding commits, that vote never becomes a `rating_events` row, and the durable marker guarantees no later repair will notice. The event authority would then be permanently missing a real vote. "The concurrent write is newer than the seed" is only true once `serve` is the dual-writing binary, which at cutover it is not.

The protocol, to be written into the README's upgrade notes:

1. stop the `serve` unit (the rating endpoint goes down; the EPUB and OPDS files stay where they are);
2. install the new binary and run `daily-epub db migrate`, which performs the migration and both bootstraps under the lock;
3. start the new `serve`.

Downtime is seconds, and votes are not lost — the rating links are HMAC-signed URLs the reader can simply re-open. A dual-write compatibility release would avoid even that, and is not worth the complexity for a single-host single-reader service.

Step 6 is not cosmetic. `stored_version` treats a malformed payload as *absent*, so without the repair the next weekly rebuild would pick version 1 again — colliding with the row just seeded, and forcing either a failed insert or an upsert that overwrites the very history the table exists to preserve. Belt and braces: `profile::weekly_rebuild_if_due` allocates the next version as `MAX(taste_profile_versions.version) + 1` rather than from the `kv` pointer, and updates the pointer transactionally with the append.

§31.13 tests all four profile input states, and the malformed case runs a rebuild afterwards to assert it produces **version 2 with version 1 preserved** — not merely that the bootstrap succeeded. It also tests observation seeding independently: a **projection-only database with issues and ratings but no taste profile** gets its events seeded, a second startup is a no-op by marker, and a seed interrupted mid-transaction leaves neither rows nor marker, so the retry succeeds cleanly.

### 7.4c Generation mutual exclusion: an OS file lock, not an expiring lease

Only one mutating `generate` may run at a time (§24.2).

**Use an advisory file lock (`flock(LOCK_EX | LOCK_NB)`) on `<database_path>.lock`, held by an open file descriptor for the process lifetime.** v3 proposed a SQLite lease with a 30-minute TTL refreshed "at each stage boundary". That design is unsound here, and fixing it properly costs more than replacing it:

- Stage A wall clock is currently **unmeasured** (§18.5 adds the instrumentation), and `features backfill` can run for many batches. Any stage exceeding the TTL lets a second process reclaim the lease while the first is still working — both then proceed, which is precisely the guarantee the lease was for.
- Worse, the original holder's RAII guard would later refresh or delete the *replacement owner's* row. Preventing that needs a fencing token on every refresh and release, plus a background heartbeat at TTL/3, plus an ownership re-check before every external call and every persistent side effect.

A file lock has none of those failure modes: there is no expiry, so nothing to race; the kernel releases it when the process dies, however it dies, so crash recovery is automatic and needs no timeout heuristic; and a stale holder physically cannot steal it back. This is a single-reader service on one Linux host with a local-disk SQLite database — the deployment the primitive is designed for.

A small advisory row remains, for human-readable diagnostics only, written *after* the lock is held and never consulted for correctness:

```sql
CREATE TABLE generation_lock_info (
    name        TEXT PRIMARY KEY,      -- 'generate'
    run_id      INTEGER,
    owner       TEXT NOT NULL,         -- host:pid
    acquired_at TEXT NOT NULL
);
```

So the second invocation can say *"generate is already running (host:pid 41233, started 05:31:02)"* instead of a bare `EWOULDBLOCK`. If the row is stale because a process was killed, the message is stale too — harmless, since the lock itself already told the truth.

`--wait-for-lease [SECS]` polls `flock` with backoff up to the deadline. If a multi-host deployment ever appears, revisit — that is the one scenario where a fenced DB lease earns its complexity (§35).

### 7.4d `adjudications`

Phase A's only honest label source for candidates the authoritative selector never exposes (§32).

```sql
CREATE TABLE adjudication_batches (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id            INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    run_date          TEXT NOT NULL,
    algorithm_version INTEGER NOT NULL,
    sample_seed       TEXT NOT NULL,
    created_at        TEXT NOT NULL
);

CREATE TABLE adjudications (
    batch_id       INTEGER NOT NULL REFERENCES adjudication_batches(id) ON DELETE CASCADE,
    article_id     INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    arm            TEXT NOT NULL CHECK (arm IN ('union_only', 'control')),
    display_order  INTEGER NOT NULL,
    verdict        INTEGER CHECK (verdict IN (0, 1)),   -- NULL until adjudicated
    adjudicated_at TEXT,
    PRIMARY KEY (batch_id, article_id)
);

CREATE INDEX idx_adjudications_article ON adjudications(article_id);
```

Keyed by `run_id`, not by date. A date can carry live, shadow, dry-run, and rerun manifests with different candidate sets and different configurations, so `(run_date, article_id)` cannot say *which* run defined "union-only" and "control" — and a later rerun could silently change the reasoning behind a verdict already collected. `algorithm_version` and `sample_seed` make the sample reproducible; `display_order` is stored separately from `arm` so the blind can be verified after the fact rather than trusted.

Deduplication is **per article globally, with a cooldown**: an article already adjudicated within `adjudication_cooldown_days` (default 30) is not re-sampled. v3's date-keyed table would have re-presented the same article the next day, since the ingest window overlaps.

`evaluate --adjudicate` prints the selected `run_id` and batch ID before collecting any labels, so the operator can see what they are labelling against.

### 7.5 `candidate_rankings`

Persist a row for **every article the run considered**, including hygiene-excluded ones. The most common answer to "why did this article not show up?" is "it was blocked / already published / recently rejected", and v1's post-hygiene-only table could not answer it.

```sql
CREATE TABLE candidate_rankings (
    run_id                      INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    article_id                  INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    run_date                    TEXT NOT NULL,

    -- raw signals (NULL == not available; never coerce to 0)
    heuristic_score             REAL,
    social_score                REAL,
    feed_affinity               REAL,
    semantic_interest_score     REAL,
    semantic_interest_raw_top1  REAL,
    positive_similarity         REAL,
    negative_similarity         REAL,
    embedding_preference_score  REAL,
    facet_preference_score      REAL,
    preliminary_score           REAL,

    llm_quality_score           REAL,
    llm_reader_fit_score        REAL,
    utility_score               REAL,

    -- funnel bookkeeping
    admitted_by                 TEXT,           -- JSON array of retriever names
    excluded_reason             TEXT,           -- see enum below; NULL if not excluded
    terminal_stage              TEXT NOT NULL,  -- hygiene|admission|stage_a|utility|shortlist|selected
    exploration_candidate       INTEGER NOT NULL DEFAULT 0 CHECK (exploration_candidate IN (0,1)),
    interleave_pick             INTEGER NOT NULL DEFAULT 0 CHECK (interleave_pick IN (0,1)),
    auto_include                INTEGER NOT NULL DEFAULT 0 CHECK (auto_include IN (0,1)),
    stage_a_candidate           INTEGER NOT NULL DEFAULT 0 CHECK (stage_a_candidate IN (0,1)),
    shortlist                   INTEGER NOT NULL DEFAULT 0 CHECK (shortlist IN (0,1)),
    selected                    INTEGER NOT NULL DEFAULT 0 CHECK (selected IN (0,1)),

    rank_by_preliminary         INTEGER,
    rank_by_utility             INTEGER,
    cluster_id                  INTEGER,
    cluster_rank                INTEGER,

    explanation_json            TEXT,
    PRIMARY KEY (run_id, article_id)
);

CREATE INDEX idx_candidate_rankings_date ON candidate_rankings(run_date, run_id);
CREATE INDEX idx_candidate_rankings_article ON candidate_rankings(article_id);
CREATE INDEX idx_candidate_rankings_stage ON candidate_rankings(run_id, terminal_stage);
```

`excluded_reason` enum: `blocked` | `published_before` | `churn_recent_reject` | `not_admitted` | `stage_a_budget_skip` | `cluster_suppressed` | `shortlist_cap` | `not_selected_by_editor` | `over_max_trim`.

Retriever names for `admitted_by`: `auto_include` | `heuristic` | `semantic_interest` | `embedding_preference` | `feed_affinity` | `exploration` | `blend_fill`.

`explanation_json` has a **required, versioned schema** (`explanation_version: u32`), not free-form JSON:

```json
{
  "explanation_version": 1,
  "normalized": { "heuristic": 0.71, "semantic_interest": 0.94, "social": 0.5 },
  "present":    { "heuristic": true, "semantic_interest": true, "social": false,
                  "embedding_preference": false, "facet_preference": false },
  "effective_weights": { "heuristic": 0.36, "semantic_interest": 0.46, "social": 0.18 },
  "top_interests": [ { "name": "Gaussian Splatting", "z": 3.4, "raw": 0.62 } ],
  "nearest_upvotes": [ { "article_id": 812, "similarity": 0.71, "weight": 0.84 } ],
  "facet_contributions": [ { "dimension": "evidence", "value": "first_hand", "effect": 0.21 } ],
  "notes": ["embedding_preference gated: W_embedding 1.0 < evidence_floor 5.0"]
}
```

**Write policy** (this matters — the house idiom is wrong here). `db::upsert_score` uses `COALESCE(excluded.x, scores.x)` (`src/db.rs:394-397`), which deliberately *preserves* prior values. `candidate_rankings` must do the opposite: rows belong to one `run_id`, are written with a plain insert (or `INSERT OR REPLACE` on the same run), and a rerun creates a **new** `run_id`. Within a run, later stages update their own columns only. If a run is retried in place, `DELETE FROM candidate_rankings WHERE run_id = ?` first, inside the same transaction as the first insert — mirroring `db::replace_issue_articles`, which is the correct existing precedent.

**Lifecycle:** evaluation eligibility is defined in §7.6 against the *real* `RunStatus` vocabulary. Do not invent a `complete` status.

**Retention:** prune `candidate_rankings` rows older than `curation.personalization.ranking_retention_days` (default 180). At ~400 rows/day this is ~72k rows — trivial for SQLite, but bounded on purpose.

**Dry runs** write ranking rows (articles are already persisted under `--dry-run` in `src/pipeline.rs`); the manifest records `dry_run = 1` and evaluation may filter on it. This is what makes Phase A shadowing possible.

### 7.6 The provider ledger, and what "evaluable" means

```sql
ALTER TABLE runs ADD COLUMN voyage_input_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE runs ADD COLUMN voyage_cost_usd REAL NOT NULL DEFAULT 0.0;
```

Those two columns are a **per-run rollup for the report**. They are not the guardrail, because the existing guardrail is not actually daily:

```sql
-- src/db.rs — sums the NOMINAL issue date, not the day the money was spent
SELECT COALESCE(SUM(cost_usd), 0.0) FROM runs WHERE date = ?
```

Four holes follow from that, all of which matter for something described as a runaway guard:

1. **Wrong bucket.** Recurating August 1 on August 19 charges the August 1 bucket, which is almost certainly empty.
2. **Fresh ceiling per historical date.** Recurating five old dates in one afternoon grants five full daily ceilings on one real day.
3. **Unbucketed commands.** `features backfill` and the standalone `profile rebuild` (`Command::Profile(ProfileCommand::Rebuild)` in `src/main.rs`) both call providers and neither creates a `runs` row, so their spend is invisible to every ceiling.
4. **Crash-lost spend.** Reservations live in process memory until `finish_run`. A crash after dispatch but before that leaves zero persisted spend, and the retry starts from an understated balance. Serialization (§24.2) prevents *concurrent* overspend; it does nothing about spend that was simply never written down.

```sql
CREATE TABLE provider_usage (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    provider        TEXT NOT NULL CHECK (provider IN ('deepseek', 'voyage')),
    operation       TEXT NOT NULL,      -- stage_a | stage_b | editorial | profile | facets | embed | backfill
    budget_class    TEXT NOT NULL CHECK (budget_class IN ('publication','shadow','maintenance')),
    request_id      TEXT NOT NULL,      -- logical request; retries share it
    attempt         INTEGER NOT NULL CHECK (attempt >= 1),
    run_id          INTEGER REFERENCES runs(id),   -- NULL for non-run commands
    billing_day     TEXT NOT NULL,      -- UTC date derived from reserved_at, never caller-supplied
    reserved_at     TEXT NOT NULL,
    estimated_usd   REAL NOT NULL CHECK (estimated_usd >= 0),
    actual_usd      REAL CHECK (actual_usd IS NULL OR actual_usd >= 0),
    input_tokens    INTEGER,
    output_tokens   INTEGER,
    cached_tokens   INTEGER,
    status          TEXT NOT NULL CHECK (status IN ('reserved','settled','failed_estimated')),
    settled_at      TEXT,
    UNIQUE (request_id, attempt)
);

CREATE INDEX idx_provider_usage_day ON provider_usage(provider, billing_day, budget_class);
CREATE INDEX idx_provider_usage_run ON provider_usage(run_id);
```

**One row per outbound HTTP attempt, not per logical request.** Retries share a `request_id` and increment `attempt`. This matters exactly where §24's conservative rule matters most: a 5xx that returns no usage payload is the case most likely to have been billed anyway, and if one row covered the whole logical request, the successful retry would settle it to the retry's actual usage and *erase* the failed attempt's standing estimate. Per-attempt rows make the day's total `attempt 1 estimate + attempt 2 actual`, which is both conservative and auditable — `SELECT * FROM provider_usage WHERE request_id = ?` shows exactly what happened.

**Reservation formula — an upper bound, not an estimate**, computed before dispatch over the exact assembled payload:

```text
input_bound_tokens = payload_utf8_bytes + per_request_overhead_tokens   (default 256)

DeepSeek: input_bound_tokens * price_input_per_mtok  / 1e6
        + max_output_tokens  * price_output_per_mtok / 1e6
        (no cache discount assumed — the discount is only known after the response)
Voyage:   input_bound_tokens * price_per_mtok / 1e6      (input-only)
```

**Why bytes rather than `curate::approx_tokens`.** That helper is `text.len().div_ceil(4)`, documented in the code as a crude English-prose average — it is an *average*, and averages are not bounds. Code, punctuation-dense text, and non-Latin scripts routinely exceed one token per four bytes. A reservation admitted just under the ceiling could then settle to an `actual_usd` above it, after the call had already gone out — and no amount of transactional care repairs an under-reservation. Both providers tokenize over UTF-8 bytes, and every token consumes at least one byte, so **byte count bounds token count**; `per_request_overhead_tokens` covers chat-template and control tokens that are not part of the payload text.

**Why a ~4× loose bound is affordable here.** The ceiling sums `COALESCE(actual_usd, estimated_usd)`, and each attempt settles to actual usage as soon as it returns. Inflation therefore applies only to the handful of attempts in flight at once (`max_concurrent_requests`, default 4), never to the day's accumulated total. Reserving generously costs a slightly earlier trip in the worst case and buys a ceiling that is actually a ceiling.

Settlement moves the row **downward** to real usage from a trustworthy payload, and never upward past its reservation without tripping the meter immediately: if a provider ever reports usage above the bound, that is a broken assumption, and the run should stop rather than continue quietly.

**Budget classes.** `budget_class` is required because publication and shadow work happen *within the same run*, so joining through `run_manifests.shadow` cannot classify an individual request — the dimension has to live on the row.

Classification follows the **purpose of the HTTP attempt**, never the operation's name and never whether its output happens to be reusable:

- `publication` — a call an **actively issue-producing `generate` invocation** needs: Stage A, Stage B, editorial summaries, world briefing, embeddings for that run's candidates, and a weekly profile rebuild triggered *inside* that run.
- `shadow` — anything whose only purpose is evaluating the new ranker, **including its embeddings**, and everything in a `--dry-run` invocation, which by definition publishes nothing.

`shadow_max_daily_usd` lives under `[curation.personalization]` and is deliberately provider-agnostic: it is applied **independently to each provider's ledger**, so shadow work is capped at that amount of DeepSeek spend *and* that amount of Voyage spend, not that amount in total. One number, because the intent is "shadow work stays small", not a per-provider allocation exercise.
- `maintenance` — `features backfill` and a standalone `profile rebuild`. (`features prune` spends nothing but takes the lock.)

v7 got this wrong in a way that reopened the very hole the reserve was added to close: it classed embeddings produced during a shadow run as `publication`-equivalent "because the cache is a shared asset". Cache reuse may make a later run cheaper; it does not make an evaluation request publication-critical. Phase A is *primarily* embeddings, so under that rule a shadow phase could consume the entire ceiling, reserve included, and then refuse the 05:30 run — while the plan claimed Phase A cannot affect the paper. A shadow run over a different date, a broad feature sweep, or an interrupted partial cache fill can all spend the reserve without producing the artifacts the live run actually needs.

The class travels in a typed `BudgetContext` threaded from the top-level command into provider orchestration. It is never inferred at the call site: an embedding batch does not know why it was asked for, and code that could promote itself will eventually do so by accident.

The provider's `max_daily_usd` caps **the sum of all three classes** — it is a runaway guard on the account, and a runaway in shadow code spends exactly the same money.

**A sub-limit is not a slice, so there is also a production reserve.** v6 said shadow "can never consume the production slice" while defining only a shadow cap inside a shared ceiling — which does not follow. With Voyage's `max_daily_usd = 0.25` and a $0.20 shadow cap, a shadow command that runs first can leave $0.05 for the 05:30 timer. Production-first *ordering* (§24) protects calls within one invocation and does nothing across invocations or across the UTC day, which is precisely the scenario Phase A creates. Since Phase A's premise is that shadowing does not change the paper, publication capacity is reserved explicitly:

```text
publication_reserve_daily_usd   per provider; capacity only `publication` may use

admit(publication)  iff  total + estimate <= max_daily_usd
admit(shadow)       iff  total + estimate <= max_daily_usd - publication_reserve_daily_usd
                    and  shadow_total + estimate <= shadow_max_daily_usd
admit(maintenance)  iff  total + estimate <= max_daily_usd - publication_reserve_daily_usd
```

Only `publication` may draw on the reserve, and it may draw on the whole ceiling — the reserve is a floor under the newspaper, not a quota against it. Defaults are chosen so the two limits agree rather than fight: Voyage `max_daily_usd = 0.25`, `publication_reserve_daily_usd = 0.05`, `shadow_max_daily_usd = 0.20`; DeepSeek keeps its existing top-level `max_daily_usd = 2.0` with `publication_reserve_daily_usd = 1.00`, comfortably above a typical run's ~$0.31.

The reserve is not released when the day's publication run finishes: a rerun, a late recuration, or a corrected issue all need it, and unspent budget is not a resource worth reclaiming for shadow work.

When in doubt, class **down**, not up: misclassifying publication work as `shadow` costs a slightly tighter shadow allowance, while the reverse lets evaluation work exhaust the capacity the paper depends on.

Rules:

- **The bucket is the UTC date of `reserved_at`**, derived inside the ledger writer rather than passed in by callers — the provider's billing day, not the issue's nominal date. Document this explicitly in the README, because "daily" otherwise reads as the reader's local newspaper day.
- **Every provider-using command writes to it**: `generate`, `features backfill`, `profile rebuild`, and any future one, each carrying its `BudgetContext`. `run_id` is nullable precisely so a non-run command still lands in the right bucket.
- **Reserve before dispatch, reconcile after.** The reservation row is committed *before* the request goes out, so a crash leaves the conservative estimate rather than nothing. On success the row settles with actual usage; on a failure with no usage payload it becomes `failed_estimated` and the estimate stands (§24).
- **The ceiling is `SUM(COALESCE(actual_usd, estimated_usd))` for the provider and today's UTC day**, evaluated before each reservation — once provider-wide, and once filtered to the reservation's `budget_class` when that class has a sub-limit. `runs.cost_usd`, `runs.voyage_*`, and the report remain rollups for human consumption.
- The ledger starts empty at migration; spend recorded before `0002` is not backfilled into it (nominal dates cannot be mapped to billing days). For a runaway guard, one day of under-counting at cutover is acceptable — state it rather than fake it.
- Retention: prune ledger rows older than 400 days along with the other prune paths.

**No `runs.mode` column.** `run_manifests.mode` is the single writable source of truth; anything needing the mode joins to it. Two independently writable copies would silently disagree and quietly corrupt the evaluator's mode filtering.

#### Run eligibility for evaluation

`RunStatus` is an existing closed vocabulary — `running | ok | degraded | failed | dry_run` (`src/report.rs:19-41`, written verbatim by `db::finish_run`). There is no `complete`, and this plan does **not** add one: a filter on `status = 'complete'` would exclude every run ever recorded, which would silently make Phase A's exit gate unreachable and every lifecycle test vacuous.

Define eligibility once and use it everywhere instead of copying SQL. This repository uses runtime `sqlx::query` with no query-fragment abstraction, so the implementable shape is a `db` method that executes the whole query — `async fn eligible_runs(&self, kind: EvalKind, from: Date, to: Date) -> Result<Vec<RunRef>>` — returning typed rows that every metric then works from. (A `sqlx::QueryBuilder<Sqlite>` helper is the alternative if a metric genuinely needs to push the predicate down into a larger join; either is fine, an invented `QueryFragment` type is not.) Stage-completeness filtering happens in Rust after typed decoding, never as SQL JSON paths.

| Purpose | Run status | Manifest |
|---|---|---|
| Production outcome metrics (rating rate, issue size, selection quality) | `ok`, `degraded` | `manifest_status = 'final'`, `dry_run = 0` |
| Shadow/admission diagnostics | `ok`, `degraded`, and `dry_run` when `--include-dry-runs` is passed | `manifest_status = 'final'` |
| `explain` | any, including `failed` with `--run-id` | any, including `provisional` — it shows whatever exists |
| Any metric | never `running`, never `failed` | never `provisional` |

**`degraded` is included on purpose, per-metric.** The application deliberately marks guardrail trips and best-effort stage failures `degraded` while still publishing a valid issue, so excluding those runs wholesale would throw away exactly the days the evaluator most wants: admission behavior, fallback behavior, issue size, and user ratings are all still meaningful. What is *not* meaningful is a metric computed over a stage that did not finish. So metric eligibility is decided per stage from `run_manifests.stage_completeness_json`:

- a Stage A metric requires `stage_a.skipped_budget == 0`,
- a facet metric requires `facets.succeeded / facets.attempted >= 0.9`,
- admission and recall metrics require only that the admission stage completed,
- ratings-based metrics have no stage requirement at all.

Report the excluded-run count alongside every metric so a suspiciously small denominator is visible rather than inferred.

### 7.7 Feed priors v2 — derived per run, not stored

The current attribution is asymmetric: credit goes only to `best_entry_id`'s feed, but reads take the max across all feeds.

**There is no `feed_priors_v2` table.** v3 of this plan proposed one, rebuilt with `DELETE; INSERT` from the rating endpoint and read by generation. That design cannot satisfy the `as_of` contract and races itself in two ways:

- A `replay` bounded at an earlier `as_of` would recompute the singleton table from a past rating set and **overwrite the live priors** — a historical diagnostic corrupting production state.
- `serve` is deliberately not lease-protected (§24.2), so a 👍 arriving mid-run could rebuild the table between two reads within one generation, or write a snapshot computed from a rating set that the run's own `as_of` excludes. Atomic replacement prevents a *partial* read; it does not prevent the *wrong snapshot* winning.

Instead, `rating_events` (§7.9) is the only canonical store, and priors are derived into an immutable in-memory `HashMap<FeedId, FeedPriorV2>` inside `PreferenceState`, bounded by the run's `as_of`. At this scale — tens to hundreds of events, each already carrying the feed set recorded at vote time — derivation costs milliseconds and removes an entire class of state bug. The resulting per-candidate `feed_affinity` is persisted in `candidate_rankings` like any other signal, so nothing is lost for `explain` or `evaluate`.

The legacy `feed_priors` table stays untouched and keeps serving the old prefilter path through Phases A–B; it is retired in Phase E along with `combined_score()`.

Rating credit allocation, computed in memory from the **feed set recorded on each rating event** (§7.9) rather than the article's current `sources_json`, which is overwritten on re-ingest and would otherwise let a later pickup by three more feeds retroactively re-split a months-old vote:

1. From `article.sources`, collect **distinct** `feed_id`s whose `SourceKind` is the ordinary direct `Feed` kind.
2. If there are one or more, split exactly 1.0 vote weight evenly across those distinct feeds.
3. If there are none, fall back to the `best_entry_id` feed — **even if it is a discovery feed**, since some credit signal beats none. Record this case in the rebuild log count so it can be audited.
4. Never give separate full credit to Scour, HN-frontpage, Reddit, or Lobsters discovery feeds for carrying the same story.

Candidate feed affinity is the **unweighted mean** of the Beta-smoothed rates of the *candidate's* distinct direct-feed sources (never the maximum), read from its current cluster — a candidate is being judged now, so current provenance is the right input. Rated *history*, by contrast, always uses the credit map frozen on each rating event (§7.9). If the candidate has no direct-feed source, use the `best_entry_id` feed's rate; if that feed is unknown, the signal is **absent** (NULL), not 0.5-as-a-number — absence is handled by §12.2.

Beta smoothing on weighted counts:

```text
rate = (up_weight + 1) / (up_weight + down_weight + 2)
```

An unseen feed is neutral at 0.5. An included-but-unrated article is **not** a downvote; exposure is not a label.

**No rebuild step, therefore no rebuild race.** Each run derives the map once, from ratings bounded by its own `as_of`, and holds it immutably for the rest of the run. Two runs, a replay, and a rating arriving mid-run cannot interfere with one another because there is no shared mutable aggregate to interfere with.

### 7.8 `scores` becomes a projection; `candidate_rankings` becomes the score history

Keep the `scores` table and keep writing it (§18.4 explains why this is load-bearing). Add only:

```sql
ALTER TABLE scores ADD COLUMN llm_reader_fit_score REAL;
ALTER TABLE scores ADD COLUMN assessment_version INTEGER NOT NULL DEFAULT 1;
```

`assessment_version = 1` means "`llm_score` is the old combined score"; `2` means "`llm_score` is `quality_score` and `llm_reader_fit_score` is populated". Readers that care about the distinction check the column; readers that only want a rough quality number can read `llm_score` in both regimes. This is what makes a partially-deployed binary safe: v1 and v2 rows coexist and are distinguishable, and no read path becomes ambiguous.

**`scores` is a current-value projection, not a history — and the churn rule must stop reading it as one.** v4 tried to give it observation time with `run_id` and `scored_at` columns, which does not work: the primary key is still `(article_id, run_date)`, so `db::upsert_score` still *overwrites*. A recuration of August 1 performed on August 19 replaces the August 1 observation with an August 19 one; a fidelity replay as of August 5 then correctly excludes the surviving row for being from its future, but the row it should have used no longer exists. The churn answer silently changes because of a recuration that happened afterwards. Provenance columns on an overwriting key are provenance about the survivor, not a history.

The history already exists: **`candidate_rankings` is keyed `(run_id, article_id)`, is append-only per run, carries `llm_quality_score`, and joins to `runs.started_at` for true observation time** (§7.5). So the churn rule reads from there:

```sql
-- Rank every eligible observation per article by OBSERVATION time, then keep the
-- article only if its most recent one is below the floor.
WITH ranked AS (
    SELECT cr.article_id,
           cr.llm_quality_score,
           ROW_NUMBER() OVER (
               PARTITION BY cr.article_id
               ORDER BY r.started_at DESC, cr.run_id DESC
           ) AS rn
    FROM candidate_rankings cr
    JOIN runs r ON r.id = cr.run_id
    WHERE cr.llm_quality_score IS NOT NULL
      AND r.started_at >= :observation_since   -- as_of - recent_rejection_lookback_days
      AND r.started_at <= :as_of
      AND r.status IN ('ok', 'degraded')
)
SELECT article_id FROM ranked
WHERE rn = 1 AND llm_quality_score < :floor;
```

Two things about this query are load-bearing, and the obvious simpler version gets both wrong:

- **The latest observation must actually be selected.** A bare `WHERE llm_quality_score < :floor` returns *every* qualifying low row, so an article scored 2.0 on Monday and rescored 7.0 on Friday stays suppressed forever on the strength of a superseded observation. `ROW_NUMBER() … ORDER BY r.started_at DESC, cr.run_id DESC` picks one row per article deterministically, including when two runs share a timestamp.
- **The window is anchored to `runs.started_at`, not `candidate_rankings.run_date`.** "Recently rejected" means *recently judged by the model*, not *associated with a recent nominal issue date* — the whole reason §7.8 exists is that nominal date is the wrong time axis. Anchoring to `run_date` would put a score observed one minute ago outside a seven-day window merely because the operator was recurating an old date, and would let a future-dated nominal issue fall inside it.

For the same reason, **prune `candidate_rankings` by `runs.started_at`**, never by `run_date`: pruning on the nominal axis would silently delete recent observations of old dates and reintroduce the defect through the retention path.

Requirements this creates:

- **Ranking snapshots are written by every run from Phase A onward, regardless of `personalization.enabled`.** The snapshot is a property of the run, not of the new ranking path; gating it behind the feature flag would leave the churn rule blind whenever the flag is off.
- In Phase A the recorded `llm_quality_score` is the legacy combined Stage A score (`assessment_version = 1`); from Phase C it is `quality_score`. Both are 0–10 and both are compared against the same floor, which is exactly the continuity §18.4 requires.
- `ranking_retention_days` (default 180) must exceed `recent_rejection_lookback_days` (default 7) by a wide margin; §30 adds the config validation.
- **There is no legacy `scores` fallback.** v6 proposed one for pre-`0002` dates, which would have quietly reintroduced both defects this section exists to fix: `scores` has no observation timestamp, so recency could only come from nominal `run_date` (wrong axis), and a legacy low row unioned into the result would suppress an article that a newer `candidate_rankings` observation had already cleared (wrong value). A projection must not compete with an observation.

  The cost of dropping it is bounded and, here, essentially zero: churn suppression is blind only to articles whose *sole* low score predates the migration, and only for `recent_rejection_lookback_days` (7) after it — and the production store holds roughly one issue of history (§2). A week of slightly weaker churn suppression at cold start is a better trade than a second, semantically weaker query path that outlives its usefulness.

  If a future migration lands against a database with real history, the correct bridge is a **one-time snapshot** of the legacy low set with an explicit expiry timestamp, consulted only for articles that have no `candidate_rankings` observation at all — never a live query against a mutable projection.

### 7.9 Append-only observation layer

C1 is one instance of a general problem: v4 promised that a fidelity replay is unaffected by anything that happened after `as_of`, while querying tables that overwrite in place. Three of them matter, and each is cheap to fix at this volume.

**`ratings` loses the pre-flip vote.** It is keyed `(issue_date, article_id)`, and `db::upsert_rating` overwrites both `vote` and `rated_at`. §13.1 called the `rated_at` reset acceptable for decay, and it is — but after a flip, the *original* vote is gone, so no `rated_at <= as_of` filter can reconstruct what the reader had actually told the system at that time.

**`issues` loses the earlier publication.** `db::upsert_issue` overwrites `generated_at` and `db::replace_issue_articles` deletes and reinserts the lineup, so republishing a nominal date erases the publication fact that existed at an earlier `as_of`. The §6.1 predicate `issues.generated_at <= as_of` then excludes the *replacement* without restoring the original — the previously-published exclusion silently changes for every replay of that period.

**`sources_json` loses the attribution.** Feed-prior derivation joins each rating to its article's *current* sources, and `db::upsert_article` overwrites that field on re-ingest. A story later picked up by three more feeds retroactively changes how a months-old rating's 1.0 credit was split.

```sql
CREATE TABLE rating_events (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    article_id        INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    issue_date        TEXT NOT NULL,
    vote              INTEGER NOT NULL CHECK (vote IN (-1, 1)),
    event_at          TEXT NOT NULL,
    -- the FINAL local attribution result at vote time, post-fallback (§7.7)
    feed_credits_json TEXT NOT NULL
);

CREATE INDEX idx_rating_events_article ON rating_events(article_id, event_at);
CREATE INDEX idx_rating_events_at ON rating_events(event_at);

CREATE TABLE publication_events (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    article_id   INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    issue_date   TEXT NOT NULL,
    run_id       INTEGER REFERENCES runs(id),
    published_at TEXT NOT NULL
);

CREATE INDEX idx_publication_events_article ON publication_events(article_id, published_at);
CREATE INDEX idx_publication_events_at ON publication_events(published_at);
```

Rules:

- `ratings` and `issues`/`issue_articles` stay exactly as they are and remain the fast current-value projections that `serve`, the EPUB builder, and the front page read. Nothing about publishing or rating changes shape.
- Every accepted vote appends a `rating_events` row *in the same transaction* as the `ratings` upsert. A flip appends a second row; nothing is ever updated or deleted.
- `feed_credits_json` stores the **finished attribution**, not its inputs:

  ```json
  { "credits_version": 1, "credits": { "42": 0.5, "77": 0.5 }, "via_fallback": false }
  ```

  It is a versioned typed structure like every other authoritative JSON field in this plan, validated on read: weights must be finite, non-negative, and sum to 1.0 within `1e-6`; an unparseable or unknown-version row is skipped for feed affinity with a warning and still counts toward `W_global`. Storing the *direct feed set* instead would have been insufficient, because §7.7 falls back to the `best_entry_id` feed when an article has no direct-feed source — so a discovery-only rating would still have had to consult mutable current article state to be reproduced, which is precisely the dependency the event row exists to remove. `via_fallback` records that the fallback fired, so §7.7's audit count survives replay.
- Every publication appends one `publication_events` row per pick **in one transaction with `upsert_issue` and `replace_issue_articles`**. Today those two are separate transactions (`pipeline::record_issue`), which can already leave the projections half-updated; adding events to only one of them would compound that. Republishing appends more rows; the earlier ones stand.
- **A publication event means "published and recorded", not "the file was momentarily visible".** `pipeline::generate` copies artifacts to the publish directory *before* `record_issue` runs, so a crash in that window leaves an EPUB reachable through the directory and OPDS with no event and no `issues` row. Closing that gap entirely would need a two-phase commit against the filesystem, which is not worth it here; instead, startup runs a **reconciliation check**: for each file in the publish directory with no matching `issues` row, log a warning naming the orphan and the date. The operator can rerun that date — which is idempotent — or delete the file. The semantics are stated so nobody later reads `publication_events` as an exposure log.
- **Temporal reads all follow one shape, in every mode:** for each article, take the latest event with `event_at <= as_of` (tie-broken `ORDER BY event_at DESC, id DESC`, since two votes can share a timestamp), and ignore articles with no such event. `PreferenceState` builds from `rating_events`, never `ratings`; `previously_published_ids(as_of, before_date)` reads `publication_events`, never `issue_articles`.
- **`live` and `recurate` use the same queries with `as_of = now`.** v5 said they "may read the projections directly", which is not the same question asked with a looser bound — it is a different question with different answers:
  - `ratings` is keyed `(issue_date, article_id)`, so an article that appeared in two issues and was rated in both yields **two projection rows and one latest event**. The projection path would double-count that reader's opinion.
  - `issue_articles` holds only the *current* lineup per date, so if a republish drops an article, the projection says it was never published — and the "never print the same story twice" rule, which is a hard exclusion, would let it back into the paper. `publication_events` correctly remembers that it ran.

  Two query paths would therefore give live and fidelity subtly different *product* semantics, not merely different time bounds, and only one of them would be covered by the fidelity tests. One path, one bound.
- Projections remain exactly what their names suggest: `ratings` backs the current-vote UI, `issues`/`issue_articles` back the current issue page, OPDS, and the EPUB build. Neither ever decides ranking history.
- Migration `0002` seeds both tables from the current projections in the Rust bootstrap (§7.4b): one `rating_events` row per existing rating using its `rated_at` and the article's current sources, one `publication_events` row per `issue_articles` row using its issue's `generated_at`. Pre-migration history is therefore *as good as the projection allows* — flips before the migration are unrecoverable, and that is stated rather than papered over.
- Volume: one row per vote and ~20 per issue. This is a few thousand rows a year.

The resulting contract, stated once so §34's criteria are checkable: **a fidelity replay is stable against votes flipped afterwards, dates republished afterwards, and articles rescored afterwards.** What it is still *not* stable against is `content_html` being overwritten on re-ingest, which changes what a stored embedding describes — a limitation §27.2 already records and which no amount of event logging fixes.

---

## 8. Module layout and core types

Exact current layout: `profile` is a directory (`src/curate/profile/mod.rs`, `src/curate/profile/themes.rs`) and `src/curate/editorial.rs` exists. Do not collapse them.

```text
src/curate/
├── embedding.rs       # Voyage client, vector serialization, embedding cache/fetch
├── facets.rs          # facet schema v1, parsing, cache orchestration
├── preference.rs      # rating-derived preference state, per-signal evidence, run-local feed priors
├── recall.rs          # union admission with per-retriever quotas
├── rank.rs            # normalization, blends, utility, cluster-cap diversification
├── prefilter.rs       # (existing) hard hygiene + cheap heuristic score
├── score.rs           # (existing) Stage A: quality + reader fit + facets
├── select.rs          # (existing) Stage B, no forced top-up
├── editorial.rs       # (existing) unchanged
├── llm.rs             # (existing) DeepSeek transport
└── profile/
    ├── mod.rs         # (existing) weekly rebuild + interest parsing
    └── themes.rs      # (existing) unchanged
```

### 8.1 Signal representation

Absence must be representable everywhere, so a missing signal can never be confused with a low one.

```rust
/// A raw signal value plus whether it exists at all for this candidate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Signal {
    Present(f64),
    Absent,
}

pub struct RankingSignals {
    pub heuristic: Signal,          // always Present
    pub social: Signal,             // Absent when the article has no social rows
    pub feed_affinity: Signal,      // Absent when no direct feed is known
    pub semantic_interest: Signal,  // Absent without an embedding
    pub embedding_preference: Signal,
    pub facet_preference: Signal,
    pub llm_quality: Signal,
    pub llm_reader_fit: Signal,
}
```

`social` deserves a note: `composite_social_score` currently returns exactly `0.0` for every article with no `social` rows, which is the majority on any day. Treating that as a *value* rather than an absence is what produces the failure in §12.1. Distinguish "no social rows" (Absent) from "an HN post with 1 point" (Present(small)).

```rust
pub struct ArticleEmbedding {
    pub article_id: ArticleId,
    pub model: String,
    pub dimension: usize,
    pub values: Vec<f32>,     // unit-normalized, all finite
    pub input_hash: String,
}

pub struct PreferenceState {
    pub as_of: Timestamp,
    /// Decayed rated articles that have a compatible embedding.
    pub rated: Vec<RatedExample>,          // { article_id, vote, weight, embedding }
    pub evidence: EvidenceWeights,         // W_global / W_embedding / W_facet / W_feed (§14.1)
    pub facet_stats: FacetStats,
    /// Derived per run from `rating_events`, never from a stored aggregate or `ratings` (§7.7).
    pub feed_priors: HashMap<FeedId, FeedPriorV2>,
    pub gates: EvidenceGates,              // which signals are active, and at what ramp
}

pub struct EvidenceWeights {
    pub global: f64,     // all decayed ratings — exploration maturity and telemetry only
    pub embedding: f64,  // ratings whose article has a compatible embedding
    pub facet: f64,      // ratings whose article has usable scored facets
    pub feed: f64,       // ratings attributable to at least one feed
}
```

Use typed fields for core signals; keep `HashMap<String, f64>` only inside the serialized `explanation_json`.

---

## 9. Voyage client

### 9.1 Configuration

```rust
pub struct VoyageConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub output_dimension: usize,
    pub batch_size: usize,
    pub max_concurrent_requests: usize,
    pub max_input_chars_per_article: usize,
    pub max_input_chars_per_batch: usize,
    pub price_per_mtok: f64,
    pub max_daily_usd: f64,
    pub publication_reserve_daily_usd: f64,
    pub embedding_retention_days: u32,
}
```

Validate: dimension ∈ {256, 512, 1024, 2048}; `1 <= batch_size <= 1000`; `1 <= max_concurrent_requests <= 16`; char caps > 0; price and budget non-negative.

Note for the operator, in `config.example.toml` and the §7.1 docs: the root `Config` deliberately does **not** use `#[serde(deny_unknown_fields)]` (so bare `DAILY_EPUB_SECRET` passes through), so a `[voyages]` typo is silently ignored and defaults apply. The startup log must print the resolved `voyage.enabled`, `model`, and `output_dimension` so a typo is visible in one line.

### 9.2 Transport

Mirror the existing `ChatBackend` seam so tests never touch the network:

```rust
pub trait EmbeddingBackend: Debug + Send + Sync {
    fn embed<'a>(&'a self, req: EmbeddingRequest)
        -> BoxFuture<'a, Result<EmbeddingResponse, EmbeddingError>>;
}
```

Request body:

```json
{
  "input": ["...", "..."],
  "model": "voyage-4-lite",
  "input_type": "document",
  "truncation": true,
  "output_dimension": 512,
  "output_dtype": "float"
}
```

Retry network failures, 429, and 5xx with bounded exponential backoff; do not retry ordinary 4xx. Voyage failures are never fatal to issue generation. Map response embeddings back to inputs **by response index**, and assert the response length equals the request length before doing so.

### 9.3 Batching and concurrency

- At most `batch_size` inputs per request (default 32).
- Each article's text is capped at `max_input_chars_per_article` on a **Unicode character boundary** (`char_indices`, never byte slicing).
- A batch is split further if its total characters would exceed `max_input_chars_per_batch` (default 900,000 ≈ 225k tokens, comfortably under the 1M aggregate limit without adding a tokenizer dependency).
- `truncation = true` remains the server-side safety valve; count and log server truncations (counts only, never article text).
- Run batches with **bounded concurrency**: `futures::stream::iter(batches).buffer_unordered(voyage.max_concurrent_requests)`. `futures 0.3.34` is already a dependency.
- Check the budget meter **before spawning each request**, not between serial iterations, and make the check-and-reserve atomic (§24).
- A failed batch logs and leaves those embeddings missing; other batches continue.

### 9.4 Usage accounting

A Voyage usage meter separate from the DeepSeek `UsageMeter`, tracking input tokens, estimated cost, and ceiling-trip state. Both meters reserve and settle through the **`provider_usage` ledger** (§7.6) — per attempt, classed, and bucketed by UTC billing day — rather than preloading `runs` by nominal date. `runs.voyage_input_tokens` / `runs.voyage_cost_usd` remain per-run rollups for the report. The two providers keep independent ceilings; `max_daily_usd` must not silently become "all providers".

---

## 10. Article embedding input

### 10.1 One deterministic embedding document

```rust
const EMBEDDING_DOCUMENT_VERSION: u32 = 1;
fn embedding_document(article: &Article, cap_chars: usize) -> String
```

V1 format:

```text
Title: <title>

<full extracted article plain text, capped at cap_chars on a char boundary>
```

**`Source:` and `Author:` are deliberately excluded**, correcting v1 of this plan, which forbade ranking metadata in the vector and then included the feed title. Feed title *is* provenance metadata, and including it has two concrete costs: the rating-preference signal partly re-encodes "feeds the reader upvotes", double-counting the separate `feed_affinity` signal that §7.7 went to some trouble to de-bias; and diversification degrades, because two unrelated posts from the same blog become artificially similar and the second gets suppressed as redundant. Diversification is the one calculation where topical purity actually matters.

Use `curate::html_to_text()` as the base normalizer and collapse whitespace. Never include social score, ratings, feed prior, or LLM rationale. Use `input_type = "document"`. Prefer full extracted text up to the cap rather than an opening excerpt — embedding is exactly where more of the article is cheap and useful.

### 10.2 Hash

```text
input_hash = sha256("v" + EMBEDDING_DOCUMENT_VERSION + "\n" + embedding_document)
```

Model and dimension are already in the primary key; the version-prefixed content hash covers format changes.

---

## 11. Standing-interest semantic matching

230 standing interests exist today and require zero ratings, which makes this the highest-value signal in the entire plan for the first several months (§2).

### 11.1 Interest query embeddings

For each unique interest from `profile::parse_interests()`, embed with `input_type = "query"` and cache in `interest_embeddings`.

Two text formats, both versioned so they can be compared without a cache wipe:

- `text_version = 1`: `"Articles about: <interest>"` (v1 of this plan's format).
- `text_version = 2`: `"<interest>"` — the bare name.

Ship `text_version = 2` as the **default**. `input_type = "query"` already causes Voyage to prepend its own retrieval instruction server-side, so `"Articles about: "` is a second, redundant instruction that is byte-identical across all 230 interests, pulling every interest vector toward every other one and compressing exactly the top1-vs-top3 gap the score depends on. Keep v1 available so §27 can measure the difference: it costs 230 embeddings, i.e. nothing.

### 11.2 Per-article interest score

Compute the full similarity matrix (230 interests × ~400 articles × 512 dims ≈ 47M multiply-adds — a few tens of milliseconds; no optimization needed).

**Z-score each interest's similarity across the day's candidate pool before aggregating.** The interest list is dominated by broad single words (`Nature`, `History`, `Space`, `Engineering`, `Science`) alongside genuinely specific ones (`Gaussian Splatting`, `Writerdeck`, `tmux`). Broad terms have high *average* similarity to everything, so raw top-1 similarity mostly measures "how generic is this article" and almost always resolves to the same handful of broad interests. The valuable signal — "this article is *unusually* close to Gaussian Splatting" — is precisely what max-of-raw-cosine destroys.

```text
for each interest i:
    mu_i    = mean over the day's candidates of sim_i(a)
    sigma_i = stddev over the day's candidates of sim_i(a)      # floor at 1e-3
    z_i(a)  = (sim_i(a) - mu_i) / sigma_i

top1 = max_i z_i(a)
top3 = mean of the three largest z_i(a)
semantic_interest_score = 0.70 * top1 + 0.30 * top3
```

The matrix is already computed, so z-scoring is free. Fall back to raw similarity (and log it) when the day's candidate count is below 20, since z-scores are meaningless on a tiny pool.

Persist `semantic_interest_score`, the raw top-1 similarity (`semantic_interest_raw_top1`, for future absolute calibration), and the top three interests with both their z and raw values in `explanation_json`.

This is a **positive recall signal, not a filter**. An outstanding article outside the standing interests must still survive via the heuristic, quality, or exploration paths.

---

## 12. Signal normalization contract

This section is normative. Two of the three critical defects found in review were normalization defects, and both produce output that looks completely plausible.

### 12.1 Mid-rank percentiles, and why ID tiebreaking must not happen here

Convert each continuous signal's daily values to percentile ranks in `[0,1]` using **mid-rank (average-rank) percentiles**:

```text
p(x) = (count_below(x) + (count_equal(x) + 1) / 2) / n_present
```

**Equal raw values must receive equal normalized values.** v1 of this plan said "tie breaking must be stable by article ID", which assigns *distinct* percentiles to *equal* values. That is correct for output determinism and catastrophic inside a normalizer: percentile-ranking 400 tied zeros with an ID tiebreak produces a perfect ascending-article-ID ramp from 0.0 to 1.0. Since `articles.id` is `AUTOINCREMENT` assigned in `persist_articles` iteration order, low IDs are systematically older articles and articles from feeds that happened to sort earlier — so the blend would rank on article age and feed ordering while looking entirely healthy. The two signals most likely to be degenerate are exactly the ones that would have been affected: `embedding_preference` (identically zero whenever there are no ratings — i.e. today) and `social_score` (identically zero for most articles every day).

Article ID may break ties **only in final output ordering**, never inside the normalizer.

Degenerate cases:

- `n_present < 2` → every present value normalizes to `0.5`.
- All present values equal → every present value normalizes to `0.5`.

### 12.2 Missing values

- Missing values are **excluded from the empirical CDF** — they do not occupy rank positions.
- A missing signal normalizes to the neutral value `0.5` **and** is recorded `present: false`.
- A signal that is `Absent` for a candidate is dropped from that candidate's blend entirely (§12.3), so its `0.5` is a display value, not a scoring input.
- Availability flags are persisted per candidate in `explanation_json.present`.

This is what makes an outage degrade instead of penalize: during a Voyage outage, uncached articles must not be systematically demoted relative to cached ones.

### 12.3 Blending: weighted mean over present *and active* signals

Every blend in this plan (§16.3 preliminary score, §19 utility) is computed as:

```text
active(s)   = present(s) AND gate_ramp(s) > 0
w_eff(s)    = w_config(s) * gate_ramp(s)          for active s
blend       = sum(w_eff(s) * normalized(s)) / sum(w_eff(s))
```

If no signal is active (which cannot happen — the heuristic signal is always present and ungated), fall back to the heuristic percentile.

v1 of this plan did exactly this *inside* facet preference ("normalize by the sum of weights actually present") and then failed to do it for the outer blends. On a cold-start day that left 50% of the pre-Stage-A score constant or noise, silently compressing the heuristic, interest, and social signals to half their intended influence.

Persist `w_eff` for every candidate in `explanation_json.effective_weights` so `explain` and `evaluate` can see what actually ran.

### 12.4 Scale conversions

Two signals are **not** percentiled, because their zero points are absolute and percentiling would erase "today was a weak day":

- Facet preference is approximately `[-1, 1]`; map to `[0, 1]` with `(x + 1) / 2` and use that directly.
- LLM quality and reader fit are `[0, 10]`; divide by 10 and use that directly.

Everything else **is** percentiled per §12.1: heuristic, social, semantic interest, embedding preference, feed affinity.

---

## 13. Preference learning from ratings

Build a `PreferenceState` at the start of every run, bounded by `as_of` (§6.1).

```toml
[curation.personalization]
rating_lookback_days = 90
rating_half_life_days = 45
```

### 13.1 Time decay and vote timestamps

```text
weight(age_days) = 0.5 ^ (age_days / half_life_days)
```

Age is computed from the event time of the latest `rating_events` row at or before `as_of` (§7.9) — behavioral recency, not publication recency. A flip appends a new event, so it resets recency, which is correct: a flip *is* a fresh behavioral signal. Because events are appended rather than overwritten, a fidelity replay bounded before a flip still sees the original vote, which the `ratings` projection alone could not provide.

### 13.2 Signed nearest-neighbor preference (replaces v1's centroids)

v1 of this plan reduced all upvotes to one unit-normalized positive centroid and all downvotes to one negative centroid. For a reader with 230 standing interests spanning Rust, local Boston reporting, books, e-ink, and outdoors, a single average vector is a poor representation: a niche cluster is only weakly similar to the global mean, so the signal works *against* the plan's own headline goal of rescuing a quiet post in a favored niche. The difference of two class centroids is also just a fixed-weight naive classifier — it weights every embedding dimension equally.

Use a **signed, time-decayed top-k neighbor signal**. At a few hundred rated articles this is simpler than centroid maintenance, preserves multiple taste modes, and is directly explainable ("similar to these three things you upvoted").

```text
Given candidate embedding e(x):
  For each rated example i in the window with a compatible embedding:
      s_i = clamp(dot(e(x), e_i), -1, 1)
  P = the k highest s_i among upvotes      (k = neighbor_k, default 5)
  N = the k highest s_i among downvotes

  positive_similarity = sum(w_i * s_i for i in P) / sum(w_i for i in P)      # Absent if no upvotes
  negative_similarity = sum(w_i * s_i for i in N) / sum(w_i for i in N)      # Absent if no downvotes

  embedding_preference_raw = pos_or_0 - negative_coefficient * neg_or_0      # default 0.75
```

The signal is `Absent` unless at least one rated article with a compatible embedding exists. Persist `positive_similarity`, `negative_similarity`, the raw score, and the top three contributing neighbors (article IDs and similarities) in `explanation_json.nearest_upvotes`.

**Do not apply a whole-run confidence multiplier to this score.** v1 of this plan multiplied every candidate's raw score by `W / (W + 6)`, a single scalar for the entire run. Multiplying every candidate by the same positive constant is a strictly monotone transform, so it is erased by percentile normalization and by top-K retriever selection — the two things that consume the signal. The damping had no effect anywhere it was used, which is the exact failure it was written to prevent. Sparse evidence must instead damp the **blend weight** (§14).

### 13.3 Facet preference statistics

For each **scored** facet value (§15.2), aggregate decayed up/down weight:

```text
rate    = (u + 1) / (u + d + 2)              # Beta(1,1)
support = (u + d) / (u + d + facet_support_k)   # facet_support_k default 4
effect  = (rate - 0.5) * 2 * support         # roughly -1 .. +1
```

A facet **value** contributes only if `u + d >= facet_min_observations` (default 3); otherwise that dimension is skipped for the candidate. A facet **dimension** contributes only if at least one of its values clears the gate.

Per candidate: compute one effect per dimension, average multi-value dimensions (e.g. `tones`) to a single effect *before* they contribute, then take the weighted mean over dimensions that are actually present — so an article with three tones does not get triple weight.

V1 scored-dimension weights (all configurable, all logged into the manifest):

```text
format      1.00
depth       1.00
evidence    1.00
commerciality 1.00
```

Topic affinity is deliberately **not** a scored facet dimension: it is the embedding's job, and duplicating it here would double-count. Free-form `specific_topics` are explanation-only in V1 — synonym fragmentation makes exact matching useless.

If no dimension clears its gate, `facet_preference` is `Absent`.

### 13.4 Missing historical features

A rating is only useful for preference learning if its article has the corresponding feature. §26 provides a backfill path. During normal generation, missing historical features must never fail the run — and, critically, they must reduce evidence **for the specific signal they belong to**, which is what the per-signal weights of §14.1 measure. A rating whose article has facets but no embedding raises `W_facet` and `W_global`, and leaves `W_embedding` untouched.

---

## 14. The evidence ladder

This is the mechanism that keeps a nearly empty rating store from injecting noise into the paper, and it replaces v1's non-functional confidence damping.

Let `W_s` be the decayed rating weight that is *usable by signal `s`* (§14.1). All four values are recorded in `run_manifests.evidence_weights_json`.

```toml
[curation.personalization]
evidence_floor = 5.0      # below this, rating-derived signals are inactive
evidence_full  = 20.0     # at or above this, they carry full configured weight
```

### 14.1 Evidence is per signal, not global

**A single global `W` measures the wrong thing.** A rating only teaches the embedding signal something if its article *has* a compatible embedding; it only teaches the facet signal something if that article has usable scored facets. Those sets differ, and they differ most exactly when things have gone wrong: after a provider opt-out (§25.1 articles have no embedding by design), a partial backfill, a model or dimension change that invalidates the cache, or a run of facet parse failures.

Under v3's single `W`, nineteen ratings whose articles have no embeddings plus one that does would produce `W = 20` and hand **full configured weight** to an embedding-preference signal learned from a single example. Presence-aware blending does not catch this: one compatible example is enough to make the signal `Present`, and the unrelated global `W` then opens the gate completely. That is the sparse-evidence failure the ladder exists to prevent, reintroduced through the back door.

Maintain four separate weights, each the decayed sum over the ratings that can actually inform that signal:

```text
W_embedding = decayed weight of ratings whose article has a compatible embedding
              (same model + dimension as this run)
W_facet     = decayed weight of ratings whose article has usable scored facets
              (same facet schema version)
W_feed      = decayed weight of ratings successfully attributed to at least one feed (§7.7)
W_global    = decayed weight of all ratings — telemetry and exploration maturity only
```

```text
gate_ramp(s) = clamp((W_s - evidence_floor) / (evidence_full - evidence_floor), 0, 1)
gate_ramp(non_rating_derived) = 1.0
```

Each rating-derived signal is gated by **its own** `W_s`: `embedding_preference` by `W_embedding`, `facet_preference` by `W_facet` (in addition to its per-value support gates, §13.3), `feed_affinity` by `W_feed`. Exploration maturity (§17) uses `W_global`, since exploration is about how confident the ranker is overall, not about any one feature.

Persist all four in `run_manifests.evidence_weights_json` and echo the relevant one into each candidate's `explanation_json.notes`. When `gate_ramp = 0` the signal is inactive, is excluded from the blend, and its weight is redistributed proportionally across the active signals (§12.3). Additionally:

- Retriever quotas for `embedding_preference` and `feed_affinity` are zero while their gate is zero (§16.2) — there is nothing to retrieve *by*.
- Exploration reservation ramps **up** with evidence, not down (§17).

Required regression test (§31.4): **20 total ratings of which only one is embedding-backed leaves the embedding gate near its floor**, not fully open.

### 14.2 Day-one behavior

`W_* ≈ 0`, so the pre-Stage-A blend reduces to heuristic + semantic interest + social, renormalized — a genuine improvement over today's prefilter (semantic interest is new and strong) without pretending to know the reader's taste. Log the state explicitly at info level so it is never a mystery:

```text
personalization: W_global=0.0 (0 ratings) · W_emb=0.0 · W_facet=0.0 · W_feed=0.0
                 — embedding/facet/feed signals inactive; ranking on heuristic + interests
```

And log it again whenever the weights diverge materially, which is the symptom of a feature-coverage problem rather than a rating shortage:

```text
personalization: W_global=22.0 (31 ratings) but W_emb=3.1 — 24 rated articles lack a
                 compatible embedding; embedding preference is still ramping. Run
                 `features backfill --rated-only`.
```

---

## 15. Facets

### 15.1 Facets ride inside Stage A in V1

v1 of this plan added a dedicated DeepSeek facet-extraction stage over ~240 articles per day, before Stage A. Drop it. Extract facets as additional fields in the **existing Stage A call** over the 120 admitted candidates.

Reasons, in order of weight:

1. **There is nothing to score with.** Facet preference requires ratings; with `W ≈ 0` the facet-preference signal is gated off entirely (§14), so a facet stage placed *before* the Stage A cut would spend 15–20 extra sequential API round-trips per day to compute a signal weighted at zero.
2. **Wall clock.** `score_all` batches serially (a plain `for` over `chunks` in `src/curate/score.rs`), the job runs on a 05:30 timer, and Stage A prompts are already growing from a 200-word excerpt to a ~450-word three-part sample. Adding a second serial LLM stage to the critical path is a real delivery risk for zero day-one benefit.
3. **Facets remain fully useful where they are.** Utility scoring (§19), Stage B rendering (§21.1), `explain`, and the weekly profile prompt (§22) all happen at or after Stage A.

A dedicated pre-Stage-A facet stage is a **Phase-later option**. Its trigger cannot be an admission metric, since facet preference is forbidden from admission until such a stage exists (§16.4) — there is nothing to measure. The trigger is an **offline counterfactual evaluation** (§6.2, `evaluate --counterfactual-features`): re-rank historical eligible sets with facet preference added to the preliminary blend, using facets backfilled for the whole candidate set, and check whether the admission cut measurably improves. Keep `facets.rs` structured so the extraction path can be called from either site.

### 15.2 Facet schema v1: small enough to actually estimate

v1 of this plan defined 11 dimensions over ~84 controlled values while acknowledging in the same paragraph that enums must be "small enough that ratings accumulate statistical support". With one issue published, the median facet value would have zero observations essentially forever; a single observation yields `effect ≈ 0.067`, indistinguishable from noise.

**Scored dimensions — 4 dimensions, 16 values:**

```text
format:        reported_news | analysis_essay | how_to_technical |
               first_hand_account | announcement_roundup
               # postmortems, incident write-ups, and case studies are
               # first_hand_account (usually with evidence = first_hand);
               # release notes, link roundups, and launches are announcement_roundup
depth:         brief | standard | deep
evidence:      first_hand | original_reporting | data_or_experiment |
               synthesis | speculative
commerciality: none | vendor_educational | promotional
```

**Descriptive-only dimensions** — extracted, stored, rendered to Stage B, fed to the weekly profile prompt, shown in `explain`, but **not** used in numeric scoring in V1:

```text
topic_group:     software_engineering | ai_ml | science_space | culture_arts |
                 books_writing | games | hardware | internet_web |
                 business_economics | politics_policy | boston_new_england |
                 outdoors_lifestyle | history | other
technicality:    nontechnical | light | intermediate | advanced
tones:           0..=2 of: neutral | analytical | conversational | reflective |
                 skeptical | enthusiastic | humorous | polemical | literary
locality:        boston_new_england | us | international | not_applicable
specific_topics: 0..=3 normalized short noun phrases
```

Descriptive fields cost a little prompt budget and pay for themselves immediately in the profile prompt, where the LLM does pattern recognition rather than parameter estimation, and cardinality is not a problem.

Dropped from v1 entirely: `stance`/`stance_target` (rhetorical stance is ambiguous to annotate, risks drifting toward ideology inference, and has no scoring role), `audience` (near-duplicate of `technicality`), `temporal_orientation` (largely derivable from publication timing and format).

Schema v2 — the full vocabulary, with technicality and topic_group promoted to scored dimensions — is the documented next step once there are ~300 ratings (§35).

Rust representation uses `#[serde(rename_all = "snake_case")]`. Cardinality bounds (`0..=3`, `0..=2`) are **not** enforced by serde — validate after deserialization and truncate with a warning.

Do not include "quality" or "good/bad" as a facet: Stage A owns editorial quality, and facets must describe what the article *is*, so ratings can learn which kinds of article the reader likes.

### 15.3 The taste-profile contamination tradeoff, stated explicitly

Stage A's system prompt carries the reader taste profile for prefix-cache efficiency, so facets extracted inside Stage A are nominally profile-dependent while being cached as if globally descriptive.

Decision: **accept the dependence, mitigate it, and measure it.**

- The facet instruction states explicitly that facet fields are **descriptive, not evaluative**, and must not be influenced by whether the reader would like the article.
- `article_facets.profile_version` records provenance.
- `profile_version` is **not** part of the cache identity — making it so would invalidate every facet weekly and roughly double facet spend for fields (format, depth, evidence, commerciality) that are close to objective.
- §27.2 adds a **facet stability metric**: re-extract facets for a fixed sample of 30 articles under a changed profile version and report per-dimension agreement. If agreement on any scored dimension falls below 85%, split facets into a dedicated profile-free extraction call — the escape hatch that §15.1 keeps the code shaped for.

### 15.4 Representative text sampling

One deterministic helper, shared by Stage A (and any future facet stage):

```rust
const EXCERPT_FORMAT_VERSION: u32 = 1;
pub fn representative_excerpt(text: &str, per_segment_words: usize) -> String
```

V1 behavior with `per_segment_words = 150`:

- Convert the full extracted body to plain text.
- If ≤ ~450 words, use the whole text with no separators.
- Otherwise: first 150 words, 150 words centered on the midpoint, final 150 words.
- Insert visible separators `[BEGINNING]`, `[MIDDLE]`, `[END]`.
- Split on word boundaries; never split UTF-8 unsafely.

This is materially better evidence than the introduction alone and keeps prompts bounded.

### 15.5 Facet cache

Cached by the full primary key `(article_id, schema_version, model, prompt_version, input_hash)`. A rerun must not re-spend tokens on unchanged articles. Stage A already skips articles whose facets are cached *for facet purposes* — but note it still scores them, because quality/fit are per-run judgments while facets are content-derived. Only the facet fields are reused.

---

## 16. High-recall union admission (~400 → 120)

Replaces "sort one heuristic score, truncate at `prefilter_keep`".

### 16.1 Hard exclusions stay hard

Before admission, keep today's hard behavior, now `as_of`-bounded:

- already published in an issue **before** `run_date` (`excluded_reason = published_before`),
- explicit blocked domains (`blocked`),
- obvious non-articles removed earlier in the pipeline,
- recently-rejected churn: LLM quality `< recent_rejection_score_floor` within `recent_rejection_lookback_days` (`churn_recent_reject`), except auto-includes.

Both churn constants move from `const` (`STALE_LOW_SCORE`, `STALE_LOOKBACK_DAYS` in `src/curate/prefilter.rs`) into `[curation]` config, since §16.1 and §27 both want to tune them. Every hard-excluded article still gets a thin `candidate_rankings` row carrying only its keys and `excluded_reason` (§7.5), which is what makes acceptance criterion 12 testable.

`always_include_feeds` remain mandatory candidates.

### 16.2 Retrievers and quotas

Compute all cheap signals for every eligible article (§8.1), normalize (§12), then fill `stage_a_keep` (default 120) slots in this order:

| Order | Retriever | Quota | Active when |
|---|---|---|---|
| 1 | `auto_include` | uncapped | always |
| 2 | `semantic_interest` | `quota_interest` = 20 | embeddings available |
| 3 | `heuristic` | `quota_heuristic` = 20 | always |
| 4 | `embedding_preference` | `quota_embedding` = 20 | `gate_ramp > 0` |
| 5 | `feed_affinity` | `quota_feed` = 5 | `gate_ramp > 0` |
| 6 | `exploration` | `exploration_reserve` (§17) | `W >= exploration_floor` |
| 7 | `blend_fill` | remaining slots | always |

Each retriever contributes its top-N by its own signal, skipping articles already admitted. Remaining slots go to the highest preliminary blend score (§16.3). An article records **every** retriever that would have admitted it in `admitted_by`, and the first one that actually did as `admitted_by[0]`.

This is a union, not a weighted sum, on purpose: a personally exceptional article needs only one strong path to survive the only irreversible cut. Inactive retrievers release their quota to `blend_fill` — on day one that means 20 interest + 20 heuristic + 80 blend-fill.

### 16.3 Quality floors on the semantic paths

Dense retrieval against short queries concentrates on short documents. A 60-word "Rust 1.94.0 released" stub will out-score a 3,000-word essay that discusses Rust among other things, because the essay's vector is diluted across many topics. Without a floor, the two semantic retrievers systematically over-admit exactly the class Stage A is told to punish and that `PENALTY_TITLE_PATTERNS` already exists to catch.

Admission **via `semantic_interest` or `embedding_preference` only** additionally requires:

```toml
semantic_admission_min_words = 250
```

and `!prefilter::looks_like_roundup(title)`. An article failing these can still be admitted via `heuristic`, `auto_include`, or `blend_fill` — the floor gates a retriever, not the article. None of the plan's motivating examples are affected: a "quiet 900-word post from an obscure feed" clears 250 comfortably.

### 16.4 Preliminary blend

Computed over the whole eligible set (§12.3 rules apply — these are *configured* weights, renormalized over present and active signals):

```text
0.35 semantic standing-interest match
0.25 heuristic quality proxy
0.23 embedding preference
0.09 feed affinity
0.08 social proof
```

**`facet_preference` is deliberately absent from this blend**, and must not be added back in V1. In V1 facets are produced *by* Stage A, so at admission time a facet row exists only for articles that already went through Stage A on a previous day — and since the 26-hour ingest window overlaps consecutive days, that set is non-empty and non-random. Presence-aware renormalization (§12.3) is the right tool for an *outage*, where missingness is unrelated to the candidate; it is the wrong tool for **informative missingness**, where having the feature at all is a consequence of previously surviving the very cut now being computed. Including it would make prior admission an input to the next day's admission — a self-reinforcing incumbency advantage that no new article can compete against, and it would make the admission formula depend on cache history rather than only on the candidate and the declared `as_of` evidence.

Facet preference therefore applies **only post-Stage-A**, in utility (§19), where every scored candidate had the same opportunity to obtain facets. It may be promoted into admission only if a later dedicated pre-admission facet path gives uniform coverage across the eligible set (§35).

On day one only interest, heuristic, and social are active, so this renormalizes to 0.51 interest / 0.37 heuristic / 0.12 social — a deliberate and legible bet on the 230 curated interests.

Auto-includes bypass the cut entirely. Social proof must not exceed this weak role: Stage A judges quality separately, and HN popularity should no longer be counted three times.

---

## 17. Exploration

Exploration prevents preference lock-in without making the paper noisy.

**Exploration ramps up with evidence, not down.** v1 of this plan reserved 20 slots unconditionally, but its own predicate ("from a feed with low rating evidence **or** semantically outside the dense region of recent positive ratings") is universally true when there are no ratings — so on day one it would hand 20 admission slots, a guaranteed shortlist reservation, and Stage B exposure to articles chosen by a hash, precisely during the phase when the point is to measure whether the new ranker beats the old one. When the system knows nothing, *everything* is exploration; a dedicated reservation adds pure noise.

```toml
exploration_max = 8
exploration_floor = 15.0     # W below this ⇒ no exploration reservation
exploration_full  = 30.0     # W at which the full reservation is granted
```

```text
exploration_reserve =
    round(exploration_max * clamp((W - exploration_floor) / (exploration_full - exploration_floor), 0, 1))
```

The denominator is the ramp **width**, not `exploration_full` itself. (Written the other way — v2's `(W - floor) / full` — the reserve at `W = 20` would be `8 × 5/20 = 2`, reaching its configured maximum only at `W = 35`, which contradicts both the parameter names and the ladder's "full at `evidence_full`" semantics.)

`exploration_full` is deliberately a **separate threshold from `evidence_full` (20.0)**, not an alias: learned signals should carry full weight as soon as they are estimable, whereas exploration is most useful once the ranker is confident enough to be at risk of lock-in. So learned weight saturates at `W = 20` and exploration saturates later, at `W = 30`. Boundary values: `W ≤ 15 → 0`, `W = 20 → 3`, `W = 25 → 5`, `W ≥ 30 → 8`.

An exploration candidate must be:

- not auto-included, not hard-excluded,
- not already admitted by another retriever,
- **outside the dense positive region**, defined concretely as `positive_similarity < the 25th percentile of the day's candidate distribution of positive_similarity` (or, if `embedding_preference` is inactive, from a feed with `up_weight + down_weight < 1.0`),
- above `semantic_admission_min_words` and `heuristic percentile >= 0.30`, so the system does not explore obvious junk.

Selection among qualifying candidates is deterministic: sort by `hash(exploration_salt, run_date, article_id)` and take the first `exploration_reserve`. Same date ⇒ same picks; different dates rotate.

Exploration reserves **exposure, not publication**. Stage B may reject any of them. Persist `exploration_candidate = 1`.

---

## 18. Stage A: quality, reader fit, and facets

### 18.1 Response shape

```json
{
  "articles": [
    {
      "id": 123,
      "quality_score": 8.5,
      "reader_fit_score": 7.0,
      "category": "Tech & Engineering",
      "rationale": "first-hand failure analysis with concrete measurements",
      "is_paywalled_guess": false,
      "facets": {
        "format": "first_hand_account",
        "depth": "deep",
        "evidence": "first_hand",
        "commerciality": "none",
        "topic_group": "software_engineering",
        "technicality": "advanced",
        "tones": ["analytical"],
        "locality": "not_applicable",
        "specific_topics": ["postgres", "replication lag"]
      }
    }
  ]
}
```

Introduce `LlmArticleAssessment` as a new type rather than mutating `LlmScore` in place, and give it `assessment_version = 2`. Keep `LlmScore` as the v1 shape for as long as any read path needs it.

Parsing stays tolerant in the same way `score.rs` already is: a malformed item must not cost the rest of the batch, and a malformed `facets` object must not discard a valid `quality_score`. Validate enum values against the offered vocabulary and drop unknown values to `None` with a debug log rather than failing the item.

**Every enum token in this example, in the prompt, and in every test fixture must be a member of the §15.2 vocabulary.** The tolerant parser makes a wrong token silently become `None`, so an example that uses one teaches the model — and the implementer copying it — to emit a value that is then discarded. §31.7 requires a test that round-trips every token appearing in prompt examples through the parser. (Note in particular that a postmortem is `format = first_hand_account` with `evidence = first_hand`; `postmortem_case_study` is *not* in the v1 vocabulary, which is exactly the collapse §15.2 intends.)

### 18.2 Quality rubric

`quality_score` judges substance; originality and first-hand evidence; clarity and writing quality; depth appropriate to the subject; whether the article rewards the time spent reading it.

Tell the model explicitly:

- do not award quality merely for length,
- do not award quality merely for social popularity,
- announcements, roundups, and vendor marketing are generally low quality unless there is substantial original analysis,
- judge from the representative beginning/middle/end sample.

### 18.3 Reader-fit rubric

`reader_fit_score` judges whether the reader is likely to value the article, given the taste profile and its learned adjustments. It must **not** be shown the numeric embedding/facet/feed/social scores — those are independent model inputs, and showing them would create self-reinforcing double counting.

A caveat worth recording rather than papering over: reader-fit *is* shown the prose profile, whose learned-adjustments section is enriched with facet data derived from the same ratings that produce `facet_preference` (§22). These components therefore share error. §19's weighting is chosen with that in mind, and §27.2 tracks the correlation instead of asserting independence.

### 18.4 What writes `scores.llm_score` — do not skip this

The churn-suppression rule is implemented as `db.recently_low_scored_ids(STALE_LOW_SCORE, since)` (`src/curate/prefilter.rs:104`), which reads `scores.llm_score` (`src/db.rs:325-339`), which is written only by `db.upsert_score` from `llm.score` (`src/db.rs:402`). If Stage A stops producing a field named `score`, nothing writes that column, `recently_low_scored_ids` returns empty forever, and the rule dies with **no error and no failing test**. Yesterday's rejects then re-enter admission every day, consume Stage A tokens, and recirculate indefinitely.

Therefore:

- `scores.llm_score` continues to be written, with `quality_score`.
- `scores.llm_reader_fit_score` is written with `reader_fit_score`.
- `scores.assessment_version` is written as `2`.
- The churn rule reads the **`candidate_rankings` observation history**, not `scores` (§7.8): `scores` is keyed by nominal date and overwrites, so it cannot answer a question about observation time. `scores.llm_score` still matters as the compatibility projection and as the value any legacy or pre-`0002` reader sees.
- Required regression tests (§31.7), covering **both directions and both time axes**:
  - high→low: an article scored `< floor` yesterday does not appear in today's admitted set;
  - **low→high: an article scored 2.0 on Monday and rescored 7.0 on Friday is admitted again** — the test the naive `WHERE score < floor` query fails;
  - a low score *observed today* while recurating a nominal date three months old **does** suppress the article, proving the window follows `runs.started_at` rather than `run_date`;
  - rescoring for the same nominal date on a later day does not change what a fidelity replay bounded before the rescore concludes;
  - a pre-`0002` `scores` row **never** suppresses an article, in any mode — there is no legacy fallback to compete with an observation (§7.8).

### 18.5 Prompt evidence and concurrency

- Replace the 200-word excerpt with `representative_excerpt` (§15.4).
- Keep title, author, feed, word count, and excerpt-only status.
- **Remove raw social statistics** from the Stage A prompt, and stop telling the model that "came via HN" or "came via Scour" should boost the score. Social proof is already a separate, deliberately weak feature; the current prompt lets popularity influence the judgment a third time.
- Keep source provenance only where it helps interpret extraction quality.
- Run batches with bounded concurrency: `futures::stream::iter(batches).buffer_unordered(deepseek.max_concurrent_requests)` (default 4), with the budget check evaluated **before each spawn** (§24). Prompts grow by roughly 2× per article, so serial batching is the wrong default at 120 candidates.

Record the measured Stage A wall clock in the run report so the concurrency setting can be tuned against the real publish deadline.

---

## 19. Final utility score

After Stage A, compute utility for the 120 candidates using §12.3's present-and-active weighted mean:

```text
0.40 LLM editorial quality        (quality_score / 10)
0.15 LLM reader fit               (reader_fit_score / 10)
0.15 embedding rating preference  (gated)
0.10 facet preference             (gated)
0.10 standing-interest semantic match
0.05 feed affinity                (gated)
0.03 heuristic score
0.02 social proof
--------------------------------
1.00 nominal total
```

On a cold-start day the three gated rows (0.30 combined) drop out and the remainder renormalizes to 0.57 quality / 0.21 fit / 0.14 interest / 0.04 heuristic / 0.03 social — a sane paper on day one, with the learned components fading in as ratings arrive rather than switching on abruptly.

Store `utility_score` on a **0–100 scale** for readability (`blend * 100`). Every consumer that needs a `[0,1]` value divides by 100; state this once here so §20 does not have to guess.

Why quality remains largest: this newspaper should prefer an excellent piece slightly outside known taste over mediocre content matching a favored topic. Why learned behavior still matters: at full evidence, 30% of the score is direct rating-derived preference and reader-fit adds an adaptive signal on top.

Auto-includes remain guaranteed for Stage B consideration regardless of utility.

---

## 20. Diversification: cluster caps (~120 → ~60)

Do not take the top 60 by utility. Two articles about the same news cycle should not both consume shortlist slots merely because each scored well.

**V1 uses leader-clustered caps rather than MMR.** MMR introduces `lambda`, a parameter with no interpretable meaning in isolation, and composes awkwardly with the "preserve the top N by raw utility regardless" rule that any real deployment needs — at which point it is force-include-then-MMR, not MMR. The actual problem ("six articles about the same AI news cycle") is discrete. A cluster cap has one parameter that can be eyeballed against real article pairs, composes trivially with auto-includes and protected sets, and renders usefully: *"suppressed: 3rd article in the cluster led by #4821"*.

**This is leader clustering (Hartigan), not single linkage** — v2 of this plan used the latter name for the former algorithm. The distinction is real: under single linkage, if A~C and B~C but A≁B, all three become one connected component, so a cap of 2 would suppress one of two genuinely dissimilar articles, and chaining can swallow an entire news cycle plus its neighbors. Leader clustering compares each candidate against **cluster leaders only**, which bounds every cluster to a ball of radius `cluster_threshold` around its leader and makes the semantics statable in one sentence: *a cluster is the set of articles within `cluster_threshold` of the highest-utility article that started it.* Processing order is by descending utility, so the leader is always the strongest article in its cluster, which is exactly what should survive the cap.

```toml
[curation.personalization.diversity]
cluster_threshold = 0.85
per_cluster_cap = 2
utility_protected = 15
shortlist_keep = 60
```

Algorithm:

1. Sort candidates by `utility_score` descending, `article_id` ascending as tiebreak. This order is the algorithm's only source of nondeterminism, and it is fully specified.
2. Assign clusters in that order: a candidate joins the cluster whose **leader** has the highest `dot(e_a, e_leader) >= cluster_threshold` (ties broken by lowest `cluster_id`); if no leader qualifies, it becomes the leader of a new cluster. Compare against leaders only, never against non-leader members. Articles without an embedding are singleton clusters — never suppressed, never suppressing.
3. Admit in sorted order while `members_admitted[cluster] < per_cluster_cap`, until `shortlist_keep`.
4. Auto-includes are always admitted and count toward their cluster's tally.
5. The top `utility_protected` by raw utility are always admitted regardless of the cap, and **do** count toward cluster tallies (otherwise near-duplicates of protected items sail through).
6. Exploration candidates that cleared §17's floors get up to `ceil(exploration_reserve / 2)` reserved shortlist slots.
7. If fewer than `shortlist_keep` were admitted, relax in passes: cap 3, then uncapped, filling by utility.
8. Persist `cluster_id`, `cluster_rank`, `rank_by_utility`, and `excluded_reason = cluster_suppressed` for suppressed candidates.

Similarities are clamped to `[-1, 1]` after the dot product, and vectors are verified finite and unit-norm at load (§7.1).

The bridge case (A~C, B~C, A≁B) is a required test (§31.6): under leader clustering it must produce **two** clusters when A leads, not one. If evaluation later shows that genuine news cycles fragment across leaders and slip past the cap, switching to connected components over the threshold graph (union-find, trivial at 120 candidates) is a one-function change — but it should be a measured decision, not a naming accident.

**Duplicate stories are a different problem.** Keep URL/title dedupe and Stage B's "do not select two articles that tell the same story." Cluster caps reduce thematic redundancy among genuinely different articles; they are not duplicate detection.

MMR remains a documented alternative (§35) if cluster caps prove too blunt.

---

## 21. Stage B selection

Stage B remains the final editor and receives a larger, better, more diverse shortlist (default 60, up from 40).

### 21.1 Candidate rendering

Per candidate: title; feed; word count / reading time; LLM quality score; LLM reader-fit score; concise Stage A rationale; top matching standing interests (name + z, not raw cosine); compact facets (`format`, `depth`, `evidence`, `technicality`); whether it is auto-include or exploration; and a short representative blurb rather than the first 45 words.

Do **not** dump every numeric ranking component into the prompt. The editor should have enough evidence to edit an issue, not enough to mechanically reproduce the scorer.

### 21.2 Target and ceiling: exact precedence

Today `--max-articles` is **not** a ceiling: `src/pipeline.rs:188` assigns it to `target`, and `select::size_bounds` derives `(target - 5, target + 5)`, so `--max-articles 10` currently permits 15 picks and *forces* a floor of 5. It must **become** a ceiling.

Carry two separate values through the pipeline and into the Stage B prompt:

```text
soft_target = curation target_article_count                     (default 20)
hard_max    = min(curation.max_article_count (25), --max-articles if provided)
if --max-articles is provided: soft_target = min(soft_target, hard_max)
```

Auto-include precedence:

```toml
auto_includes_exceed_max = false     # default: hard_max is truly hard
```

With the default, if auto-includes alone exceed `hard_max`, they are trimmed by `utility_score` and the trim is logged and reported (`excluded_reason = over_max_trim`). Set it to `true` to let auto-includes exceed `hard_max`, in which case `hard_max` is documented as a *normal-content* ceiling and the exception is surfaced in the run report.

#### Capacity is reserved before Stage B, not reclaimed after

That rule only settles auto-includes against the ceiling. Three other things claim final slots — protected auto-includes reinserted after Stage B (§25.1), the Phase B interleave exposure (§32), and the editor's own picks — and if Stage B returns exactly `hard_max` articles, something must give. Deciding that during implementation would produce a different answer at each of the three insertion sites.

**Reserve capacity up front so the ceiling given to Stage B is truthful:**

```text
mandatory       = auto-includes (protected and ordinary), deduplicated by article id
interleave_slot = 1 if the interleave is enabled AND a qualifying union-only candidate exists, else 0
editor_capacity = max(0, hard_max - |mandatory| - interleave_slot)
editor_target   = min(soft_target, editor_capacity)
```

Stage B is prompted with `editor_capacity` and `editor_target`, not with `hard_max`. It sees a slightly smaller slate on days with many auto-includes, which is honest, and the alternative — letting the editor fill the issue and then evicting its choices — throws away work and produces less coherent issues.

**Merge order after Stage B**, applied exactly once, in this order:

1. **Deduplicate.** If Stage B naturally selected an article that is also mandatory or the intended interleave pick, it counts as that role and is not inserted twice. An interleave candidate chosen on merit by the editor still counts as a real exposure (`interleave_pick = 1`), since exposure is what the cohort measures.
2. **Insert mandatory auto-includes.** They are never evicted. If they alone exceed `hard_max`, §21.2's `auto_includes_exceed_max` rule decides.
3. **Insert the interleave pick**, if one qualified and was not already selected. Its slot was reserved, so this cannot overflow.
4. **Admit editor picks in Stage B's returned order** until `hard_max` is reached; anything beyond that point is dropped with `excluded_reason = over_max_trim`. One ordering rule, not two: a malformed over-cap response is trimmed from the tail of the model's own ordering, because the editor's sequencing is the only signal about which picks it considered load-bearing. `ordering_score` breaks ties only when the response supplies no usable order at all.

**When mandatory content consumes all capacity, the interleave does not run that day.** It is a measurement device, not an editorial requirement, and it must never displace an article the operator explicitly asked to always include. The run report records `interleave_reserved = 1, interleave_selected = 0, reason = "no capacity"`, and `interleave_selected` counts only a genuine final exposure — so the Phase B exit gate (§32) counts real labels, never intentions.

### 21.3 No minimum, ever

Stage B instructions become:

- aim for approximately `soft_target`,
- never exceed `hard_max`,
- choose materially fewer when the shortlist does not justify a full issue,
- never pad with an article the editor would not defend.

In `assemble()`:

- keep the max-size trim,
- **delete the "Too few: top up from the best unpicked candidates" branch entirely**,
- fall back to heuristic selection only when Stage B returns zero usable picks or the call fails,
- if the model returns 8 good articles, publish 8.

### 21.4 Sort keys after `combined_score()` is removed

`assemble()` currently uses `ScoredArticle::combined_score()` in three places (oversize trim, `sort_by_combined`, intra-section ordering). Replacing it without specifying a successor would leave `assemble` unsorted or silently reaching for a stale formula.

Rule: **trim and order by `utility_score`, falling back to `prefilter_score` when utility is absent** (auto-includes that never reached Stage A, `--skip-llm` runs). Implement as one helper, `rank::ordering_score(&Candidate) -> f64`, used by every call site, and delete `combined_score()` in the same commit so there are never two competing formulas.

`select_without_llm` (the `--skip-llm` path) currently calls `sort_by_prefilter` directly. Update it to use `ordering_score` so the deterministic path benefits from semantic-interest and cached-preference signals instead of reverting all the way to the old prefilter order.

### 21.5 Section diversity stays editorial

Keep the section palette, section validation, unique-lead rule, auto-include reinsertion, duplicate-ID defense, and malformed-response fallback. Clustering handles topical redundancy before the LLM; section assignment and issue rhythm remain Stage B's job.

---

## 22. Weekly natural-language profile

Keep `profile::weekly_rebuild_if_due()` — the qualitative summary is valuable to Stage A/B — but change its authority and its evidence.

**Current problem:** the prompt says learned adjustments must "never contradict the stated preferences — refine them", which makes the source-code profile a constitution rather than a prior.

**New instruction, approximately:**

> Treat stated preferences as a strong initial prior, not an immutable rule. Prefer repeated, recent behavioral evidence when it clearly conflicts with an older stated preference. Do not override a stated preference on one or two anomalous ratings; call out genuine preference drift only when it is supported across multiple articles.

Enrich the rebuild prompt with saved facet data, including the descriptive-only dimensions — the LLM is doing pattern recognition, not parameter estimation, so high-cardinality fields help here even while they are unscored in §13.3. Each rated line, compactly:

```text
UP | title | feed | topic_group | format | depth | technicality | evidence | tones
```

Keep the weekly cadence. Immediate quantitative preference now reacts to each vote (§13), so the prose profile provides stability rather than latency.

Anchor the ratings window to `as_of`, not `Timestamp::now()` (`src/curate/profile/mod.rs:312`), and record `profile_version` + `profile_hash` in the run manifest.

---

## 23. Rating flow

The rating HTTP endpoint stays fast and simple. On a changed 👍/👎:

1. upsert the `ratings` projection exactly as today **and append a `rating_events` row in the same transaction** (§7.9), storing the **completed** attribution as versioned `feed_credits_json` — the post-fallback credit map, computed at vote time, not the raw direct-feed set,
2. do **not** call Voyage or DeepSeek synchronously from the request,
3. do **not** rebuild any derived aggregate — there is none to rebuild. Feed priors are derived per run from `rating_events` (§7.7), so the endpoint's only job is to record the vote and its attribution. `serve` therefore needs no generation lock: it *does* append to the event authority, but every run reads that authority with an `as_of` bound, so a vote arriving mid-run is simply later than the bound and invisible to it — no torn read is possible.
4. return the confirmation page immediately.

Rated articles already have embedding and facet rows because they appeared in an issue. If one is missing, the next `generate` or an explicit backfill repairs it. The endpoint must never depend on external AI latency.

Flipping a vote appends a new event and is picked up by the next run's latest-event-per-article read (§7.9). Derived preferences are always recomputed from events, never by incrementing counters.

---

## 24. Budgets and provider accounting

- DeepSeek and Voyage ceilings are independent, each evaluated against the `provider_usage` ledger for the current UTC billing day (§7.6) — not against `runs` by nominal date.
- **Publication-critical calls run first.** Ordering within a run: Voyage embeddings → Stage A → Stage B → editorial/world → any shadow or evaluation work. Ordering alone is not a guarantee, though — it only governs one invocation, so the durable protection is §7.6's per-provider `publication_reserve_daily_usd`, which `shadow` and `maintenance` work can never draw on, plus the `shadow_max_daily_usd` cap on top. In practice V1 shadow work is embeddings-only (§32 Phase A), so DeepSeek contention is near zero — but the reserve must exist anyway, because Phase A's whole premise is that it does not change the paper.
- Budget checks are **reserve-then-spend**: a request reserves its estimated cost before being dispatched and reconciles against actual usage on completion. With `buffer_unordered`, a check performed "between batches" is not a guardrail.

### 24.1 How check-and-reserve is made atomic

"Atomic" needs a named primitive, or it is a wish. The process-wide file lock (§24.2) serializes *commands*, not the async tasks inside one — `buffer_unordered` runs sibling reservations concurrently on separate pooled connections. A plain SQLx transaction is `BEGIN DEFERRED` in SQLite, so two tasks can both read the same pre-reservation total, both conclude they fit, and both insert: either the ceiling is exceeded, or one gets `SQLITE_BUSY` at a point the plan would otherwise treat as ordinary provider degradation. Neither `UNIQUE (request_id, attempt)` nor the spend index constrains a *sum*.

The reservation path is therefore:

1. Acquire a **provider-scoped in-process async mutex** (`tokio::sync::Mutex`, one per provider). Every provider-spending command already holds the OS lock, so per-process serialization is sufficient for this single-host deployment.
2. Open a short transaction with an explicit **`BEGIN IMMEDIATE`** (sqlx's `begin_with`, or the statement issued on the connection), so the write lock is taken up front rather than on first write. The pool's `busy_timeout` is already 30s.
3. Re-sum today's spend for the provider *inside* the transaction, apply the class rules above, and either insert the reservation row or refuse.
4. Commit, release the mutex, **then** dispatch the HTTP attempt.

No network work happens inside the transaction — reservations are tiny, so serializing admission costs nothing while the HTTP attempts themselves stay fully concurrent. A refusal is returned **before** the request is dispatched, never after.
- Failed requests and retries are accounted conservatively: **keep the reservation estimate when actual usage is unavailable**, and reconcile down only from a trustworthy `usage` payload. A transport failure or a 5xx often returns no usage block even though the provider may have billed work, so "count actual tokens" is not always observable. The run report shows estimated and provider-reported usage as separate lines so the gap is visible rather than assumed away. Dry runs count normally, since they make real API calls.
- Once a provider's meter trips, remaining calls for that provider are skipped for the run, in-flight requests are allowed to finish, and the run report records how many candidates went unscored. Cached features stay usable.

### 24.2 One mutating run at a time

The ledger makes spend durable and correctly bucketed, but it does not by itself order two processes: overlapping `generate` invocations — the 05:30 timer and an operator rerun, say — can still interleave reservations, race issue publication, and produce two competing lineups for one date. Ordering is a separate concern from accounting, and for a single-reader, single-host service the cheap answer is to **serialize generation**:

**The exact command matrix**, because "every mutating command" plus a three-item list is how `profile rebuild` got missed in v4 — a command that calls DeepSeek, allocates a profile version, and rewrites the `kv` pointer that a run reads while establishing its manifest:

| Command | Holds the lock | Why |
|---|---|---|
| `generate` (incl. `--dry-run`) | **yes**, whole run | Provider spend, publication, ranking snapshots. `--dry-run` persists articles and ranking rows. |
| `profile rebuild` | **yes**, whole command | DeepSeek spend; allocates `taste_profile_versions.version`; moves the `kv` current pointer. Concurrent with a run it can duplicate spend, collide on a version number, or swap the profile mid-manifest. |
| `features backfill` | **yes**, whole command | Provider spend, possibly for a long time. |
| `features prune` | **yes**, whole command | Deletes rows a concurrent run may be reading. |
| `backfill-social` | **yes**, whole command | Writes `social`, which feeds a run's signals. No LLM spend, but the same read-during-write hazard. |
| `db migrate` | **migration section only** | See below. |
| `serve` | **no** | Long-running; must never block a run, and a run must never block the reader's votes. It appends `rating_events`/`ratings` (§7.9), which are timestamped and read as of a bound — a vote landing mid-run is simply after that run's `as_of`. |
| `evaluate`, `explain` | **no** | Read-only. |

- The lock is held by an open file descriptor for the process lifetime and released by the kernel on exit, however the process exits.
- A second invocation **fails immediately**, naming the holder from `generation_lock_info` — not a silent wait, since the common case is an operator who did not realize the timer was running. `--wait-for-lease [SECS]` opts into blocking.
- **No TTL, no heartbeat, no fencing token, no reclamation.** A stage that runs for two hours is simply a stage that runs for two hours; nothing expires underneath it, and no second process can start.

**Acquisition point.** `src/main.rs` currently calls `Db::open_and_migrate` inside each command arm, so "before doing any work" would exclude schema migration and the §7.4b profile bootstrap — and different commands would acquire at different points. Instead:

1. `main` resolves config and takes the lock for *every* command, including `serve`, then runs `open_and_migrate` plus both bootstraps (§7.4b). Two processes starting together therefore cannot interleave migrations or double-seed history.
2. **A lock-holding command keeps the same file descriptor** and simply carries the guard into the command, updating its `generation_lock_info` diagnostics now that the database is open. Only non-lock-holding commands (`serve`, `evaluate`, `explain`) release after the migration section.

Releasing and re-acquiring around that boundary would open a gap in which another process could win the lock, so a command that had just completed startup successfully would fail before doing any work — safe, but a confusing way to fail.

This makes acquisition uniform and visible in one place, instead of a rule each command implements for itself.

This also gives issue publication a mutual-exclusion guarantee it does not have today. A fenced DB lease or a provider ledger becomes the answer only if generation ever needs to span hosts (§35).

---

## 25. Untrusted content and data handling

Extracted third-party article text goes to DeepSeek (Stage A/B prompts) and Voyage (up to 60,000 characters per article). Article text is untrusted input that can contain instructions.

- Delimit article content unambiguously in every prompt (fenced block with an explicit label), and instruct the model to treat everything inside as data and ignore any instructions found within it.
- Validate every model output against offered IDs and known enum values. An article ID not in the batch is dropped; an unknown facet value is dropped to `None`. Never let model output name a new section, article, or facet value.
- Cap and escape metadata fields (title, author, feed) before interpolation; a title is not allowed to close a delimiter.
- Document plainly in the README that article text is sent to external providers, and confirm both providers' retention/training terms before rollout.

### 25.1 `no_external_ai_feed_ids`: scope, and enforcement that actually holds

`curation.no_external_ai_feed_ids: Vec<FeedId>` — **typed Miniflux feed IDs, not strings** (default empty) — is the opt-out for private or authenticated feeds. A non-numeric entry is a startup error, not a silently ignored one.

**Why not the `always_include_feeds` matcher.** v6 said "feed IDs or host substrings, matched exactly like `always_include_feeds`", which is an unsafe basis for a deny policy. That matcher (`prefilter::is_auto_include`) treats numeric values as feed IDs but searches string values as case-insensitive substrings of `article.url` and `article.canonical_url` — the *article's* URL, never the feed's. `SourceRef` carries `entry_id`, `feed_id`, `feed_title`, `category`, and `kind`; there is no feed URL anywhere in the cluster to match against. So a private feed at `reader.internal/private.xml` whose entries link to public sites would be configured as `reader.internal`, match nothing, and be sent to both providers — with the type-level guarantee faithfully carrying the protected data, because the wrapper was constructed. Substring matching is also wrong for a deny rule in general: `example.com` matches `notexample.com.evil.test`, and nothing defines exact-host versus subdomain behavior.

Feed IDs are the right boundary here: every `SourceRef` carries one, they survive deduplication, and the Miniflux account owns the mapping.

**Classification is over the whole cluster, and it is durable across re-ingestion.** An article is protected when **any** feed ever observed carrying it has a protected `feed_id` — not merely `best_entry_id`'s feed, and not merely the feeds in today's cluster.

The cluster-only rule that v7 specified still leaks, because `db::upsert_article` replaces provenance wholesale (`sources_json = excluded.sources_json`):

1. Day 1 ingests canonical article A through protected feed 42. A is correctly withheld and is not selected.
2. Day 2's overlapping 26-hour window sees the same canonical URL only through a public mirror. `upsert_article` overwrites `sources_json`; feed 42 is gone.
3. The policy gate now sees only public sources, constructs the wrapper, and ships A — with the type-level guarantee once again faithfully enforcing an incomplete classification.

`rating_events.feed_credits_json` does not help: it covers rated articles only, and the wrapper never consults it. Nor can this be fixed by merging historical sources back into `articles.sources_json`, because §7.7 deliberately wants *current* provenance for a candidate's feed affinity. Privacy provenance and ranking provenance are different questions and need different storage:

```sql
CREATE TABLE article_feed_observations (
    article_id  INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    feed_id     INTEGER NOT NULL,
    first_seen  TEXT NOT NULL,
    last_seen   TEXT NOT NULL,
    PRIMARY KEY (article_id, feed_id)
);

CREATE INDEX idx_article_feed_observations_feed ON article_feed_observations(feed_id);
```

Article persistence upserts one row per observed source — bumping `last_seen`, never deleting — so membership accumulates even as the cluster churns. `ProviderPolicy::load` computes the run's protected article set as the intersection of this table with the configured IDs, once per run, and the wrapper constructor consults that set rather than re-deriving it from `Article.sources`.

The resulting rule reads the way the guarantee is worded: **once observed through a protected feed, an article stays protected until the operator removes that feed ID from configuration.** Removing the ID is a deliberate unprotection; failing to see the feed again on some later day is not.

Migration `0002` seeds the table from current `sources_json` in `bootstrap_observation_history()` (§7.4b). Provenance already overwritten before the migration is unrecoverable — which is a reason for the operator to confirm the private-subscription list *before* rollout (§32), not a reason to pretend otherwise.

A domain-based rule (for public content the operator does not want sent anywhere) is a *different* policy with different semantics and is deliberately not in V1. If it is added later it gets its own field name, parses URLs, and compares normalized hosts under a documented exact-host-plus-subdomain rule — never arbitrary substrings, and never conflated with feed identity (§35).

**Scope is strict and total.** No field of a protected article — body, title, feed name, author, derived excerpt, derived facets, or rating-history line — appears in any request to any external provider. Not "the body is withheld": if the operator's threat model is a private feed, a title is often the sensitive part.

Stating that in §25 was not enough in v2, because the enforcement was described only at the embedding and Stage A orchestration sites, while three other paths still send article-derived text:

1. the Stage B prompt, whose renderer appends an `opening:` blurb built from `content_html` (`src/curate/select.rs`, `render_candidate`);
2. `editorial::summarize_article`, which sends title plus up to `SUMMARY_INPUT_TOKEN_BUDGET` of body text for every selected pick (`src/curate/editorial.rs:110-145`);
3. the weekly profile rebuild, whose rating-history lines carry title, feed, and now facets (§22).

**Enforcement is a type, not a convention.** Scattered feed checks are exactly the kind of thing that gets forgotten when a new provider call is added:

```rust
/// The ONLY way to obtain one is `provider_policy::externally_processable`,
/// which consults the run's `ProviderPolicy` — the set of articles ever observed
/// through a feed in `curation.no_external_ai_feed_ids`. Construct it nowhere else.
pub struct ExternallyProcessable<'a>(&'a Article);
```

Every Voyage and DeepSeek orchestration function — `embed_articles`, `score_all`, Stage B candidate rendering, `summarize_article`/`summarize_all`, the profile-rebuild history builder, and any comment/world enrichment that touches article fields — accepts `&[ExternallyProcessable<'_>]` or `ExternallyProcessable<'_>` rather than `&Article`. Leaking a protected article then requires deliberately constructing the wrapper, instead of merely forgetting a check.

Behavior for protected articles, so they are excluded from providers without being excluded from the paper:

- No embedding, no facets, no Stage A score; ranked on heuristic signals alone via §12.3's presence-aware blend, and flagged in `explanation_json.notes`.
- **Omitted from the Stage B prompt entirely.** Protected auto-includes are reinserted deterministically after Stage B — sorted by `ordering_score`, assigned sections by `heuristic_section`, subject to `hard_max` under §21.2's precedence.
- **Summaries always come from the local excerpt path**, never `summarize_article`.
- **Excluded from the profile-rebuild prompt's rating history**, even though their ratings still count in the quantitative preference state (§13), which is entirely local.
- Their ratings still inform **feed affinity** and `W_global` — both derived locally from the article's `rating_events` and the credit map stored on each event (§7.9), never from `ratings` or current `sources_json`, with no external call involved. They cannot inform kNN preference or facet preference, because a protected article has no embedding and no facets to compare against; correspondingly they do not raise `W_embedding` or `W_facet` (§14.1). v3 claimed protected ratings updated the kNN state, which is not possible and would have inflated the embedding gate with observations that contribute nothing.

The test is a recording mock asserting that no Voyage or DeepSeek request body contains **any** field of a protected article — title, canonical URL, author, feed title, or body substring — across a full pipeline run in which a protected article is auto-included and selected (§31.8).

The operator must confirm before the first live run whether any Miniflux feed is private or authenticated. If none are, the setting stays empty and costs nothing.

---

## 26. Backfill and new CLI commands

```text
daily-epub features backfill [--days N] [--rated-only] [--all] [--embeddings-only] [--facets-only] [--yes]
daily-epub features prune [--days N]
daily-epub evaluate --from YYYY-MM-DD --to YYYY-MM-DD [--include-dry-runs]
daily-epub evaluate --from … --to … --counterfactual-features   # tune on history (§6.2, §27.1)
daily-epub evaluate --adjudicate --date YYYY-MM-DD        # blinded Phase A labelling (§32)
daily-epub explain --date YYYY-MM-DD --article ID [--run-id N]
```

### 26.1 Backfill safety

Defaults are conservative: `--rated-only` **on** and `--days 30`. `--all` is required to go beyond rated articles.

Before doing any work, backfill prints an estimate and requires confirmation above a threshold:

```text
features backfill: 1,203 articles, ~1.6M input tokens, ~$0.03 estimated (free-tier allocation applies)
                   38 requests at batch_size 32. Continue? [y/N]
```

`--yes` skips the prompt for cron use. Above `backfill_confirm_token_threshold` (default 5M tokens), `--yes` is *required* — a bare invocation refuses. This matters more later than now: at ~400 articles/day a year of history is ~146k articles and tens of millions of tokens, and one careless command should not eat a quarter of the lifetime free allocation.

Backfill is **resumable and idempotent**: re-running with a warm cache makes zero API calls.

### 26.2 Backfill order

1. Embed all rated articles.
2. Embed articles published in issues (they are the future rated set).
3. Build interest query embeddings (230 items, one request).
4. Embed other recent articles only under `--all`.
5. Facet-backfill rated articles only. Do not spend DeepSeek tokens on the archive automatically.

Backfilled features raise `W_embedding` and `W_facet` for the ratings they cover (§14.1) — that is the point of running it, and the §14.2 log line tells the operator when it is needed. They are, however, invisible to fidelity replays of earlier dates (§6.2); historical tuning that needs them runs under `--counterfactual-features`.

Nothing here requires a Voyage key to compile or to pass tests.

### 26.3 `explain`

Prints the persisted ranking row in human-readable form: raw and normalized signals with presence flags and effective weights; top semantic interests with z-scores; nearest rated neighbors with similarities; strongest positive/negative facet contributions; feed affinity; Stage A quality/fit/rationale; utility and its rank; cluster ID, cluster rank, and what suppressed it; which retrievers admitted it; the terminal stage and `excluded_reason`; whether it was selected. Defaults to the latest **eligible** run for the date under the §7.6 predicate (excluding dry-run and shadow runs unless `--include-dry-runs` or `--shadow` is given); `--run-id` selects a specific one, eligible or not, since debugging a failed run is a legitimate reason to reach for `explain`.

---

## 27. Offline evaluation

### 27.1 Two evaluation modes, never mixed

`evaluate` runs with `as_of` = end of the target day and selects runs through the single typed eligibility predicate of §7.6 — `ok`/`degraded` with a final manifest, never `running` or `failed`, dry runs only on request — with per-metric stage-completeness requirements on top. It never hard-codes a status string.

Which of §6.2's two historical modes applies is a deliberate choice, recorded per result:

- **Fidelity** (`evaluate`, default): features created after `as_of` are invisible. Answers "what could the system have known that day?" Only meaningful for dates after this system shipped and generated features live. On earlier dates it will honestly report that the semantic signals were absent — the true answer, not a defect.
- **Counterfactual** (`evaluate --counterfactual-features`): later-created embeddings and facets are permitted, so a historical candidate set can be re-ranked with today's algorithm and today's features. This is the mode for weight tuning and for the §15.1 facet-stage decision, and it is the mode that makes backfilling rated articles worthwhile.

Every reported metric carries its `feature_time_policy`. Pooling fidelity and counterfactual results would compare "what we knew" against "what we know now" and silently attribute the difference to the algorithm.

For historical days, `articles` stores all deduped articles, not just selected ones, so recent candidate universes can be partially reconstructed from `first_seen` — with the caveat in §27.2.

### 27.2 What is and is not reproducible

State this honestly rather than promising exactness:

- **Scalar ranking is replayable.** `candidate_rankings` stores raw signal values for every considered article, and `run_manifests` stores the weights. `evaluate` **recomputes normalization from raw columns** and never trusts persisted normalized values across a code change — percentile normalization is day-relative, so re-tuning weights requires recomputing percentiles from the full day's candidate set, which is exactly why §7.5 writes a row for every article.
- **Vector-dependent metrics are approximate for historical dates.** `article_embeddings` overwrites in place, and `db::upsert_article` overwrites `content_html` on re-ingest of the same `canonical_url` (which happens routinely, since the 26-hour lookback overlaps consecutive days). Cluster assignments and diversity metrics for a past date are therefore indicative, not exact. This is a deliberate storage tradeoff; do not add immutable vector versioning to fix it.

### 27.3 Metrics

Only shown articles can be rated, so labels are selection-biased. Never treat unrated or unshown articles as negatives.

1. **Recall boundary diagnostics** — for historical upvoted articles, how many would have been lost at each boundary (`admission`, `shortlist`), new pipeline vs. the current top-120 prefilter. *This is the most important metric and the primary Phase A exit gate.*
2. **Pairwise preference accuracy** — when an upvoted and a downvoted article occur in the same issue, how often does utility rank the upvote higher?
3. **Mean utility rank by explicit vote.**
4. **NDCG over explicitly rated articles only** (up=1, down=0), labeled conditional-on-rated.
5. **Shortlist diversity** — cluster count, largest cluster size, mean pairwise similarity (approximate for historical dates per §27.2).
6. **Facet calibration and facet stability** — predicted effect vs. later votes for values with enough evidence; plus the §15.3 stability check (30-article sample re-extracted under a changed profile version, per-dimension agreement, alarm below 85%).
7. **Signal correlation** — pairwise correlation between `reader_fit`, `facet_preference`, and `embedding_preference` on the same candidates. §18.3 notes these share error; measure it rather than assuming independence.
8. **Admission composition** — share of admitted articles by retriever, and the share of semantic admissions below 400 words (a regression alarm for §16.3).
9. **Exploration yield** — up/down rate of selected exploration articles, reported only above 20 observations.
10. **Issue size and rating rate** — confirm that removing the minimum does not collapse issues or engagement.

### 27.4 Weight tuning

Ship with this plan's weights. Move them only on replay evidence, and when they move, append a dated row to `docs/plans/evaluation-log.md` (new file) recording the metric that justified the change. Do not introduce an optimizer or a learned ranker until the simple weighted blend has enough labeled examples to justify it (§35).

---

## 28. Failure and fallback

The service's degradation philosophy is good and must be preserved.

**Voyage unavailable / key missing / disabled**

- Load cached article and interest embeddings; generate none.
- Embedding-derived signals become `Absent` (§12.2) — neutral, never a penalty, never zero-as-a-value.
- The `semantic_interest` and `embedding_preference` retrievers release their quotas to `blend_fill`.
- Clustering degrades to singletons; the shortlist is the top `shortlist_keep` by utility.
- Issue generation continues.

**DeepSeek unavailable / `--skip-llm`**

- No Stage A, so no new facets; cached facets are reused.
- Utility falls back to the deterministic blend over present non-LLM signals — not to the old prefilter order (§21.4).
- Editorial summaries continue to fall back to excerpts.

**Flags** (`--skip-llm` and `--skip-embeddings` ship in the same commit; splitting them across releases is the confusing option):

- `--skip-llm` gates DeepSeek only.
- `--skip-embeddings` gates Voyage only; cached embeddings are still read.
- Neither ever issues an uncached call to the provider it gates.

**Partial facet failure** — parsed rows are stored; missing facet preference is `Absent`; never drop an article because facet parsing failed.

**Budget ceiling** — per §24: independent meters, in-flight requests finish, unscored counts reported, caches remain usable.

---

## 29. Observability

Extend `RunReport` with:

```text
eligible_articles
embedding_cache_hits, embeddings_generated, embedding_failures
voyage_input_tokens, voyage_cost_usd, voyage_truncations
interest_embeddings_generated
admitted_total, admitted_by_retriever{...}
stage_a_candidates, stage_a_scored, stage_a_unscored_budget
facets_cache_hits, facets_generated, facet_parse_failures
shortlist_candidates, clusters, largest_cluster
exploration_reserved, exploration_admitted, exploration_selected
interleave_reserved, interleave_selected
protected_articles (no_external_ai_feed_ids), protected_selected
evidence_weights{global, embedding, facet, feed}, active_signals[...]
stage_completeness{embeddings, admission, stage_a, facets, utility,
                   diversification, selection, publication}   -- §7.4 schema, verbatim
```

Stage timings: `embedding`, `preference`, `signals`, `admission`, `stage_a`, `utility`, `diversity`, `stage_b`.

Info-level summary, one block per run:

```text
curation: 417 eligible -> 120 admitted -> 60 shortlisted -> 17 selected
admission: interest 20, heuristic 20, auto 3, blend 77 (embedding/feed/exploration inactive)
personalization: W=0.0 (0 ratings) — embedding/facet/feed signals inactive
providers: deepseek $0.31 / 2.00 · voyage $0.004 / 0.25 (guard)
```

At debug level, log top ranking explanations. Never log full embedding vectors or API keys.

---

## 30. File-by-file changes

### `src/config.rs`

The complete resulting configuration surface is listed in §38; this is what changes in code.

- Add `VoyageConfig` and `PersonalizationConfig` (nested `quotas`, `weights`, and `diversity` blocks).
- Validate: dimension enum; `1 <= batch_size <= 1000`; concurrency bounds; `0 <= cluster_threshold <= 1`; `per_cluster_cap >= 1`; `stage_a_keep >= shortlist_keep >= soft_target`; `hard_max >= soft_target`; non-negative weights, lookbacks, budgets; `evidence_full > evidence_floor >= 0`.
- Relocate the existing `prefilter_keep >= target_article_count` check to `stage_a_keep >= target_article_count`. Accept `prefilter_keep` as a deprecated alias for `stage_a_keep` for one release, warning at startup.
- Move `STALE_LOW_SCORE` / `STALE_LOOKBACK_DAYS` into `[curation]` as `recent_rejection_score_floor` / `recent_rejection_lookback_days`.
- Add `curation.no_external_ai_feed_ids` (typed `Vec<FeedId>`; non-numeric entries are a startup error), `curation.max_article_count`, `curation.auto_includes_exceed_max`, `personalization.interleave_union_only_slots`, `personalization.exploration_full`.
- Validate `exploration_full > exploration_floor >= 0`, `interleave_union_only_slots < target_article_count`, `0 <= interleave_min_quality <= 10`, and `adjudication_cooldown_days >= 0`.
- No lease TTL setting exists: mutual exclusion is a file lock with no expiry (§7.4c).
- Tests for TOML/env layering including `DAILY_EPUB_VOYAGE__API_KEY`, asserting the secret never appears in `Debug` output.

### `src/types.rs`

- Add `Signal`, `RankingSignals`, `ArticleEmbedding`, `PreferenceState`, `RatedExample`, `Candidate`.
- Add `LlmArticleAssessment` (v2) beside `LlmScore` (v1); do not mutate `LlmScore`.
- Delete `ScoredArticle::combined_score()` in the same commit that introduces `rank::ordering_score`.

### `src/curate/provider_policy.rs` (new)

The single gate for external processing (§25.1): `ExternallyProcessable<'a>` with a private field, one constructor `externally_processable(&Article, &CurationConfig) -> Option<ExternallyProcessable<'_>>`, and a slice helper that partitions a candidate list into permitted and protected halves. No other module may construct the wrapper. Every Voyage/DeepSeek orchestration signature changes to take it.

### `src/lock.rs` (new)

`GenerationLock`: `flock(LOCK_EX | LOCK_NB)` on `<database_path>.lock`, holding the file descriptor for the process lifetime (§7.4c). Writes the advisory `generation_lock_info` row after acquiring, so a blocked invocation can name the holder. No TTL, no heartbeat, no fencing token — the kernel is the authority, and the row is diagnostics only. `--wait-for-lease` polls with backoff to a deadline.

### `src/db.rs`

Runtime `sqlx::query` only, no new compile-time DB requirements. Add:

- get/upsert article embedding; batch-load embeddings by article IDs,
- get/upsert interest embeddings,
- get/upsert article facets (full key),
- load the latest `rating_events` row per article bounded by `as_of` (§7.9), joined to article/facet rows only for locally compatible signals — never to current `sources_json`, whose credits come from the event,
- derive feed priors in memory from ratings bounded by `as_of` (no aggregate table),
- insert the provisional run manifest; transition it to `ranking_fixed`; transition it to `final` transactionally with `finish_run` (§7.4),
- insert/update candidate ranking rows for a `run_id`,
- ledger reads: provider-wide and per-`budget_class` spend for a UTC billing day,
- append `rating_events` (transactionally with the `ratings` upsert) and `publication_events` (in the single transaction that also writes `issues` and `issue_articles`, §7.9),
- as-of-bounded temporal reads: `rating_events_as_of(as_of, lookback)`, `previously_published_ids(as_of, before_date)`, `recently_low_scored_ids(floor, since, as_of)` over `candidate_rankings`,
- provider ledger: reserve, settle, mark-failed, and `provider_spend_for_billing_day(provider, utc_date)`,
- `taste_profile_versions`: append-on-rebuild (in the same transaction as the `kv` update) and `profile_effective_at(as_of)`,
- read/write the advisory `generation_lock_info` row,
- `bootstrap_profile_history()` and `bootstrap_observation_history()` (§7.4b) — two independent, marker-guarded, idempotent bootstraps run after `sqlx::migrate!`,
- adjudication insert and per-arm rollup,
- **one typed run-eligibility predicate** (§7.6) shared by every evaluation query — no status strings duplicated at call sites,
- listing helpers for `evaluate` and `explain`,
- prune helpers for embeddings, ranking rows, and provider-ledger rows.

### `src/curate/embedding.rs` (new)

Voyage request/response types; backend trait + mock; retry classification; batching with char budgets and bounded concurrency; embedding document; SHA-256 hashing; f32 BLOB encode/decode with finite/norm validation; dot product with dimension checking; article + interest cache orchestration; usage meter.

### `src/curate/facets.rs` (new)

Facet schema v1 (scored + descriptive); tolerant parser with enum validation and cardinality truncation; `representative_excerpt`; cache orchestration; the prompt fragment injected into Stage A; a standalone extraction entry point kept for the §15.3 escape hatch.

### `src/curate/preference.rs` (new)

As-of-bounded load of the latest `rating_events` row per article; time decay; signed top-k neighbor signal; facet statistics with support gates; run-local feed priors from each event's stored `feed_credits_json` (§7.7, §7.9) plus candidate affinity; the four per-signal evidence weights and `EvidenceGates` (§14.1); explanation generation.

### `src/curate/recall.rs` (new)

Reuses `prefilter`'s hygiene context rather than duplicating SQL; retriever quota admission; semantic quality floors; deterministic exploration; `admitted_by` / `excluded_reason` bookkeeping.

### `src/curate/rank.rs` (new)

Mid-rank percentile normalization; presence-aware blending with weight renormalization; preliminary and utility scores; `ordering_score`; leader-clustered caps; stable sorting.

### `src/curate/prefilter.rs`

Keep hard hygiene and the cheap heuristic score. Remove its role as the only top-N cutoff. Take churn constants from config. Preserve current heuristic point values for now so evaluation has a stable baseline; the heuristic's influence is weak in the new utility.

### `src/curate/score.rs`

Quality + reader-fit + facets response; representative excerpt; remove social statistics and provenance boosts from the rubric; bounded concurrency; tolerant parsing; write `scores.llm_score` = `quality_score` plus the two new columns (§18.4).

### `src/curate/select.rs`

Shortlist input default 60; render facets, interests, quality and fit; separate `soft_target` from `hard_max`; delete the top-up branch; `ordering_score` at all three former `combined_score()` sites; update `select_without_llm`; keep max trim, section validation, unique lead, auto-include reinsertion, duplicate-ID defense, and malformed-response fallback.

### `src/curate/profile/mod.rs`

Keep OPML parsing and theme grouping. Change the learned-adjustment instruction to prior-plus-drift. Include facet context in rating history — **excluding protected articles** (§25.1). Anchor to `as_of` and select the profile through `taste_profile_versions`. `store()` appends a history row in the same transaction as the `kv` update. Keep weekly cadence. Move feed-prior logic to `preference.rs` with a thin wrapper if convenient.

### `src/curate/editorial.rs`

`summarize_article` / `summarize_all` accept `ExternallyProcessable` only; protected picks take the local excerpt path without an API call.

### `src/server.rs`

The rating endpoint appends a `rating_events` row transactionally with the `ratings` upsert, storing the completed `feed_credits_json` credit map for that vote (§7.9). Nothing else changes: no aggregate rebuild, no provider call, no lock.

### `src/publish.rs`

`pipeline::record_issue` becomes **one** transaction containing `upsert_issue`, `replace_issue_articles`, and one `publication_events` row per pick (§7.9) — today those are two separate transactions, which can already leave the projections half-updated. `issues`/`issue_articles` keep their overwrite semantics as projections. Startup gains the reconciliation check for published files with no `issues` row.

### `src/pipeline.rs`

Wire the §5 order. Receive the generation lock guard from `main` (§24.2) rather than acquiring it here. Insert the **provisional** manifest after the run row; transition it to **`ranking_fixed`** once preference state and profile selection are known; transition it to **`final`** with terminal stage completeness in the same transaction as `finish_run` (§7.4). Put `upsert_issue`, `replace_issue_articles`, and `publication_events` in **one** transaction after file publication (§7.9). Thread `as_of` and `mode` everywhere. Every new external stage is non-fatal. Separate `soft_target` from `hard_max` at the point where `--max-articles` is read (`src/pipeline.rs:188` today). Reinsert protected auto-includes after Stage B.

### `src/main.rs`

Add `features backfill`, `features prune`, `evaluate`, `explain`, `--as-of-date`, `--skip-embeddings`, `--wait-for-lease`, `--counterfactual-features`, and `evaluate --adjudicate` / `--include-dry-runs`. Update `--skip-llm` help text.

Restructure startup so locking is uniform (§24.2): resolve config → take the lock → `open_and_migrate` → `bootstrap_profile_history()` and `bootstrap_observation_history()` → then **keep the same file descriptor** and hand the guard to a lock-holding command, or release it for `serve`/`evaluate`/`explain`. Never release-and-reacquire: the gap lets another process win the lock and fail a command that had already completed startup. Today each arm calls `Db::open_and_migrate` itself, which is why "before doing any work" had no single meaning.

`profile rebuild` is a lock-holding, provider-spending command and must reserve through the ledger like any other.

### `src/report.rs`

New counts, per-stage timings, per-provider usage, funnel summary, active-signal list, and the per-stage completeness block that feeds `run_manifests.stage_completeness_json`. Do not add a `complete` variant to `RunStatus`; the existing five values stay as they are (§7.6).

### `README.md` / `config.example.toml`

Voyage key and config; the new curation architecture at a high level; backfill/evaluate/explain; the evidence ladder in one paragraph (why day-one issues are ranked on interests, not learned taste); soft target vs hard ceiling and no forced filler; that only one `generate` may run at a time; and that article text is sent to external providers, with `no_external_ai_feed_ids` as the opt-out, why it is feed ids rather than hostnames, and exactly what it withholds.

---

## 31. Tests

No test may call Voyage or DeepSeek over the network.

### 31.1 Embedding

- f32 BLOB round trip preserves values and dimension; malformed length rejected safely; non-finite values rejected.
- Non-unit vectors are normalized at load, with tolerance.
- Embedding document deterministic, char-capped on a UTF-8 boundary, and **contains no feed title or author**.
- Cache hit on matching hash/model/dimension; miss on changed content, dimension, or model.
- Mock Voyage response maps embeddings by index; a length mismatch is an error.
- A failed batch does not abort other batches; bounded concurrency respects the limit.
- 429/5xx retryable, ordinary 4xx not.
- Dot product with mismatched dimensions returns an error, never panics.

### 31.2 Interests

- Interest embeddings cached and deduped; both `text_version`s coexist.
- Z-scoring: a broad interest with uniformly high similarity does **not** dominate top-1; a specific interest with one strong match does.
- Falls back to raw similarity below 20 candidates.
- Top-1/top-3 aggregation deterministic.

### 31.3 Normalization *(new — these guard the critical defects)*

- **A signal constant across all candidates normalizes to 0.5 for every candidate.**
- **Ties receive equal normalized values** (explicitly: 400 candidates with identical raw 0.0 all get 0.5; no ID ramp).
- Missing values are excluded from the CDF and do not shift other candidates' percentiles.
- A candidate missing signal X is scored on the renormalized weights of its remaining signals; a mixed cached/missing embedding population does not systematically demote the uncached half.
- Effective weights persisted in `explanation_json` sum to 1 within tolerance.

### 31.4 Preference and the evidence ladder

- No ratings ⇒ embedding/facet/feed signals `Absent`, gates 0, weights redistributed, blend equals the interest/heuristic/social mean.
- One upvote produces a positive neighbor similarity; a multi-modal rating set (two unrelated clusters) yields high similarity to **both** clusters — the regression test for the centroid problem.
- Time decay halves at the configured half-life.
- Flipping a rating changes derived state correctly.
- Facet Beta smoothing stays near neutral with one observation and strengthens with repeated evidence; a value below `facet_min_observations` is skipped.
- Multi-value facet dimensions contribute once per dimension, not once per label.
- Feed credit sums to exactly 1.0 across distinct direct-feed sources; discovery feeds get none when a direct feed exists.
- Candidate feed affinity uses the mean, never the optimistic max.
- Feed priors are derived per run and bounded by `as_of`: ratings created after `as_of` do not appear in them, and running a fidelity replay leaves no persisted aggregate behind to affect the next live run.
- Gate ramp is linear between `evidence_floor` and `evidence_full` and clamps at both ends.
- **Per-signal evidence (§14.1):** 20 decayed ratings of which exactly one has a compatible embedding leave `W_embedding ≈ 1` and the embedding gate near its floor, while `W_global = 20`; the facet and feed gates are computed independently from their own coverage.
- A model or dimension change drops `W_embedding` to zero even though `W_global` is unchanged, closing the embedding gate until the cache is rebuilt.
- A rating on a protected article (§25.1) raises `W_global` and `W_feed` but neither `W_embedding` nor `W_facet`, and never enters the kNN example set.

### 31.5 Admission

Critical regression test:

> An article with mediocre heuristic and social scores but very strong semantic interest similarity is admitted and reaches Stage A.

Inverse regression test *(new)*:

> A 60-word release-note stub with very high interest similarity is **not** admitted via a semantic retriever.

Also: high-heuristic articles survive via the heuristic retriever; auto-includes always survive; blocked/published/churn articles never leak through and each gets a thin row with the right `excluded_reason`; each active retriever's quota is honored under cap pressure; inactive retrievers release quota to `blend_fill`; exploration is deterministic per date and rotates across dates; exploration reserve is 0 below `exploration_floor`.

### 31.6 Ranking and diversification

- Utility calculation exact against a hand-computed fixture.
- Near-duplicate embeddings land in one cluster and the third is suppressed.
- A lower-utility diverse article outranks a redundant higher-utility one under the configured cap.
- Protected top-N survive and still count toward cluster tallies.
- Auto-includes survive the shortlist limit.
- Articles without embeddings are singleton clusters and are never suppressed.
- Relaxation passes fill the shortlist when caps leave it short.
- **Bridge case:** given `sim(A,C) >= threshold`, `sim(B,C) >= threshold`, `sim(A,B) < threshold`, and utility order A > B > C, leader clustering yields **two** clusters (A leads, B leads, C joins whichever leader it is closer to) — not the single connected component single linkage would produce.
- Candidates are compared against leaders only: adding a fourth article similar to a non-leader member but not to any leader starts a new cluster.

### 31.7 Stage A / Stage B

- Stage A parses quality + fit + facets; a malformed facet object preserves the scores; an unknown enum value degrades to `None`.
- **Every enum token appearing in a prompt example or test fixture parses to `Some(_)`** — the guard against §18.1's `postmortem_case_study` class of bug, where a tolerant parser turns a documentation error into silent data loss.
- Stage A prompt contains the three-part sample and **no** social-score calibration instruction.
- **`scores.llm_score` is written from `quality_score`, and an article with `quality_score < 3` yesterday is not admitted today** (the churn regression).
- `assessment_version` distinguishes v1 and v2 rows; a v1 row remains readable.
- Stage B prompt includes compact facets and top interest matches.
- Stage B accepts a deliberately small lineup and publishes it unchanged.
- **Delete every test that requires top-up to `target - 5`.**
- Zero usable Stage B picks still triggers the heuristic fallback.
- `--max-articles` below, equal to, and above `soft_target`, including the case where auto-includes alone exceed it, under both `auto_includes_exceed_max` settings.
- **Capacity precedence at the ceiling (§21.2):** Stage B is prompted with `editor_capacity`, not `hard_max`; when Stage B returns exactly its capacity, mandatory auto-includes and the reserved interleave slot all fit without eviction and without exceeding `hard_max`.
- When mandatory content consumes all capacity, the interleave does not run, `interleave_selected = 0` with a recorded reason, and no auto-include is displaced.
- An interleave candidate the editor selected on merit counts once, as a real exposure, and is not inserted twice.

### 31.8 Pipeline integration (mocked Voyage + DeepSeek)

- The full funnel writes a `candidate_rankings` row for **every** eligible article plus every hygiene-excluded one, with correct `terminal_stage` and `excluded_reason`.
- Voyage failure still publishes; facet failure still publishes; DeepSeek failure publishes via the deterministic path.
- `--skip-llm` makes zero DeepSeek calls; `--skip-embeddings` makes zero Voyage calls; neither makes uncached calls to the provider it gates.
- Rerunning a date is idempotent, reuses caches, and creates a **new** `run_id`.
- **A rerun that previously reached Stage B but now trips the budget at admission leaves no stale Stage A/B flags** — the anti-`COALESCE` regression.
- A run that fails **before** preference/profile capture keeps `status = 'failed'` and a `provisional` manifest; a run that fails **after** it keeps `ranking_fixed` (or reaches `final` with terminal completeness, per §7.4). Either way `evaluate` excludes it and `explain --run-id` can still read whatever exists.
- **A run with zero eligible candidates finalizes its manifest anyway** and is evaluable, reporting "0 eligible" rather than sitting provisional forever (§7.4).
- Failure **before** preference capture leaves `provisional`; failure **after** it leaves `ranking_fixed` or `final` with terminal completeness (§7.4). Both are excluded from `evaluate`; both are readable by `explain --run-id`.
- `issues`, `issue_articles`, and `publication_events` commit in one transaction: an injected failure leaves none of the three written, never a partial issue record.
- Startup reconciliation reports a published file whose `issues` row is missing, and reruns of that date remain idempotent.
- A final manifest always carries a parseable, current-version `stage_completeness_json`; a row with a malformed or unknown-version blob makes the run ineligible for **every** metric, with a diagnostic naming the run.

### 31.9 Run eligibility *(new — C1 guard)*

- Each of the five real statuses (`running`, `ok`, `degraded`, `failed`, `dry_run`) is classified correctly by the typed predicate for each `EvalKind`.
- **A normal successful run is evaluable.** This is the test that would have caught a `status = 'complete'` filter, which excludes everything.
- A `degraded` run whose Stage A tripped the budget contributes admission and ratings metrics but is excluded from Stage A accuracy metrics, driven by `stage_completeness_json`.
- A run with a provisional manifest is never evaluable, whatever its status.
- Dry runs appear only under `--include-dry-runs`.

### 31.10 Provider policy *(new — C2 guard)*

- A recording mock over both providers asserts that **no request body contains any field of a protected article** — title, canonical URL, author, feed title, or a body substring — across a full pipeline run in which a protected article is auto-included and selected.
- A protected auto-include is absent from the Stage B prompt and still appears in the published issue, with a locally derived summary and no `summarize_article` call.
- Protected articles are excluded from the profile-rebuild prompt; when rated they still move feed affinity and `W_global`, but not `W_embedding`, `W_facet`, or the kNN example set.
- Type-level: constructing `ExternallyProcessable` outside `provider_policy` does not compile (compile-fail test or a documented visibility check).
- **Classification, not just enforcement** — the type guarantee is worthless if the gate says "yes" to the wrong article:
  - a protected feed whose entries link to public hosts is still protected (the feed's URL host never appears in the article URL);
  - a cluster whose *best* source is public but whose secondary `SourceRef.feed_id` is protected is protected;
  - a non-numeric `no_external_ai_feed_ids` entry is a **startup error**, not a silently ignored one.
- The full-run recording mock uses the secondary-source case, not a trivial best-feed match.
- **Durability across re-ingestion (§25.1):** ingest article A through protected feed 42, then re-ingest the same canonical URL through public feeds only so `sources_json` no longer mentions 42 — every provider path still rejects A on the second run.
- Removing feed 42 from `no_external_ai_feed_ids` *does* unprotect A on the next run: the protection follows configuration, not history alone.

### 31.11 Lease and concurrency *(new — H4 guard)*

- Two processes racing for the lock: exactly one acquires, the other fails immediately naming the holder.
- A killed holder's lock is immediately available to the next process, with no timeout and no manual cleanup.
- **A stage lasting longer than any plausible TTL does not lose the lock** — the regression test for v3's expiring lease.
- The lock survives an error path: a run returning `Err` or panicking does not strand it, and does not leave a second process blocked.
- A stale `generation_lock_info` row (holder already dead) does not prevent acquisition; it only affects the message.
- `--wait-for-lease` blocks and then succeeds once the holder exits.
- **`profile rebuild` and `generate` cannot overlap**: the second fails immediately, and no two profile versions are ever allocated for the same number.
- The migration critical section serializes two processes starting simultaneously: migrations run once, and **both** bootstraps (profile history and observation history) run exactly once, each guarded by its own marker.
- A lock-holding command never releases between the migration section and the command body: an interposed process cannot acquire the lock in that window.

### 31.11b Provider ledger *(new — H1 guard)*

- A reservation is committed **before** dispatch: killing the process between dispatch and completion leaves the estimate persisted, and the next invocation's ceiling reflects it.
- Settlement replaces the estimate with actual usage; a failure with no usage payload leaves `failed_estimated` and the estimate standing.
- The bucket is the **UTC billing day of `reserved_at`**: recurating three historical dates in one afternoon draws from one ceiling, not three.
- `features backfill` and `profile rebuild` spend lands in the ledger with `run_id IS NULL` and counts toward the same ceiling as `generate`.
- Once the ceiling is reached, the next reservation is refused before any request is dispatched.
- **Retry accounting:** attempt 1 returns 5xx with no usage payload, attempt 2 succeeds — the day's total is attempt 1's standing estimate **plus** attempt 2's actual usage, and both rows share a `request_id`.
- **Budget classes:** a run mixing production and shadow calls stops shadow work at `shadow_max_daily_usd` while production continues; both classes count toward the provider-wide ceiling, and hitting that ceiling stops production too.
- **Publication reserve, order-sensitive** — each of these runs *first*, exhausts what it is allowed, and a subsequent issue-producing run still dispatches with the full reserve available: (a) a shadow run whose spend is overwhelmingly embeddings, (b) a `--dry-run` generate, (c) a standalone `profile rebuild`. A same-run mixed-class test does not cover any of them; the failure only appears across invocations, and (a) is the one v7's cacheability rule would have let through.
- A weekly profile rebuild triggered *inside* an issue-producing run is classed `publication`; the same rebuild invoked standalone is `maintenance`.
- **Concurrent admission:** many reservation tasks released simultaneously against a barrier near the ceiling — the sum of admitted estimates never exceeds the provider cap or the applicable class cap, no task observes `SQLITE_BUSY` as a provider error, and every refusal happens **before** its mock HTTP dispatch.
- The reservation is an **upper bound**, not an average: with an adversarial payload (dense punctuation, source code, CJK text) whose real tokenization exceeds `len/4`, and a mock response reporting input usage above `approx_tokens`, the already-admitted reservation still covers actual usage, and settlement never turns an under-ceiling admitted total into an over-ceiling one.
- The bound includes maximum possible output tokens at output prices with no cache discount assumed, plus `per_request_overhead_tokens`.
- Reported usage above the reservation trips the meter rather than being silently absorbed.
- `billing_day` is derived from `reserved_at` inside the writer: a caller cannot supply a different one.

### 31.12 As-of, leakage, and profile history

- With future ratings and future issues present in the DB, a `fidelity` replay of an earlier date produces identical output to one run without them.
- An article published in an issue **after** the replay date is not excluded from that replay.
- `recurate` mode does the opposite (uses today's knowledge) and says so in the manifest.
**Mutation tests — the destructive cases, not just additive ones.** Each performs a *future* mutation of state a past replay depends on, then asserts the earlier result is byte-identical:

- **Flip a vote in the future:** an upvote on day 1 flipped to a downvote on day 10; a fidelity replay as of day 5 still sees the upvote and produces the same preference state and the same output.
- **Republish a nominal date in the future:** regenerate issue D on day 10 with a different lineup; a fidelity replay as of day 5 still excludes exactly the articles published in the original D and no others.
- **Rescore the same article and nominal date in the future:** the v4-breaking case — score article A as 2.0 on day 1, then rescore it as 7.0 on day 10 via `generate --date`; a fidelity replay as of day 5 still sees 2.0 and still suppresses A under the churn rule. Asserting only that a future score row is *excluded* is not sufficient; the earlier observation must still be *present*.
- **Re-ingest an article with more sources in the future:** feed credit for an older rating splits exactly as it did at vote time, from the event's stored `feed_credits_json`.
- **Discovery-only rating:** an article with no direct-feed source keeps its vote-time fallback attribution (`via_fallback = true`) even after its current provenance changes — the case a pre-fallback "direct feed set" could not have reproduced.
- **One path, both modes:** an article removed from an issue by a same-date republication is still excluded as previously-published by a subsequent **live** run; an article rated through two different issue dates contributes its latest vote exactly once in a **live** run. Both fail if live reads the projections instead of the events.
- Two events sharing an `event_at` resolve deterministically by `id DESC`.
- Legacy pre-`0002` `scores` rows are **never consulted by churn in any mode** — live, recurate, or fidelity (§7.8). They remain readable as a compatibility projection; that is all they are.
- **Feature-time policy:** an embedding backfilled today is invisible to a fidelity replay of an earlier date and visible under `--counterfactual-features`; both runs record which policy applied, and results carrying different policies cannot be pooled by the reporting code.
- **Profile history:** after two weekly rebuilds, a fidelity replay of a date between them selects the *older* profile text verbatim; with no qualifying row, the run records `profile_version = NULL` and omits the prose profile entirely.

### 31.13 Migration

Open a temp DB, run all migrations, exercise the new tables and indexes, and confirm existing rating/issue/score data survives migration and remains readable — including that a v1 `scores` row is still parseable as a compatibility projection. Whether churn *consults* it is a separate question with a separate answer: it does not (§31.7).

Profile bootstrap (§7.4b) is tested across all four input states, since it runs on every startup against whatever the live database happens to hold:

- **absent** — no `kv[taste_profile]`: no row is written, and the first `profile::store` creates version 1;
- **malformed** — `kv[profile_version]` is not valid JSON: the row is still seeded, with `version = 1` and a warning, matching `stored_version`'s existing tolerance;
- **valid** — version, `built_at`, hash, and learned text are all carried across verbatim;
- **already seeded** — a second bootstrap is a no-op and does not duplicate or rewrite the row.

---

## 32. Rollout

```toml
[curation.personalization]
enabled = false
```

**Flag semantics, stated because they were ambiguous:** `enabled = false` disables the new *ranking path* (admission, utility, diversification, Stage A facets, no-minimum Stage B). It does **not** disable embedding generation or feature persistence — those are controlled by `voyage.enabled` and `--skip-embeddings`. Phase A depends on collecting features while the old selector remains authoritative, so tying feature collection to this flag would make the shadow phase impossible.

### Phase A — feature collection and recall shadowing

Ship: migrations, Voyage client, embeddings, interest embeddings, preference state, manifests, candidate ranking snapshots, and the new admission computed in shadow. Production selection stays on the current path.

**Phase A shadows admission only, not selection.** Utility is 40% quality + 15% reader fit, and those fields do not exist until Phase C, so a "compare old vs new selections" gate would be comparing a shortlist ranked on less than half its intended signal. The honest and more useful comparison is the recall-boundary diagnostic — which is also the single best evidence for whether the redesign is justified at all.

**What Phase A cannot measure, and what replaces it.** v2's gate required the union to "admit at least one upvoted article per week that the prefilter would have dropped." That is unobservable by construction: an article the authoritative prefilter drops is never printed, so it can never be upvoted. Every historically upvoted article necessarily survived the old funnel on the day it was shown. This is selection bias, not a sample-size problem — no amount of additional shadow data fixes it. Retaining known positives *is* observable, because those labels already exist; rescuing new positives is not, until the rescued candidates are actually exposed.

So Phase A's gate splits in two: a measurable retention half, and a **blinded operator adjudication** of the union-only candidates, which is the only honest label source available before exposure.

Adjudication protocol: `evaluate --adjudicate --date D` prints a randomized, unlabeled sample of 10 union-only candidates (admitted by the new union, *not* by the old prefilter) mixed with 5 controls drawn from articles the old prefilter admitted but did not select. The operator marks each "would have wanted to read" or not, without seeing which is which or any score. Verdicts are stored in `adjudication_batches` + `adjudications` (§7.4d), keyed by `run_id` with a persisted `sample_seed`, so the sample is reproducible, the blind is verifiable after the fact, and an article is not re-presented within `adjudication_cooldown_days`.

Exit criteria (all must hold):

- ≥ 14 eligible runs (§7.6) with final manifests and feature snapshots,
- ≥ 40 explicit ratings accumulated,
- **retention:** the union admits **≥ 95%** of historically upvoted articles that the current top-120 prefilter would have admitted,
- **adjudicated yield:** across ≥ 40 adjudicated union-only candidates, the "would have wanted to read" rate is **at least equal to** the control rate — i.e. the union's exclusive picks are no worse than the incumbent's unselected pool,
- **< 10%** of semantic-retriever admissions are under 400 words (§27.3 metric 8),
- Voyage cost per run below $0.02 and no budget trips.

Measured *user* upvote yield for rescued candidates moves to Phase B, where they can finally be exposed.

### Phase B — new admission goes live, with bounded interleaving

Enable union admission and the new Stage A candidate set; keep the existing final selector.

Add a small, explicitly bounded interleaving bucket so rescued candidates earn real labels rather than adjudicated ones:

```toml
interleave_union_only_slots = 1     # issue slots, not shortlist slots; 0 disables
```

One slot per issue (of ~20) is reserved for a candidate admitted **only** by the new union. The rule must be exactly stated, because v3's was not implementable: it ranked the cohort by "highest utility" and simultaneously let Stage B refuse — but the v3 utility score is 55% Stage A quality and reader-fit fields that do not exist until Phase C, and a refusable slot is a nomination, not an exposure, so a seven-run window could yield zero labels and defeat the entire purpose.

Phase B behavior, precisely:

1. Rank union-only candidates by the **preliminary blend** (§16.4), which is fully available in Phase B.
2. Require the §16.3 quality floor **and** `interleave_min_quality = 6.0` on the legacy Stage A score, so the slot cannot be filled with something indefensible.
3. **Deterministically reinsert** the top qualifying candidate after Stage B — a guaranteed exposure, using the same post-Stage-B reinsertion path as protected auto-includes (§25.1), with `heuristic_section` for placement and subject to `hard_max`.
4. If no candidate qualifies, the issue simply has no interleaved pick that day, and the run report says so.

Its `candidate_rankings` row records `interleave_pick = 1` — the exposure-origin record that makes the cohort evaluable at all.

Exit: **≥ 7 runs and ≥ 5 actual interleaved exposures**, no drop in overall issue rating rate, no operator-visible junk influx, and the interleaved cohort's up/down ratio not materially below baseline (reported with its confidence interval — five observations is five observations; this is a guardrail against obvious harm, not a claim of significance).

### Phase C — Stage A split, facets, utility, diversification

Enable separated quality/fit scoring with facets, the utility blend, and the 60-item cluster-capped shortlist. Monitor shortlist diversity (cluster count ≥ 25 of 60) and rating rate. Exit: 7 runs, pairwise preference accuracy not worse than Phase B, Stage A wall clock within the publish window.

### Phase D — remove the forced minimum

Enable no-top-up Stage B. Observe issue sizes and ratings for ≥ 7 runs. This is the phase that changes what the reader sees most visibly; expect and accept some short issues.

### Phase E — retire compatibility code

Remove `combined_score()`'s last references, the `prefilter_keep` alias, and any shadow scaffolding once the new system has been stable for two weeks. Candidate cleanups: retiring `PENALTY_TITLE_PATTERNS` in favor of `format = announcement_roundup` (better recall than 22 title substrings), and collapsing `scores` into `candidate_rankings`.

**One question for the operator before Phase A:** exploration (§17) and the semantic retrievers will be most visible exactly when the system has the least evidence. The defaults here start exploration at zero and floor the semantic paths at 250 words specifically to keep Phase A–B quiet. If a noisier paper is acceptable in exchange for faster learning, raise `exploration_max` and lower `exploration_floor`.

---

## 33. Implementation sequence

Small, reviewable commits, in this order:

0. **`Serialize generation and gate external providers`** — `lock.rs`, `provider_policy.rs`, `no_external_ai_feed_ids`, and the wrapper threaded through every existing DeepSeek call site. Small, independent of everything else, and it removes two whole classes of bug before the surface area grows; landing it after the provider calls multiply is strictly harder.
1. **`Add the observation layer and provider ledger`** — migration `0002` tables `rating_events`, `publication_events`, `provider_usage`, plus dual-writes from `serve`/publish and the ledger's reserve-then-settle path. Independent of ranking, and everything after it depends on the temporal reads being correct.
2. **`Add personalization schema, config, and run manifests`** — the rest of migration `0002` (including `taste_profile_versions` plus its Rust bootstrap, `generation_lock_info`, adjudication tables), `VoyageConfig`, `PersonalizationConfig`, as-of-bounded db helpers, the typed run-eligibility predicate, base types, migration tests.
3. **`Add Voyage embedding cache and client`** — backend seam + mock, embedding document, f32 serialization with validation, batching/concurrency, article + interest orchestration, usage meter and `runs` columns, tests.
4. **`Add signal normalization and presence-aware blending`** — `rank.rs` normalization, `Signal`, blend renormalization, `explanation_json` schema, tests (§31.3). *Land this before anything consumes it.*
5. **`Add standing-interest semantic matching`** — z-scored interest scores, both text versions, tests.
6. **`Build rating-derived preference state and the evidence ladder`** — decayed kNN preference, run-local feed priors from rating events, the four per-signal evidence weights, gates, tests.
7. **`Add union admission with retriever quotas`** — `recall.rs`, semantic floors, exploration, candidate ranking snapshots including hygiene exclusions, tests.
8. **`Separate LLM quality and reader fit, and extract facets in Stage A`** — assessment v2, facet schema v1, representative excerpt, `scores` columns, churn-rule continuity test, concurrency.
9. **`Add utility ranking and cluster-capped shortlist`** — utility blend, clustering, `ordering_score`, deletion of `combined_score()`, tests.
10. **`Make final selection quality-gated rather than padded`** — soft target vs hard max, remove top-up, `--max-articles` as ceiling, tests.
11. **`Add backfill, evaluate, and explain`** — CLI with cost guard, replay with as-of, metrics, explain output.
12. **`Enable personalized curation and update docs`** — config example, README, rollout flag flip after Phase A evidence.

Do not combine these into one commit.

---

## 34. Acceptance criteria

1. Every eligible new article can receive a cached `voyage-4-lite` embedding before the admission cut.
2. An article with a low social/word-count heuristic score reaches Stage A solely because it strongly matches standing interests or positive rating history — with a test for each path.
3. Article facets are stored under a versioned typed schema whose cache key includes model, prompt version, and input hash, and cover at minimum format, depth, evidence, commerciality, topic group, and technicality.
4. Explicit ratings affect the next day's ranking through neighbor similarity, facet preferences, and corrected feed affinity, without waiting for the weekly profile rebuild — **and contribute zero weight, with weights renormalized, until the evidence ladder opens.**
5. A signal that is constant or missing across the candidate pool normalizes to 0.5 for every candidate and never introduces article-ID ordering into any score.
6. Weekly profile adjustments include facet context and can recognize sustained preference drift.
7. Utility exposes separate quality, reader-fit, semantic-preference, facet, feed, social, and heuristic components, along with the effective weight actually applied to each.
8. The shortlist is diversified by embedding clusters and is larger than today's ~40 by default.
9. Stage B can publish fewer than 15 articles with no deterministic filler added, and `--max-articles N` is a hard ceiling under a stated auto-include precedence rule.
10. `scores.llm_score` is still written after the Stage A split, and a regression test proves the churn-suppression rule still fires.
11. Missing Voyage or DeepSeek service/key does not prevent issue generation, and missing signals never act as penalties.
12. `candidate_rankings` records, for every article the run considered — hygiene-excluded ones included — which retrievers admitted it, which stage it died at, and why.
13. A fidelity replay bounded by `as_of` is unaffected by anything that happens afterwards — including a **flipped vote, a republished issue date, and a rescored article** — proven by mutation tests, not merely by additive leakage tests.
14. Ranking and publication history is read from the event tables in **every** mode, live included, with `as_of = now`; the projections never decide history.
15. `features backfill` is resumable and idempotent: re-running with a warm cache makes **zero** API calls, and a large backfill refuses to run without explicit confirmation.
16. `evaluate` reports recall losses at each funnel boundary and compares upvoted vs downvoted ranking quality, recomputing normalization from raw persisted values.
17. `explain` answers "why did this article show up (or not)?" entirely from persisted data.
18. Evaluation selects runs through one typed predicate over the **real** `RunStatus` vocabulary, a normal successful run is evaluable, and a budget-degraded run still contributes the metrics its completed stages support.
19. No field of an article **ever observed** through a `no_external_ai_feed_ids` feed reaches Voyage or DeepSeek through any path — embedding, Stage A, Stage B, editorial summary, or profile rebuild — including when only a secondary cluster source is protected, and including after a later re-ingest through public sources alone; proven by a recording mock over a full run in which such an article is published.
20. A fidelity replay selects the prose profile that was effective at `as_of` from stored profile *text*, or records that none existed, and its feature-time policy is recorded so its results can never be pooled with counterfactual results.
21. Two concurrent mutating invocations cannot both proceed — including `generate` against `profile rebuild` — the loser fails immediately naming the holder, and a killed holder's lock is free for the next process with no timeout heuristic.
22. Provider ceilings are enforced per **UTC billing day** across every provider-using command and per **budget class**, accounted **per HTTP attempt** against a reservation that is a genuine upper bound, and a crash after dispatch leaves that reservation persisted rather than zero. No `shadow` or `maintenance` work can consume `publication_reserve_daily_usd`, in any invocation order.
23. `facet_preference` contributes to utility only, never to admission, so no article gains admission advantage from having been admitted before.
24. Each learned signal is gated by evidence **of its own kind**: 20 ratings of which one is embedding-backed leave the embedding gate near its floor.
25. At the ceiling, mandatory auto-includes, the interleave exposure, and editor picks resolve by one deterministic merge order, with Stage B prompted with the capacity that actually remains.
26. The churn rule suppresses an article only when its **most recent** observation within the window is below the floor, with the window measured in observation time.
27. No API keys and no raw embedding vectors are emitted to logs or reports.
28. All current tests pass after intentional expectation updates, and every new module has deterministic unit coverage.

---

## 35. Deliberately deferred, and what would trigger it

Do not block on these; the defaults above are chosen to be safe, and each item names its trigger.

| Deferred option | Trigger to revisit |
|---|---|
| `output_dimension = 1024` | §27 shows a measurable retrieval difference vs 512. |
| Facet schema v2 (full vocabulary, technicality/topic_group scored) | ~300 ratings accumulated. |
| Dedicated pre-Stage-A facet extraction stage, and with it `facet_preference` in the admission blend | §27.3 metric 6 shows profile contamination below 85% agreement, **or** facet preference is shown to improve the admission cut — but the stage must land *first*, since uniform coverage across the eligible set is what makes the signal admissible at all (§16.4). |
| **Ridge-regularized linear probe on embeddings** instead of (or beside) facet scoring | ≥ 200 ratings. A regularized linear model learns *which* embedding dimensions discriminate, costs zero LLM tokens, and handles the low-*n* regime better than either centroids or sparse facet statistics. Evaluate it head-to-head against the kNN signal; the facet path keeps its explainability value regardless. |
| MMR instead of cluster caps | Cluster caps prove too blunt — e.g. legitimate deep coverage of one topic is repeatedly suppressed. |
| Connected components (union-find) instead of leader clustering | Evaluation shows news cycles fragmenting across leaders and slipping past the cap (§20). |
| Fenced SQLite lease or another multi-host generation coordinator instead of the file lock | Generation needs to span hosts, which a file lock cannot coordinate (§24.2). The provider ledger is *not* deferred — it ships in §7.6. |
| Purpose-built current projections (latest-rating-per-article, ever-published set) maintained transactionally from events | Event scans become measurably expensive. At tens of rating events per week and ~20 publication events per issue they are not, and a second query path is the thing R6-H2 removed — so any such projection must be defined and tested as an exact query-equivalent cache, never as a parallel semantic. |
| Two vectors per article (title+lead for interest matching, full body for preference and clustering) | §27.3 metric 8 shows the 250-word floor is not enough to control short-document bias. The `article_embeddings` key accommodates it via a `kind` column. |
| Learned weight optimization | Enough labeled examples that hand-tuning is demonstrably worse. |
| Collapsing `scores` into `candidate_rankings` | Phase E. |

Also tunable without ceremony: rating lookback and half-life, `neighbor_k`, negative coefficient, evidence-ladder thresholds, retriever quotas, facet dimension weights, blend weights, cluster threshold and cap, shortlist size, exploration parameters, Stage B soft target and hard max.

The architectural commitments — union admission with quotas, full-content semantic embeddings, z-scored interest matching, evidence-gated rating-derived signals, presence-aware blending, quality/fit separation, cluster-capped diversification, and no forced filler — are the parts that should not drift.

---

## 36. Review findings → resolutions

Both reviews were checked against the code; every code-level claim in them was verified as accurate.

### R1 (first review)

| # | Finding | Resolution |
|---|---|---|
| H1 | Facet cache key cannot honor invalidation; profile in facet prompt | §7.3 full-key PK including model + prompt_version + input_hash; `input_hash` defined over the exact effective input; §15.3 states the profile-dependence tradeoff explicitly, records `profile_version` as provenance, and adds a stability metric with a defined escape hatch. Taste profile is *not* removed — facets ride inside Stage A (§15.1) — but the decision is now explicit and measured rather than accidental. |
| H2 | `candidate_rankings` not per-run, insufficient for replay | §6.3 run identity; §7.4 `run_manifests`; §7.5 keyed by `run_id`, with `admitted_by`, `excluded_reason`, `terminal_stage`, versioned `explanation_json`, lifecycle filtering, and an anti-`COALESCE` write policy. Replay claim narrowed in goal 7 and §27.2. |
| H3 | Undefined as-of semantics; future-data leakage | §6 in full: single `as_of`, named modes with an explicit feature-time policy, every history query bounded, leakage tests in §31.12. |
| H4 | One centroid collapses multi-modal taste | §13.2 replaces centroids with a signed time-decayed top-k neighbor signal; multi-modal recall test in §31.4. |
| H5 | Shadow mode can consume the production budget | §24 production-first ordering, separate shadow slice, atomic reserve-then-spend, per-provider persisted ledger (§7.6). Largely defused by §15.1: Phase A shadow work is embeddings-only. |
| H6 | Target/ceiling/auto-include/`--max-articles` precedence contradictory | §21.2 defines `soft_target` and `hard_max` separately with one auto-include precedence rule and a config switch; §20 defines shortlist-cap precedence; tests in §31.7. |
| M1 | Missing-signal normalization underspecified | §12 is now a normative contract. |
| M2 | Vote-time and exposure semantics | §13.1 uses `rated_at`, documents the flip-resets-recency quirk; exposure stays out of the label. |
| M3 | Feed-prior rebuild not atomic or fully specified | §7.7 defines dedup, split, fallback, mean-not-max, and a single-transaction rebuild. |
| M4 | No prompt-injection / data-handling policy | §25, including the provider opt-out (now `no_external_ai_feeds`, §25.1). |
| M5 | Facet stage expensive, necessity untested | §15.1 removes the dedicated stage in V1; §27.3 metric 6 measures stability; §35 names the trigger to add it back. |
| M6 | Partial-deployment compatibility undefined | §7.8 `assessment_version`, both regimes readable; §31.10 migration test. |
| L1 | Pricing/compatibility are metadata | §4 re-verification rule; model isolation documented as a deliberate reproducibility choice. |
| L2 | Unicode-safe caps, token margin | §9.3 char-boundary caps, per-input and aggregate budgets, truncation counting. |
| L3 | MMR clamping and utility scale | §19 fixes the 0–100 scale explicitly; §20 clamps similarities; §7.1 validates finiteness and norm. |
| Nits | Row coverage, SQLite booleans, cardinality validation, exploration salt, approximate language | §7.5 thin rows + `CHECK` constraints; §15.2 post-deserialize validation; §6.4 salt in manifest; approximate quantities replaced with named config fields throughout. |

### R2 (second review)

| # | Finding | Resolution |
|---|---|---|
| C1 | Percentile ties inject article-ID bias | §12.1 mandates mid-rank percentiles and forbids ID tiebreaking inside the normalizer; §31.3 adds the constant-signal test. |
| C2 | Stage A split silently kills the churn rule | §18.4 keeps `scores.llm_score` written from `quality_score`, adds the reader-fit column, and requires the regression test. |
| C3 | Confidence damping is a no-op | §13.2 removes the whole-run multiplier; §14 damps the *weight* and redistributes. |
| H1 | Absent signals must be dropped and weights renormalized | §12.3. |
| H2 | Facet vocabulary too large for available ratings | §15.2 cuts scored facets to 4 dimensions / 16 values, keeps richer descriptive fields for the profile prompt and `explain`, and defers the full vocabulary to schema v2. |
| H3 | No quality floor on semantic retrievers; short-doc bias | §16.3 word-count and roundup floors on semantic admission paths only; inverse regression test in §31.5; §27.3 metric 8 monitors it. |
| H4 | Doubled sequential LLM round-trips, no concurrency | §15.1 removes the extra stage entirely; §9.3 and §18.5 specify bounded concurrency with pre-spawn budget checks; §24 covers trip semantics under concurrency. |
| H5 | Rerun state interleaving | §7.5 write policy: new `run_id` per invocation, no `COALESCE`, delete-then-insert in one transaction, plus the §31.8 regression test. |
| H6 | `explain` / criterion 10 unsatisfiable | §7.5 writes thin rows for hygiene-excluded articles with `excluded_reason`; criterion 12 rewritten to be testable. |
| M1 | Backfill cost guard, unbounded storage, 512 default | §26.1 estimate + `--yes` + conservative defaults; §7.1 retention; §4.1 makes 512 the default. |
| M2 | `Source:` in the embedding document | §10.1 drops `Source:` and `Author:`, with the reasoning recorded; §31.1 asserts it. |
| M3 | Broad interests dominate max-cosine | §11.2 z-scores each interest across the day's pool; §11.1 makes the bare interest name the default text and keeps v1 for comparison. |
| M4 | Exploration unbounded when least useful | §17 ramps exploration up with evidence, defines "outside the dense region" as a percentile, defaults to 8. |
| M5 | Phase A cannot shadow what it claims | §32 Phase A shadows admission only, with recall diagnostics as the exit gate. |
| M6 | Voyage ceiling not day-scoped | §7.6 originally added `runs` columns and symmetric preloading; **superseded by R5-H1**, which replaced nominal-date preloading with the `provider_usage` ledger. |
| M7 | Replay overstated | Goal 7 narrowed; §27.2 states exactly what is and is not reproducible and requires recomputing normalization from raw values. |
| M8 | `--max-articles` is not currently a ceiling | §21.2 says "must become", with the composition rule and tests. |
| L1–L9 | Sort keys, `select_without_llm`, seed rule, exploration bonus, facet model key, skip flags, config validation, signal correlation, dry-run rows | §21.4 `ordering_score` at all sites incl. `select_without_llm`; §20 seeds by utility with auto-includes eligible and protected items counting toward clusters; the undefined "exploration/novelty bonus" is deleted from the blend (exploration is an admission quota, not a score term); §7.3 puts `model` in the facet key; §28 ships both skip flags together; §30 lists the config ordering constraints; §18.3 and §27.3 metric 7 track correlation instead of asserting independence; §7.5 documents dry-run rows. |
| N1–N7 | Stale paths, churn lookback config, vacuous criterion, root-config typos, budget framing, title patterns, re-verification | Paths corrected in the header and §8; churn constants moved to config (§30); criterion 14 rewritten around idempotency; §9.1 notes the typo hazard and the startup log; §4.1 reframes the Voyage budget as a runaway guard; §32 Phase E lists the title-pattern retirement; §4 adds the re-verification rule. |
| A1 | Linear probe instead of facets | Adopted as the documented middle path: facets ship for extraction, storage, `explain`, and the profile prompt, but scored facets are minimal and evidence-gated; the probe is queued in §35 with a 200-rating trigger. |
| A2 | Cluster caps instead of MMR | Adopted in §20; MMR kept as the documented fallback. |
| A3 | Two vectors per article | Deferred in §35 behind the short-document metric; the cheap fixes (§16.3 floor, §11.2 z-scoring, §10.1 no feed title) are taken now. |
| A4 | 512 dimensions | Adopted as the default (§4.1). |

### Reviewer open questions, answered

1. **Historical `--date D`: reproduce day D, or re-curate with today's knowledge?** Both, as distinct named modes — §6.2. `--date` keeps today's behavior (`recurate`); `--as-of-date` opts into `replay`.
2. **Is `--max-articles` absolute against excess auto-includes?** Yes by default; `auto_includes_exceed_max = false`, with excess auto-includes trimmed by utility and reported (§21.2).
3. **Should facets be reader-independent?** Ideally yes, but not at the cost of a second LLM stage in V1. §15.3 accepts mild profile dependence, records provenance, and measures stability with a defined escape hatch.
4. **Is exact replay a hard requirement?** No. Scalar replay is required; vector-exact replay is not, and the claim is narrowed (§27.2).
5. **Budget split between publication-critical and shadow work?** §24: production-first ordering plus a `shadow_max_daily_usd` cap — strengthened by R7-H2 into a real `publication_reserve_daily_usd` that shadow cannot touch. V1 shadow work is embeddings-only, so DeepSeek contention is near zero.
6. **Private/authenticated feeds?** Unknown — the operator must confirm before the first live run. `curation.no_external_ai_feed_ids` (§25.1) is the opt-out, and it ships in V1 regardless.
7. **Phase exit criteria?** Numeric criteria for every phase in §32.
8. **How many ratings/articles exist today?** The production DB is not readable from the development account, but the service has published on the order of one issue (§2), so rating evidence is effectively zero. This is why the evidence ladder (§14) exists and why day-one ranking leans on the 230 standing interests. §2 gives the query the operator should run and record.
9. **Current `generate` wall clock and publish deadline?** Unmeasured. §18.5 requires recording Stage A wall clock in the run report, and §9.3/§18.5 specify bounded concurrency regardless, since it is a few lines against a real 05:30 deadline.
10. **Does `enabled = false` disable embedding generation?** No — §32 states the flag semantics explicitly; feature collection is controlled by `voyage.enabled` and `--skip-embeddings`, so Phase A can collect data while the old selector stays authoritative.

### R3 (re-review of v2, 2026-08-19)

All four code-level claims were verified: `RunStatus` really has no `complete` variant (`src/report.rs:19-41`, written verbatim by `db::finish_run`); `select.rs`'s candidate renderer really appends a body-derived `opening:` blurb; `editorial::summarize_article` really sends title plus body; and `profile::store` really overwrites the `kv` singletons, so historical profile text is unrecoverable.

| # | Finding | Resolution |
|---|---|---|
| C1 | Evaluation filters on a nonexistent `complete` status | §7.6 defines eligibility against the real five-value vocabulary through **one typed predicate**: `ok`/`degraded` + final manifest for outcome metrics, dry runs only on request, never `running`/`failed`. No status is renamed or added. §31.9 tests all five, including the "a normal successful run *is* evaluable" case that would have caught this. |
| C2 | `no_external_ai_feeds` leaks through Stage B, editorial, and profile | §25.1 rewritten: scope is total (no field, not just body), enforcement is a `provider_policy::ExternallyProcessable` wrapper that every provider call must accept, protected auto-includes are reinserted after Stage B with local summaries, and §31.10 asserts with a recording mock that no request body contains any protected field. Renamed from `no_external_content_feeds`. Landed first in the commit sequence (§33 step 0). |
| H1 | Phase A gate needs an upvote that cannot exist | §32 splits the gate: the observable retention half stays; the counterfactual half is replaced by **blinded operator adjudication** (10 union-only + 5 controls per week, stored in `adjudications`, §7.4d) with a "no worse than control" bar. Measured user yield moves to Phase B, which adds one interleaved issue slot and an `interleave_pick` flag so the cohort is identifiable. |
| H2 | Replay needs profile *text*, not just a version | §7.4b adds `taste_profile_versions`, written transactionally with the `kv` update and seeded from the current profile in migration `0002`; §6.2 selects `MAX(built_at) <= as_of` or records `profile_version = NULL`. Tested in §31.12. |
| H3 | Cached facets create an incumbency-only admission signal | §16.4 removes `facet_preference` from the preliminary blend and reweights the remaining five to sum to 1. The distinction between an *outage* (presence-aware renormalization is right) and *informative missingness* (it is not) is now stated explicitly. Facets apply to utility only; acceptance criterion 21 locks it. |
| H4 | Cross-process budgeting is unenforceable | §24.2 adopts a SQLite generation lease (§7.4c) instead of a distributed ledger: one mutating run at a time, immediate failure naming the holder, `--wait-for-lease` to block, expiry-based crash recovery. Also fixes publication and feed-prior rebuild races. Tested in §31.11. |
| M1 | Exploration ramp never reaches full strength | §17 divides by the ramp **width** and introduces an explicit `exploration_full = 30.0`, deliberately later than `evidence_full = 20.0`, with the boundary table and required tests. |
| M2 | The algorithm is not single linkage | §20 renames it **leader clustering**, compares candidates against leaders only (bounding each cluster to a ball around its leader), states the order semantics, and requires the A~C/B~C/A≁B bridge test. Union-find remains a measured switch, not a naming fix. |
| M3 | Manifest creation conflicts with its NOT NULL fields | §7.4 makes the write two-phase: `provisional` at run start, `final` once evidence weights and profile selection exist. (R4-M2 then removed the dependency on a first ranking row existing, so zero-candidate runs finalize too.) Evaluation requires `final`. |
| M4 | Invalid enum in the Stage A example | §18.1 uses `first_hand_account`; §15.2 documents the postmortem → `first_hand_account` mapping inline; §31.7 requires every token in prompt examples and fixtures to parse. |
| L1 | `article_facets` missing the FK | Added, with `ON DELETE CASCADE`. |
| L2 | Two writable sources of run mode | `runs.mode` dropped; `run_manifests.mode` is authoritative and joined to. |
| L3 | "Budget-degraded" vs "truncated and unusable" | §7.6 keeps `degraded` runs and decides eligibility **per metric** from `run_manifests.stage_completeness_json`; excluded-run counts are reported alongside every metric. |
| Nits | Criterion renumber, status wording, `CHECK` on `source`, `CHECK` on manifest flags | All applied (§16.1, §7.3, §7.4). |
| A1 | Facets strictly post-admission | Adopted (H3). |
| A2 | Controlled interleaving | Adopted for Phase B at one issue slot; Phase A uses adjudication (H1). |
| A3 | Central provider-policy wrapper | Adopted (C2). |
| A4 | Serialize generation rather than a ledger | Adopted (H4). |

**R3 open questions, answered:** (1) `ok` + `degraded`, dry runs on request, per-metric stage gating. (2) Everything — body, title, feed, facets, and rating-history metadata. (3) Blinded adjudication in Phase A, one interleaved slot in Phase B. (4) Yes, profile text is stored. (5) No overlap; the second invocation fails immediately unless `--wait-for-lease`. (6) Leader clustering, deliberately, not single linkage.

### R4 (re-review of v3, 2026-08-19)

| # | Finding | Resolution |
|---|---|---|
| C1 | One global `W` activates signals with no compatible evidence | §14.1 replaces it with four weights — `W_embedding`, `W_facet`, `W_feed`, `W_global` — each summed only over ratings that can inform that signal, each gating its own signal; `W_global` is left to exploration maturity and telemetry. Stored in `run_manifests.evidence_weights_json`. §25.1's impossible claim that protected ratings update kNN is corrected. Required test: 20 ratings, one embedding-backed, leaves the embedding gate near its floor. §14.2 adds a log line for the divergence case, which is the real-world symptom of a coverage problem. |
| C2 | `as_of` and backfill contracts are unsatisfiable as specified | Three separate fixes. **Feed priors** (§7.7): the `feed_priors_v2` table is deleted; priors are derived per run into an immutable in-memory map, so a replay cannot overwrite live state and `serve` cannot race a run. **Churn** (§7.8): the rule bounds on observation time rather than nominal `run_date` — though the v4 mechanism for that (columns on `scores`) was itself wrong, and R5-C1 replaces it with `candidate_rankings`. **Feature time** (§6.2): `replay` splits into `fidelity` (`created_at <= as_of`) and `counterfactual` (later features permitted), recorded as `feature_time_policy`, with §27.1 forbidding results from the two being pooled. |
| H1 | The expiring lease is unfenced and can expire mid-stage | §7.4c replaces it with an OS advisory file lock (Alternative D). No TTL means no reclamation race, no fencing token, and no heartbeat; the kernel releases on process death, so crash recovery needs no timeout heuristic and a stale holder cannot steal the lock back. The DB row survives as diagnostics only. §31.11 adds the long-stage and stale-row tests. |
| H2 | Phase B interleave depends on a Phase C score and guarantees nothing | §32 fixes the rule exactly: rank by the **preliminary blend** (available in Phase B), require the §16.3 floor plus `interleave_min_quality = 6.0`, then **deterministically reinsert** after Stage B — a guaranteed exposure through the same path as protected auto-includes. Exit requires ≥ 5 actual exposures, not merely 7 runs. |
| H3 | Migration `0002` cannot hash a profile row in plain SQL | §7.4b moves seeding to an idempotent Rust bootstrap (`db::bootstrap_profile_history`) run after `sqlx::migrate!`, since SQLite has no SHA-256 and a placeholder hash would break the identity contract. §31.13 tests absent, malformed, valid, and already-seeded states. |
| M1 | Adjudications not tied to a run | §7.4d splits into `adjudication_batches` (keyed by `run_id`, with `algorithm_version` and `sample_seed`) and `adjudications` (with `display_order` stored apart from `arm`). Deduplication is per article globally with a 30-day cooldown; the CLI prints the run and batch before collecting labels. |
| M2 | A zero-candidate run can never finalize | §7.4 finalizes on reaching the pipeline point, in a transaction that inserts *zero or more* rows. §31.8 adds the zero-eligible-candidate case. |
| M3 | `stage_completeness_json` is authoritative but unversioned | §7.4 makes it a versioned typed structure, required when `manifest_status = 'final'`, with parse or unknown-version failure making the run ineligible for every metric under an explicit diagnostic. All filtering happens after typed decoding, never in SQL JSON paths. |
| L1 | `explain` still said "latest complete run" | §26.3 now says latest **eligible** run under the §7.6 predicate, excludes dry-run/shadow by default, and allows `--run-id` to reach an ineligible run for debugging. |
| L2 | `QueryFragment` is not a thing in this codebase | §7.6 specifies an implementable shape: `db::eligible_runs(kind, from, to) -> Vec<RunRef>`, with `QueryBuilder<Sqlite>` as the alternative for pushed-down predicates. |
| L3 | "Actual tokens" is not always observable | §24 states the conservative rule: keep the reservation estimate when no usage payload exists, reconcile only from trustworthy ones, and report estimated versus provider-reported separately. |
| Nits | Duplicate criterion 18, stale §31.9 pointer, dry-run/lease wording, facet-stage trigger | All fixed: criteria renumbered through 24; the R1 table points to §31.12; §24.2 simply says dry runs take the lock; §15.1's trigger is now an offline counterfactual evaluation, since facet preference is barred from admission until the stage exists. |
| A1–A4 | Per-signal gates, run-local preference snapshots, fidelity/counterfactual split, OS lock | All four adopted (C1, C2, C2, H1). |

**R4 open questions, answered:** (1) Both, as separate named modes with a recorded feature-time policy (§6.2). (2) Yes — a protected or feature-less rating counts toward `W_global` and exploration maturity, and toward `W_feed` when attributable, but never toward `W_embedding` or `W_facet`. (3) Neither: there is no global priors table at all; priors are run-local, and `ratings` is canonical. (4) Answered again, differently, by R5-C1 below: the churn rule moves to `candidate_rankings`, because columns on an overwriting key are not a history. (5) A guaranteed exposure, ranked by the preliminary blend, with a quality floor. (6) Yes, stages can exceed 30 minutes — which is exactly why the TTL is gone rather than fenced.

### R5 (re-review of v4, 2026-08-19)

All five code claims verified: `scores` is still keyed `(article_id, run_date)` and `db::upsert_score` writes through it; `db::upsert_rating` overwrites `vote` and `rated_at`; `db::upsert_issue` overwrites `generated_at` and `replace_issue_articles` deletes and reinserts; `db::spend_for_date` sums `runs.cost_usd` by nominal date; and `Command::Profile(ProfileCommand::Rebuild)` is a real standalone DeepSeek-spending command that `main` runs after its own `Db::open_and_migrate`.

| # | Finding | Resolution |
|---|---|---|
| C1 | `run_id`/`scored_at` on `scores` is provenance, not history | Correct, and my v4 fix was wrong: the key still overwrites, so a recuration destroys the observation a fidelity replay needs. §7.8 drops those columns and moves the churn rule to **`candidate_rankings`**, which is already keyed `(run_id, article_id)`, append-only, and joinable to `runs.started_at` for true observation time — no new table needed. `scores` is demoted to an explicitly labelled current-value projection. Ranking snapshots are now written by every run regardless of `personalization.enabled`, since the churn rule depends on them. |
| C2 | The fidelity contract queries mutable snapshots | Adopted the reviewer's recommended contract, minimally: §7.9 adds append-only `rating_events` (with the rating's feed set captured at vote time, closing the `sources_json` hole too) and `publication_events`. `ratings`, `issues`, and `issue_articles` stay as fast current-value projections, unchanged in shape. All temporal reads follow one rule — latest event at or before `as_of` — and §6.1's bullets are rewritten to point at the event tables. `generate --as-of-date` is kept. The contract is stated explicitly, including what it still does *not* cover (`content_html` overwrites). |
| H1 | Daily accounting omits billing day, commands, and crashes | §7.6 adds an append-only `provider_usage` ledger bucketed by the **UTC billing day of `reserved_at`**, covering `generate`, `features backfill`, and `profile rebuild` (`run_id` nullable). Reserve-before-dispatch is committed, so a crash leaves the conservative estimate rather than zero; settlement writes actual usage, and a failure with no usage payload keeps the estimate. `runs.cost_usd`/`runs.voyage_*` remain report rollups, not the guardrail. |
| H2 | Lock scope omits `profile rebuild`; acquisition point undefined | §24.2 replaces the prose list with an explicit command matrix — `generate`, `profile rebuild`, `features backfill`, `features prune`, `backfill-social` hold it; `serve`, `evaluate`, `explain` do not — and specifies acquisition in `main`: a short migration critical section for every command, then a held lock for mutating ones, with the guard passed into the command. |
| H3 | A manifest goes `final` before its completeness data exists | §7.4 splits the lifecycle into `provisional` → `ranking_fixed` → `final`. `ranking_fixed` is what makes candidate rows interpretable; `final` is written in the same transaction as `runs.status` at `finish_run`, so completeness and status cannot disagree. Completeness gains `admission`, `utility`, `diversification`, `selection`, and `publication` — including the admission field §7.6's own gating rule required. Zero-candidate runs still finalize. `explain` accepts `ranking_fixed`. |
| H4 | No total precedence rule at `hard_max` | §21.2 adopts pre-reserved capacity (the reviewer's third alternative): Stage B is prompted with `editor_capacity = hard_max − mandatory − interleave_slot`, so the ceiling it is given is truthful and editor picks are never evicted. A four-step merge order settles the rest, and when mandatory content fills the issue the interleave simply does not run — it is a measurement device and must not displace an operator's auto-include. `interleave_selected` counts only real exposures. |
| M1 | Malformed bootstrap leaves the pointer malformed | §7.4b repairs `kv[profile_version]` in the same transaction and allocates future versions from `MAX(version) + 1` rather than the pointer. The malformed-state test now runs a rebuild afterwards and asserts version 2 with version 1 preserved. |
| Alt | One append-only observation layer | Adopted as the organizing idea, scoped to the three tables that actually break fidelity, with the existing tables kept as projections — rather than either a full event-sourcing rewrite or dropping the replay claim. |

**R5 open questions, answered:** (1) Yes — append-only rating and publication history is now mandatory and specified (§7.9). (2) The provider's real UTC day, across generation, backfill, profile rebuild, retries, and crashes (§7.6). (3) Mandatory auto-includes first, then the reserved interleave, then editor picks; when auto-includes fill the ceiling the interleave yields (§21.2). (4) "Ranking inputs fixed" and "run outcome complete" are now two distinct states, `ranking_fixed` and `final` (§7.4).

### R6 (re-review of v5, 2026-08-19)

All five code claims verified, including that `pipeline::record_issue` calls `upsert_issue` and `replace_issue_articles` as two separate transactions *after* files are already copied to the publish directory.

| # | Finding | Resolution |
|---|---|---|
| H1 | The churn SQL selects every low row, not the latest, and windows on nominal date | §7.8 replaces the query with a `ROW_NUMBER() … PARTITION BY article_id ORDER BY r.started_at DESC, cr.run_id DESC` form that keeps one observation per article, and anchors the window to `runs.started_at`. The prose said "latest observation wins" while the SQL did not implement it — an implementation agent would have written the SQL. Pruning also moves to `started_at`, since pruning on the nominal axis reintroduces the same defect. Tests now cover low→high, high→low, and a low score observed today while recurating an old date. |
| H2 | Event tables declared authoritative, then bypassed in live/recurate | §7.9 now uses the event tables in **every** mode, with `live`/`recurate` passing `as_of = now`. The reviewer's two counterexamples are recorded in the plan because they are the argument: a twice-published, twice-rated article yields two projection rows but one latest event, and an article dropped by a republish looks unpublished to `issue_articles` while `publication_events` correctly still excludes it. `rating_events` now stores the **finished** attribution as a versioned `feed_credits_json` (with `via_fallback`), not the pre-fallback direct-feed set, so a discovery-only rating no longer needs mutable article state to reproduce. Ordering is `event_at DESC, id DESC`. |
| H3 | The ledger cannot express shadow slices or retry accounting | §7.6 adds `budget_class` (renamed by R8-H2 to `publication` \| `shadow` \| `maintenance`) — required because both classes occur inside one run, so `run_manifests.shadow` cannot classify a request — with `shadow_max_daily_usd` defined as a **sub-limit inside** the provider ceiling rather than an additive allowance, and shared feature collection classed `production` — that last part superseded by R8-H2. Rows are now **per HTTP attempt**, linked by `request_id` + `attempt`, so a 5xx with no usage payload keeps its estimate instead of being erased by a successful retry's settlement. The conservative estimate formula is spelled out (DeepSeek input + max output at output prices, no cache discount; Voyage input-only). |
| M1 | New authorities not propagated into the normative sections | One pass over §5, §8.1, §9.4, §23, §24, §24.2, §29, §30, §31.8, and Phase A: every stale reference to nominal-date preloading, canonical `ratings`, "finalize with the first ranking rows", the old completeness block, and the date-keyed adjudication table now names `provider_usage`, `rating_events`, `ranking_fixed`, and the §7.4 schema. Superseded cells in the older tables are labelled rather than left as competing instructions. |
| M2 | Observation seeding coupled to profile bootstrap, no marker | §7.4b splits `bootstrap_profile_history()` from `bootstrap_observation_history()`; each records a versioned `kv` marker **in the same transaction as its seeding**, so an interrupted attempt leaves neither rows nor marker. Event seeding no longer depends on a profile existing. Tested against a projection-only database and an interrupted seed. |
| M3 | Failure-state manifest semantics contradictory | §7.4 adds a transitions-by-failure-point table: `provisional` before preference capture, `ranking_fixed` after it, `final` with terminal completeness when `finish_run` runs on an error path. `evaluate` excludes `failed` in all three; `explain --run-id` accepts all three. |
| M4 | Publication authority stops short of the publish boundary | §7.9 puts `issues`, `issue_articles`, and `publication_events` in **one** transaction, and states plainly that a publication event means "published and recorded", not "momentarily visible". A startup reconciliation check reports files in the publish directory with no `issues` row, rather than pretending the crash window does not exist. |
| L1 | Migration lock release/reacquire opens a gap | §24.2 keeps the same file descriptor for lock-holding commands; only `serve`, `evaluate`, and `explain` release after the migration section. |
| L2 | Deferred table still defers the provider ledger | Renamed: what is deferred is a multi-host generation coordinator, and the row now says the ledger ships in §7.6. |
| Nits | Typed `feed_credits_json`, ledger `CHECK`s, derived `billing_day`, Stage B over-cap ordering, `serve` wording | All applied. Over-cap responses are trimmed from the tail of the model's own ordering (one rule, not two); §23 now says correctly that `serve` *does* append to the event authority and that `as_of`-bounded reads are what make concurrent votes invisible to a run. |
| Alt | Always read the observation layer | Adopted (H2). The "query-equivalent projections" fallback is recorded in §35 against the day event scans become measurable, which at tens of events per week they are not. |

**R6 open questions, answered:** (1) Recently *observed* — the window is `runs.started_at`, and the SQL now matches the prose. (2) A sub-limit inside each provider's daily ceiling; shared feature collection was classed `production` here, **superseded by R8-H2**, which classes by purpose instead. (3) Failure after `ranking_fixed` stays `ranking_fixed`, or reaches `final` with terminal completeness when `finish_run` runs; never "always provisional". (4) It means the successful database commit; the file-visible-but-uncommitted window is reported by startup reconciliation rather than modelled.

### R7 (re-review of v6, 2026-08-19)

Both code claims verified: `prefilter::is_auto_include` matches string entries as substrings of `article.url`/`canonical_url` and never sees a feed URL, and `SourceRef` carries only `entry_id`, `feed_id`, `feed_title`, `category`, and `kind`.

| # | Finding | Resolution |
|---|---|---|
| H1 | The privacy matcher cannot identify a private feed | §25.1 replaces `no_external_ai_feeds: Vec<String>` with typed **`no_external_ai_feed_ids: Vec<FeedId>`**, non-numeric entries a startup error. Reusing the `always_include_feeds` matcher would have searched the *article's* URL for a *feed's* host — so a private feed at `reader.internal/private.xml` linking to public sites would have matched nothing while the wrapper faithfully shipped its contents. Classification is over **every** source in the cluster, not `best_entry_id`'s. Substring matching is also simply wrong for a deny rule. A domain policy, if ever wanted, gets its own field with parsed-host semantics (§35). Adversarial classification tests added, and the full-run mock now uses the secondary-source case. |
| H2 | A sub-limit inside a shared ceiling is not a slice | §7.6 adds per-provider **`publication_reserve_daily_usd`**, admitting `shadow`/`maintenance` only when `total + estimate <= max_daily_usd - reserve`, while the publication class may use the whole ceiling. v6's claim that shadow "can never consume the production slice" did not follow from a shared ceiling: at Voyage's defaults a shadow command running first could leave $0.05 for the 05:30 timer, and production-first ordering protects nothing across invocations. Defaults are now internally consistent (0.25 = 0.05 reserve + 0.20 shadow cap), with an order-sensitive test. |
| H3 | "Atomic check-and-reserve" had no named primitive | New §24.0: a provider-scoped `tokio::sync::Mutex` around a short **`BEGIN IMMEDIATE`** transaction that re-sums inside the transaction and inserts before commit, with HTTP strictly outside it. A deferred SQLx transaction lets two `buffer_unordered` siblings both read the same total and both fit; `UNIQUE (request_id, attempt)` constrains rows, not sums. The 30s `busy_timeout` already configured on the pool covers the short contention window. Barrier-based concurrency test asserts admitted estimates never exceed either cap and that refusals precede dispatch. |
| H4 | The legacy `scores` churn fallback cannot honor the new semantics | §7.8 **deletes the fallback**. It could only have ranked recency by nominal date (wrong axis) and would have let a stale projection row suppress an article a newer observation had cleared (wrong value). The cost is one `recent_rejection_lookback_days` window of weaker suppression against a store holding roughly one issue (§2). The correct bridge for a future migration against real history — a one-time snapshot with an explicit expiry, consulted only where no observation exists — is recorded rather than built. |
| M1 | v5 authorities left in the normative worklist | Pass over §7.7, §23, §25.1, §30 (`db.rs`, `server.rs`, `publish.rs`, `main.rs`), and the eligibility table: rating events store the completed `feed_credits_json`, preference loads read latest events rather than ratings joined to current sources, `bootstrap_observation_history()` appears in the startup sequence and the concurrency test, `record_issue` is one transaction over `issues` + `issue_articles` + events, `main` keeps the same file descriptor, and `explain` accepts `provisional`. |
| Alt | Typed feed IDs; retire legacy churn state; serialize admission only | All three adopted (H1, H4, H3). |

**R7 open questions, answered:** (1) Miniflux feed subscriptions only, in V1; a domain policy would be a separate, separately-named field. (2) Incapable — hence the production reserve, not merely a shadow cap. (3) No: at roughly one issue of history, one week without legacy churn suppression is cheaper than a second, weaker semantic path.

### R8 (re-review of v7, 2026-08-19)

Both code claims verified: `db::upsert_article` sets `sources_json = excluded.sources_json`, replacing provenance wholesale on every re-ingest, and `curate::approx_tokens` is `text.len().div_ceil(4)`, documented in the source as a crude English-prose average.

| # | Finding | Resolution |
|---|---|---|
| H1 | Protected classification is not durable across re-ingestion | §25.1 adds `article_feed_observations` — an accumulate-never-delete record of every feed ever seen carrying an article — and builds the run's protected set from its intersection with the configured IDs. v7 checked the *current* cluster, so a protected article re-ingested a day later through a public mirror alone would silently lose its protection while the wrapper faithfully shipped it. Privacy provenance and ranking provenance now have separate storage, since §7.7 legitimately wants current sources for candidate feed affinity. The rule is now stateable: once observed through a protected feed, an article stays protected until the operator removes that feed ID. Seeded at migration, with pre-migration overwrites acknowledged as unrecoverable. |
| H2 | The reserve is bypassed by classing reusable shadow work as production | Classes are renamed and redefined **by purpose**: `publication` \| `shadow` \| `maintenance`, carried in a typed `BudgetContext` threaded from the top-level command and never inferred from operation name or cacheability. Embeddings in a shadow run are `shadow`; a dry run is `shadow`; a standalone profile rebuild is `maintenance`; a rebuild inside an issue-producing run is `publication`. v7's "cache is reusable, so class it production" rule would have let Phase A — which is *primarily* embeddings — consume the whole ceiling including the reserve, refuting its own premise. Order-sensitive tests for all three cases. |
| H3 | The "conservative" reservation was not an upper bound | §7.6 replaces `approx_tokens` with `payload_utf8_bytes + per_request_overhead_tokens`: every token consumes at least one UTF-8 byte, so bytes bound tokens, while an English average is beaten by code, punctuation, and non-Latin text. An under-reservation admitted just below the ceiling cannot be repaired after dispatch, however atomic the transaction. The ~4× looseness costs little because settlement replaces estimates with actuals immediately, so inflation applies only to the ≤4 attempts in flight. Adversarial-payload tests added, and usage reported above the bound trips the meter rather than being absorbed. |
| M1 | Bootstrap assumes a running `serve` is already the new binary | §7.4b drops that claim and adds an explicit cutover protocol — stop `serve`, migrate, restart — because an old `serve` writes only the `ratings` projection, and the durable marker would prevent any later repair of the vote it misses. Downtime is seconds and HMAC rating links are re-openable. |
| M2 | Two tests still required the deleted legacy fallback | §31.12 and §31.13 now assert the opposite: a v1 `scores` row survives migration and remains readable as a projection, and is never consulted by churn in any mode. |
| L1 | `VoyageConfig` omitted the reserve | Added to the struct, to §30's config list, and documented: `shadow_max_daily_usd` lives under `[curation.personalization]` and applies independently to *each* provider's ledger. |
| Nits | §24.0 numbering; historical rows using the old key name | Renumbered to §24.1/§24.2 with cross-references updated; superseded historical cells now say so. |

**R8 open questions, answered:** (1) Yes — protection persists until the feed ID is removed from configuration; a day without observing the feed does not unprotect. (2) Only an actively issue-producing `generate`; dry runs, standalone rebuilds, and shadow cache warming may not touch the reserve. (3) Strict pre-dispatch ceiling — which is why the estimator became a genuine upper bound rather than the acceptance criterion being weakened.

---

## 37. Final target behavior

A good outcome should feel qualitatively different from the current implementation:

- A quiet 900-word post from an obscure feed on a highly favored niche beats a viral generic HN story, because z-scored interest matching rescues it at the only irreversible cut — and it does so on **day one**, before a single rating exists.
- A 60-word release-note stub does *not* get rescued the same way.
- Once ratings accumulate, a reader who upvotes first-hand postmortems and downvotes vendor announcements sees that preference propagate across unrelated topics and publications through facets and neighbor similarity, not merely through feed priors — and before then, those signals contribute nothing rather than noise.
- Six articles about the same AI news cycle occupy at most two shortlist slots.
- The LLM editor receives a broad, high-quality, deliberately diverse set of candidates and is free to publish a short issue when that is what the day deserves.
- Every one of those outcomes is answerable after the fact from `explain`, including for the articles that never appeared.

The design goal, unchanged: **a recommendation system that maximizes candidate recall for this reader first, then uses explicit editorial quality judgment and an LLM editor to turn those candidates into a coherent newspaper** — one that is honest about how little it knows on day one, and that gets measurably better as it learns.

---

## 38. Appendix: complete configuration surface

Every number in this plan lives here, not scattered as literals across modules. Weights are validated as non-negative and **normalized in code**; TOML is never required to sum to 1. All of it is echoed into `run_manifests.ranking_config_json` so a ranking row from three weeks ago is still interpretable.

```toml
# --- existing keys whose meaning or default changes ---
target_article_count = 20              # now the SOFT target (§21.2)
prefilter_keep = 120                   # DEPRECATED alias for personalization.stage_a_keep
max_daily_usd = 2.0                    # DeepSeek only; unchanged semantics (§24)

[deepseek]
score_batch_size = 12
max_concurrent_requests = 4            # new (§18.5)
publication_reserve_daily_usd = 1.00    # inside the top-level max_daily_usd (§7.6)

[curation]
max_article_count = 25                 # hard ceiling (§21.2)
auto_includes_exceed_max = false       # false ⇒ hard_max is truly hard
recent_rejection_score_floor = 3.0     # was the STALE_LOW_SCORE const
recent_rejection_lookback_days = 7     # was the STALE_LOOKBACK_DAYS const
no_external_ai_feed_ids = []           # typed Miniflux feed ids; no field of these
                                       # articles ever leaves the host (§25.1)

[voyage]
enabled = true
base_url = "https://api.voyageai.com/v1"
model = "voyage-4-lite"
# api_key via DAILY_EPUB_VOYAGE__API_KEY only
output_dimension = 512
batch_size = 32
max_concurrent_requests = 4
max_input_chars_per_article = 60000
max_input_chars_per_batch = 900000
price_per_mtok = 0.02
max_daily_usd = 0.25                   # runaway guard, not a bill (§4.1)
publication_reserve_daily_usd = 0.05    # capacity only the `publication` class may use (§7.6)
embedding_retention_days = 120

[curation.personalization]
enabled = false                        # gates the new RANKING path only (§32)
stage_a_keep = 120
shortlist_keep = 60
interest_text_version = 2              # 2 = bare interest name (§11.1)
rating_lookback_days = 90
rating_half_life_days = 45
neighbor_k = 5                         # top-k rated neighbors (§13.2)
negative_coefficient = 0.75
evidence_floor = 5.0                   # decayed weight W below which learned signals are off (§14)
evidence_full = 20.0                   # W at which they carry full weight
facet_min_observations = 3             # per facet value (§13.3)
facet_support_k = 4.0
semantic_admission_min_words = 250     # floor on semantic retrievers only (§16.3)
exploration_max = 8                    # §17
exploration_floor = 15.0               # W below which exploration reserves nothing
exploration_full = 30.0                # W at which the full reservation is granted (§17)
interleave_union_only_slots = 1        # Phase B issue slots for union-only picks (§32); 0 disables
interleave_min_quality = 6.0           # legacy Stage A score floor for an interleaved pick (§32)
adjudication_cooldown_days = 30        # per-article re-sampling cooldown (§7.4d)
ranking_retention_days = 180           # must exceed recent_rejection_lookback_days (§7.8)
provider_usage_retention_days = 400    # append-only provider ledger (§7.6)
shadow_max_daily_usd = 0.20            # applied per provider, above each reserve (§7.6)
backfill_confirm_token_threshold = 5000000   # §26.1

[curation.personalization.quotas]      # guaranteed Stage A slots per retriever (§16.2)
interest = 20
heuristic = 20
embedding_preference = 20
feed_affinity = 5

[curation.personalization.weights.preliminary]   # §16.4
semantic_interest = 0.35
heuristic = 0.25
embedding_preference = 0.23
feed_affinity = 0.09
social = 0.08
# facet_preference is intentionally absent here — utility only (§16.4)

[curation.personalization.weights.utility]       # §19
llm_quality = 0.40
llm_reader_fit = 0.15
embedding_preference = 0.15
facet_preference = 0.10
semantic_interest = 0.10
feed_affinity = 0.05
heuristic = 0.03
social = 0.02

[curation.personalization.weights.facet_dimensions]   # §13.3
format = 1.0
depth = 1.0
evidence = 1.0
commerciality = 1.0

[curation.personalization.diversity]   # §20
cluster_threshold = 0.85
per_cluster_cap = 2
utility_protected = 15
```

Validation rules (§30): dimension ∈ {256, 512, 1024, 2048}; `1 <= batch_size <= 1000`; `1 <= max_concurrent_requests <= 16`; `0 <= cluster_threshold <= 1`; `per_cluster_cap >= 1`; `stage_a_keep >= shortlist_keep >= target_article_count`; `max_article_count >= target_article_count`; `evidence_full > evidence_floor >= 0`; `exploration_full > exploration_floor >= 0`; `interleave_union_only_slots < target_article_count`; `0 <= interleave_min_quality <= 10`; `0 <= publication_reserve_daily_usd < max_daily_usd` per provider, and `shadow_max_daily_usd <= max_daily_usd - publication_reserve_daily_usd` (a shadow cap above the remaining capacity is a configuration error, not a silent no-op); every `no_external_ai_feed_ids` entry parses as a feed id; **`ranking_retention_days > recent_rejection_lookback_days`** (the churn rule reads ranking snapshots, §7.8, so pruning them below the churn window would silently disable it); all weights, lookbacks, and budgets non-negative.

Startup logs the resolved `voyage.enabled`/`model`/`output_dimension` and the count of `no_external_ai_feed_ids` entries, so a typo in a section name (which the root `Config` silently ignores by design — §9.1) is visible in one line rather than discovered by an unexpected API bill or an unexpected leak.
