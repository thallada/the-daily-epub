
---

# YOUR STEP: 3 — Embeddings, signals, telemetry (plan §21 step 3)

You are working in a git **worktree** on branch `curation-v2-step3`, which branches from the step 1
commit. Another agent is implementing step 2 (the Claude editor) in parallel on the main checkout;
you will not see its changes and must not need them. Read `git log -3` and the current
`src/curate/`, `src/pipeline.rs`, `src/config.rs`, `src/db.rs`, `src/report.rs`, `src/main.rs` first.
Do not run `git checkout`, `git stash`, `git worktree` or `git commit`.

Scope (plan §4.3, §7.1, §7.2, §7.4, §7.5, §9, §12.2 normalization only, §12.4 preliminary blend,
§15.2 `explain`, §15.4 log line parts, §16, §17 Voyage row, §19 `[voyage]` + `[curation.ranking]`).
The old heuristic prefilter still gates in this step; signals are computed for every article that
passes hygiene and persisted, nothing about *selection* changes yet.

1. **`src/curate/embedding.rs`** (§4.3, §7.1, §7.2): `EmbeddingBackend` trait mirroring `ChatBackend`
   (with a mock for tests), `VoyageBackend` (`POST {base_url}/embeddings`, `Authorization: Bearer`,
   body `{input, model, input_type: "document"|"query", truncation: true, output_dimension, output_dtype: "float"}`,
   response mapped by index and length-checked, usage `total_tokens` metered), batching by `batch_size`
   with `buffer_unordered(max_concurrent_requests)`, retry 429/5xx via `RetryPolicy`, f32
   little-endian BLOB encode/decode with length and finiteness checks, `dot(a, b)` with a dimension
   check, and cache orchestration: `article_embeddings` keyed by `(model, dimension, input_hash = sha256
   of the embedded text)`, overwritten on change. Embedded text = `"Title: {title}\n\n{plain body}"`
   via `curate::prompt_text`, whitespace collapsed, cut at `max_input_chars` on a char boundary —
   **no feed name, author or scores**. Interests go to `interest_embeddings` with `input_type = "query"`
   and the bare interest string. A failed batch leaves those articles without embeddings; never fatal.
   A Voyage `UsageMeter` with `price_per_mtok = 0.02` and `max_daily_usd` (0.50) as a runaway guard.
   Key only from `DAILY_EPUB_VOYAGE__API_KEY`. Raw vectors never reach logs or reports.
2. **Config** (§4.3, §19): `[voyage]` (`enabled`, `base_url = "https://api.voyageai.com/v1"`,
   `model = "voyage-4-lite"`, `output_dimension = 512`, `batch_size = 32`, `max_concurrent_requests = 4`,
   `max_input_chars = 60000`, `max_daily_usd = 0.50`) and the complete `[curation.ranking]` block from
   §19 (`triage_max`, `deep_keep`, `shortlist_keep`, `assessment_reuse_days`, `rating_lookback_days`,
   `rating_half_life_days`, `neighbour_k`, `negative_coefficient`, `knn_floor/full`, `feed_floor/full`,
   `semantic_min_words`, `exploration_slots`, `embedding_retention_days`, `telemetry_retention_days`,
   `[curation.ranking.quotas]`, `[curation.ranking.weights.preliminary]`, `[curation.ranking.weights.utility]`,
   `[curation.ranking.diversity]`) with the plan's defaults and the validation rules in §19 (weights
   non-negative; `deep_keep ≥ shortlist_keep ≥ target_article_count`; `*_full > *_floor ≥ 0`;
   `0 ≤ cluster_threshold ≤ 1`; `per_cluster_cap ≥ 1`; batch sizes ≥ 1; dimension ∈ {256,512,1024,2048}).
   Steps 4–5 will *use* the ranking keys; you only add and validate them. Keep `prefilter_keep` for now.
   Startup logs whether Voyage is enabled. Update `config.example.toml` and the README.
3. **`src/curate/signals.rs`** (§9): a `Signals` type where every signal is `Option<f64>` (absent ≠ 0):
   `interest` (§9.1 z-scored interest match, top-three interests recorded; raw top-1 cosine fallback
   under 30 embedded articles, logged), `knn` (§9.2 preference state from `db::current_ratings(rating_lookback_days)`
   joined to `article_embeddings`, decayed weights `value × 0.5^(age/half_life)`, top-k positives and
   negatives, `knn = pos − negative_coefficient × neg`, top-three neighbours recorded, gate ramp
   `clamp((n − knn_floor)/(knn_full − knn_floor), 0, 1)` multiplying the weight, absent at 0),
   `feed` (§9.3 Beta-smoothed decayed rate credited to distinct direct `SourceKind::Feed` feeds, mean
   over the article's rated feeds, gate `feed_floor/feed_full`), `social` (existing composite; absent
   with no rows), `heuristic` (`longform_points(word_count)` − excerpt-only − roundup penalty, from
   `prefilter.rs` **without** the social, Scour/HN, multi-source terms; expose those pieces as
   functions). Log once per run:
   `preference: N rated articles with embeddings → knn gate X; feed gate Y (n=…)`.
   Also implement the **mid-rank percentile normalizer** of §12.2 and the **preliminary blend** of
   §12.4 (present-and-active weights renormalized) so `signals_json.norm`/`weights` can be written now.
4. **Pipeline**: after social enrichment, for every article that passes hygiene, compute embeddings
   (cached), signals, and the preliminary blend, then run the old prefilter as today. New stage
   timings `embed` and `signals`. `--skip-embeddings` uses cached embeddings only (zero Voyage calls);
   `--skip-llm` unchanged. `StageCounts` gains `eligible`, `embedded`, `rated_with_embeddings`, and
   the report/`print_report` show them. The paper must still build when Voyage is down or unconfigured
   (§17): `interest`/`knn` absent, never a penalty.
5. **`src/curate/telemetry.rs`** (§7.4, §7.5): the `candidate_runs` writer. One row per considered
   article per run, upserted with `INSERT … ON CONFLICT(run_id, article_id) DO UPDATE` setting every
   column. Hygiene-excluded articles get thin rows (`stage='excluded'`, `excluded_reason` ∈
   `blocked | published_before | recently_rejected`, `signals_json='{}'`). In this step map the old
   pipeline onto the stage vocabulary: passed hygiene → `eligible`; cut by the prefilter →
   `eligible` + `excluded_reason='not_admitted'`; prefilter survivors → `admitted`; Stage-A scored →
   `assessed`; sent to selection → `shortlisted`; picked → `selected` (+ `editor_why` if a `why`
   exists on the pick, else NULL); unpicked shortlist → `excluded_reason='not_selected'`. `signals_json`
   follows §7.5 exactly (`v: 1`, `raw`, `norm`, `present`, `weights` = effective preliminary weights,
   `top_interests`, `neighbours`, `exploration: false`, `auto_include`, `notes`). `utility` and
   `rank_utility` stay NULL until step 5.
6. **CLI** (§15.2, §16) in `src/main.rs`:
   `explain --date YYYY-MM-DD (--article ID | --url URL) [--run-id N]` and
   `explain --date YYYY-MM-DD --near-misses [N]` printing the persisted `candidate_runs` row for the
   latest non-dry run of that date (or `--run-id`): stage and reason; every raw/normalized signal with
   presence and effective weight; top interests with z; nearest rated neighbours; assessments from
   `article_assessments` when present (empty until step 4); admitted_by; the editor's `why`. `--url`
   canonicalizes (`dedupe`) and looks the article up; if absent from `articles`, print that it was never
   ingested. `--near-misses` lists the top N by preliminary blend (utility from step 5 when present)
   that were not selected. `features backfill [--days 30] [--rated-only] [--all] [--yes]` embeds rated
   and published articles first, then interests, then other recent articles only under `--all`, prints
   an estimate and asks for confirmation above 5M tokens unless `--yes`, and is idempotent (warm cache
   ⇒ zero calls). `features prune` removes `article_embeddings` rows for articles neither rated nor
   published older than `embedding_retention_days` and `candidate_runs` rows whose run started more
   than `telemetry_retention_days` ago. `features backfill` takes no lock yet (step 6 adds `lock.rs`).
7. Types (§18): `Signals`, `Signal` helpers, `RatedArticle` already exists — extend if needed. Do not
   create `Candidate` yet (step 4/5).

Tests (§20 "Embeddings", "Interest z-scores", "Preference", "Normalization", plus telemetry/explain):
BLOB round trip; wrong length and non-finite rejected; cache hit on same hash, miss on changed
text/model/dimension; response mapped by index and length-checked; a failed batch does not abort the
others; embedded text contains no feed name or author; a broad interest with uniformly high cosine does
not dominate while a specific interest with one strong match does; raw fallback under 30 articles; one
loved article gives a positive `knn` to a near neighbour; two unrelated loved clusters both score high
(anti-centroid); `good` moves the signal 0.35× as much as `loved`; decay halves at the half-life; gate
0 below `knn_floor`, 1 at `knn_full`, linear between; feed credit sums to 1 across direct feeds; feed
affinity uses the mean; constant signal → 0.5 for everyone; ties get equal percentiles (400 identical
zeros → all 0.5, no id ramp); absent values do not shift others; effective weights sum to 1; a candidate
missing a signal is scored on the rest; `candidate_runs` rows written for every considered article with
the right stage/reason on a mocked run; `--skip-embeddings` makes zero Voyage calls; a Voyage failure
still publishes; `explain` renders from persisted rows and reports "never ingested"; `features prune`
respects rated/published.
