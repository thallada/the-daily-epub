
---

# YOUR STEP: 4 — Triage replaces the gate (plan §21 step 4)

Steps 1–3 have landed: three-way votes and `rating_events`; the Claude editor (`Llms { bulk, editor }`,
`AnthropicBackend`, per-provider `UsageMeter`s, `why` lines, the Brief); Voyage embeddings,
`signals.rs` (interest/knn/feed/social/heuristic, percentile normalizer, preliminary blend),
`telemetry.rs` (`candidate_runs` writer), `explain`, `features backfill|prune`, and the full
`[curation.ranking]` config. Read `git log -6`, then the current `src/curate/` (all files),
`src/pipeline.rs`, `src/config.rs`, `src/db.rs`, `src/report.rs`, `src/main.rs`, `src/types.rs`.

Scope (plan §7.3 reuse and churn rules, §8.1 hygiene, §10 triage, §11 admission and exploration,
§12.4 preliminary blend already exists, §15.4 log line parts, §17 DeepSeek row, §19 `deep_keep`):

1. **Migration `migrations/0003_drop_scores.sql`**: `DROP TABLE scores;` (deferred from step 1).
   Remove `db::upsert_score`, `recently_low_scored_ids` and every `scores` reader. The churn rule
   now reads `article_assessments`: an article whose latest `triage` `score < recent_rejection_floor`
   (3.0) or `deep` `score < recent_rejection_floor` within `recent_rejection_days` (7) is
   `recently_rejected`, unless auto-include (§7.3, §8.1).
2. **`src/curate/triage.rs`** (§10): `TRIAGE_INSTRUCTIONS` and `TRIAGE_PROMPT_VERSION = 1` verbatim
   from the plan; per-article block exactly as §10 (id, title, feed (category), author, length ·
   excerpt only, opening = first 200 words via `prompt_text` + `truncate_words`, `matches interests:`
   from `top_interests` with z ≥ 1.5 labelled strong (z ≥ 2.5) / weak, `closest rated:` from
   neighbours with cosine ≥ 0.55; omit lines with nothing). Batches of `deepseek.triage_batch_size`
   (25) through `buffer_unordered(max_concurrent_requests)` on the **bulk** client, budget check
   before each batch. Tolerant parsing generalized from `score.rs::parse_score_response`: a
   malformed item never sinks the batch, ids not in the batch are dropped, unknown `kind` → `other`,
   `interest` clamped 0–10. Persist every result to `article_assessments (stage='triage', model,
   prompt_version, profile_version, score, kind, rationale=why, assessed_at)`.
   **Cache** (§7.3): skip articles with a reusable `triage` or `deep` assessment (same `model` and
   `prompt_version`, `assessed_at` within `assessment_reuse_days`); `generate --rescore` ignores the
   cache. A failed batch leaves `triage` absent for its articles.
   **Pool cap** (§10): if eligible > `triage_max` (800), triage the union of top `0.7 × triage_max`
   by preliminary blend, top 100 by `interest`, top 100 by `knn` (if active), all auto-includes,
   filled to `triage_max` by blend; the rest get `stage='eligible'`, `excluded_reason='not_admitted'`.
3. **`src/curate/admit.rs`** (§8.1, §11): hygiene moves here (`blocked`, `published_before` = in
   `issue_articles` for any issue date before this run's date, `recently_rejected` per item 1;
   auto-includes never excluded) writing the thin `candidate_runs` rows, and the union admission
   into `deep_keep` (120) slots in this order, each retriever taking its top-N by its own signal among
   not-yet-admitted, not-excluded articles, recording every retriever that would have taken an
   article in `admitted_by` (first = the one that admitted it):
   `auto_include` (uncapped) → `triage` (quota 60, floor `interest ≥ 5`) → `interest` (20; floors
   `word_count ≥ semantic_min_words`, not `looks_like_roundup`, triage `interest ≥ 3` if triaged)
   → `knn` (20, gate > 0, same floors) → `exploration` (5, §11.1) → `blend` (remaining).
   Inactive retrievers release their quota to `blend`. Not admitted → `stage='triaged'` (or
   `'eligible'` if never triaged) + `excluded_reason='not_admitted'`.
   **Exploration** (§11.1): five slots for articles ranked between `deep_keep` and `deep_keep × 2.5`
   by the preliminary blend with `word_count ≥ 300`, not roundups, triage `interest ≥ 4`; ordered by
   `sha256(run_date || article_id)`; flagged `exploration = true` through to the editor prompt.
4. **Prefilter reduced** (§18): `prefilter.rs` keeps only hygiene helpers (`is_blocked`,
   `is_auto_include`, `looks_like_roundup`) and the text heuristic pieces `signals.rs` uses. Delete
   `prefilter::run`, `score_article`'s social/Scour/HN/multi-source terms, `PrefilterContext`, the
   `prefilter_keep` config key (replace uses with `curation.ranking.deep_keep`; the config validation
   `deep_keep ≥ shortlist_keep ≥ target_article_count` already exists), and `Curator::prefilter`.
   A stale `prefilter_keep` key in a config file must fail loudly with a message naming
   `curation.ranking.deep_keep` (follow the `bookorbit_dir` precedent in `config.rs`).
5. **Pipeline**: hygiene → embeddings → signals → preliminary blend → triage → admission → the
   *existing* Stage A scoring (`score.rs`, unchanged this step) over the admitted deep set → the
   editor → editorial, as today. The deep set (`admitted`) is what Stage A scores and what the editor
   sees; step 5 replaces Stage A with deep assessment and adds the shortlist. Telemetry stages now:
   `excluded | eligible | triaged | admitted | assessed | shortlisted | selected` with reasons per §7.4;
   `admitted_by` and `exploration` written; `signals_json.raw/norm/present` gain `triage` (÷10 for norm).
   Stage timings `triage` and `admit`; `StageCounts` gains `triaged`, `admitted`, `admitted_by` (map),
   `exploration_admitted`, `exploration_selected`. Log the admission line from §15.4:
   `admission: triage 60 · interest 20 · knn 12 · exploration 5 · blend 23 · auto 0`.
   `--skip-llm` skips triage; DeepSeek down or tripped ⇒ no triage, admission by
   `interest`/`knn`/`blend`, editor still runs (§17). The editor prompt (§13 rendering) shows the
   triage score and `flags: exploration` where present. `explain` prints the triage assessment
   (score, kind, why) and `admitted_by`.
6. `types.rs` (§18): introduce `Assessment { triage: Option<Triage>, deep: Option<Deep> }` and
   `Triage { interest, kind, why, model, prompt_version, assessed_at }` (keep `Deep`/`LlmScore`
   compatible with the existing Stage A output for now), and `Candidate { article, auto_include,
   exploration, signals, assessment, utility: Option<f64>, cluster: Option<...>, admitted_by: Vec<String>,
   stage, excluded_reason }` replacing `ScoredArticle` where the pipeline flows through admission.
   Keep the change mechanical where `score.rs`/`select.rs` still consume the old shape (step 5
   retires them); an adapter is acceptable.
7. `runs.config_json` gains `TRIAGE_PROMPT_VERSION`; `config.example.toml` and README updated
   (`prefilter_keep` removed, `[deepseek].triage_batch_size`, `--rescore`).

Tests (§20 "Triage and deep parsing" triage half, "Admission", churn/cache): realistic triage
fixture parsed; malformed items do not sink a batch; unknown kind → other; every `kind` token in the
prompt round-trips; cached triage reused within `assessment_reuse_days` and ignored with `--rescore`;
churn rule excludes a recent low triage score and spares auto-includes; a strong-interest,
weak-heuristic, no-social article reaches the deep set; a 60-word stub with high interest similarity
is not admitted by `interest`/`knn`; quotas honoured; inactive retrievers release quota; exploration
deterministic per date and rotating across dates; auto-includes always admitted; excluded articles get
thin rows with the right reason; pool cap path yields `not_admitted` rows; the mocked full pipeline
still publishes with DeepSeek failing; migration 0003 drops `scores`.
