# Personalized Curation v2 — implementation progress and handoff

**Updated:** 2026-09-02 (end of session 2)
**Plan:** `docs/plans/2026-09-02-personalized-curation-v2.md` (§21 is the step sequence)
**Branch:** `curation-v2` (branched from `main` at `a599745`; not merged, not pushed)
**Briefs:** `docs/plans/curation-v2-briefs/` — `00-preamble.md` + one `stepN.md` per step. Each
brief was handed to an implementation agent as `cat 00-preamble.md stepN.md`.

## Where things stand

| Step (§21) | Status | Commit |
|---|---|---|
| 1. Feedback and profile | **done**, reviewed (codex review: no actionable issues) | `3a9f4b9` |
| 2. Claude editor and editorial | **done** | `57efbb4` |
| 3. Embeddings, signals, telemetry | **done** (branch `curation-v2-step3`, merged) | `ea3b141` + merge commit |
| 4. Triage replaces the gate | **done** (Codex plugin, high effort), reviewed | `10f4afd` |
| 5. Deep assessment, utility, diversity | **done** (Codex started, cut off by quota; Claude agent finished), reviewed | `05a74a0` |
| 6. Paper telemetry, stats, lock | **done** (Claude agent), reviewed | `d261cd4` |
| 7. Cleanup + implementation notes | **done** (Claude agent), reviewed | `d403c51` |

**Post-plan addition (same day):** a provider-agnostic LLM registry — `[llm]` roles (`bulk`,
`editor`) over named `[providers.*]` entries of `kind = openai | anthropic`, keys from
`DAILY_EPUB_PROVIDERS__<NAME>__API_KEY`, a `gemini` entry (Gemini 3.8 Flash via Google's
OpenAI-compatible endpoint) declared but unreferenced, and a `daily-epub config check` subcommand.
The old `[deepseek]`/`[anthropic]` tables, top-level `max_daily_usd` and the old key env vars
fail loudly. The server upgrade is written up in `docs/runbooks/curation-v2-migration.md`.

All seven plan steps are implemented. `cargo fmt --check`, `cargo clippy --all-targets`
(including `-W dead_code`) and `cargo test` (329 lib tests + 6 bin tests + the 7 integration suites) are green at HEAD.

Nothing has been run against the production database or the real providers. No local
`config.toml`, database, or API keys exist on the dev box, so verification so far is tests only.

## Decisions and deviations made while implementing

Steps 1–3 (session 1):

- **Legacy `up` links.** `Vote::parse("up")` → `Loved`, and `auth::verify_token` also accepts tokens
  signed over the literal `up` segment so already-published issues keep working. Keep both.
- **Same-date regeneration** no longer excludes its own picks (`published_before` uses issue dates
  strictly before the run date), per plan §8.1.
- **`VoyageConfig.api_key`** exists as a field (figment maps `DAILY_EPUB_VOYAGE__API_KEY` into it;
  `deny_unknown_fields` would otherwise reject the env var). Never document it in TOML.
- **`anthropic.max_concurrent_requests`** is validated but not consumed; summary concurrency is
  the constant `SUMMARY_CONCURRENCY = 4` in `editorial.rs`.
- `RATINGS_LOOKBACK_DAYS` in `profile/mod.rs` is effectively unbounded (36,500) for the prompt
  verdict block and the weekly rebuild; the knn/feed preference state uses
  `curation.ranking.rating_lookback_days` (180) as the plan says.
- Footer CSS: `.rating` has no `white-space: nowrap` (it would clip on narrow e-ink screens).

Step 4:

- Hygiene order is blocked → published_before → recently_rejected, all skipped for auto-includes.
- `admitted_by` records every retriever whose own top-N would have taken the article (first entry
  is the admitting one); exploration is flagged only when `exploration` admitted it.
- Under the pool cap (`triage_max`) the surplus gets `stage='eligible'`, `excluded_reason='not_admitted'`.
- Migration `0003_drop_scores.sql` drops `scores`; the churn rule reads `article_assessments`.

Step 5:

- `Candidate` is the only flow type; `ScoredArticle`, `combined_score`, `score.rs`, `select.rs` are gone.
- The editor omits the `facets:` line when every facet is unknown (plan renders it unconditionally).
- Articles cut at the shortlist keep the stage they reached (`admitted` if never deep-assessed,
  else `assessed`) with `excluded_reason` `cluster_suppressed` (hit the base cap) or `shortlist_cap`.
- `Curator::assess` is a no-op under `--skip-llm`; cached deep rows are still reused when DeepSeek
  is merely down.
- `--near-misses` and the Behind-the-paper list order by utility, falling back to the preliminary
  blend per row.
- Fixtures: `tests/fixtures/deepseek_deep_batch{,_messy}.json` replace the old score fixtures;
  `deepseek_triage_batch.json` added in step 4.

Step 6:

- `BehindThePaper` hangs off `Issue.behind` (filled after `build_issue`), not a `build_issue` argument.
  Generation time and cost shown in the chapter are measured at issue assembly (they exclude the
  EPUB build and publish, a few seconds); the cost includes Voyage.
- Voyage spend is recorded under `provider_costs["voyage"]`; `finish()` counts it once and keeps its
  tokens out of the LLM aggregate. Voyage spend is not preloaded into its meter for the UTC day.
- `src/lock.rs` uses `libc::flock` (`libc` was already in `Cargo.lock`; now a direct dependency).
  The holder's command name is written into the lock file for the "X is already running" message.
- The mid-run `admission:` line is `debug`; the four-line §15.4 block is logged once after
  `finish()` and printed by `print_report`. Timings `summaries` and `brief` replace `editorial`.

Step 7:

- `features prune` sweeps `article_embeddings`, `candidate_runs` and now `article_assessments`;
  `generate` runs it once after a published (non-dry) issue, best effort.
- The Brief chapter's TOC/title string is now "The Brief" (was still "From the Editor").
- Kept on purpose: `UsageMeter::new(&DeepseekConfig, ..)` (a convenience over `with_prices`, 12
  call sites); `StageCounts.candidates` (feeds `runs.candidates` and the colophon); the
  `prefilter_keep`/`score_batch_size` removed-key startup errors (operator guards); the SQL comment
  "filled in by step 4/5" inside the applied migration `0002` (sqlx checksums); six v1-era
  unreferenced `pub` items (`enrich_one`, `issues_before`, `today_in_tz`, `to_json_pretty`,
  `with_retry_policy`, `db::issues_before`) as pre-v2 API surface.
- `config::tests::shipped_example_config_matches_the_defaults_key_for_key` compares the example
  against `Config::default()` in both directions (numbers within 1e-6).

## Operator to-dos before the first real run

1. Set `DAILY_EPUB_ANTHROPIC__API_KEY` and `DAILY_EPUB_VOYAGE__API_KEY` in the systemd env file.
2. Set hard spend limits in the DeepSeek, Anthropic and Voyage dashboards (the meters are runaway
   guards, not accounting).
3. Copy `data/profile.md` to wherever `profile_path` points on the server (default is relative to
   `WorkingDirectory=/var/lib/daily-epub`, like `data/scour-interests.opml`).
4. Back up the DB, then `daily-epub db migrate` (0002 drops `ratings`/`feed_priors`, 0003 drops `scores`).
5. `daily-epub features backfill --rated-only` then `--days 30` to warm the embedding cache.
6. `daily-epub generate --dry-run`, read the paper (including the new Behind-the-paper chapter),
   then `explain --date … --near-misses` and `stats`.
7. Watch the first few real runs' `providers:` log line against the ~$0.80/day estimate (plan §3).

## How the work was run (so the next session can repeat it)

- Orchestrator: Claude Code (this repo), one implementation agent per step, review + commit by the
  orchestrator after independent `cargo fmt/clippy/test`.
- **Codex via the `openai-codex` Claude Code plugin (v1.0.6) works on this host as of session 2**
  (the operator fixed bubblewrap). Launch:
  `node ~/.claude/plugins/cache/openai-codex/codex/1.0.6/scripts/codex-companion.mjs task --background --write --effort high "$(cat brief.md)"`,
  poll `… status <job-id> --json` (`.job.status`), read `… result <job-id>`. Inside its sandbox
  `bind()` on 127.0.0.1 is forbidden, so exactly ten pre-existing loopback tests fail there
  (`curate::llm::tests::anthropic_*` ×4, `extract::tests::relative_urls_resolve_against_the_url_we_landed_on`,
  `server::tests::*` ×5, plus the two `m7_server` integration tests); the brief must tell the agent
  to ignore those and the orchestrator runs the full suite outside. The old "host quirk" paragraph
  in `00-preamble.md` is obsolete.
- Codex usage budget is roughly one large step per ChatGPT usage window: in session 2 it finished
  step 4 (~26 min) and was cut off 21 minutes into step 5 ("try again at 7:44 PM"). The job status
  becomes `failed` with the partial work left in the tree; a Claude `general-purpose` agent with a
  "finish, don't restart" brief (known failures, scope audit checklist) completed it. Steps 6 and 7
  ran on Claude agents directly.
- Steps 4 → 5 → 6 → 7 were sequential, each reviewed against the plan sections named in its brief
  before committing.

## Next session: what remains

1. `git checkout curation-v2 && cargo test` (expect green). Follow
   `docs/runbooks/curation-v2-migration.md` on the server.
2. Optional: a Codex `review --background --scope branch --base main` pass over the whole branch.
3. `git merge --no-ff curation-v2` into `main`, build, deploy, and do the operator to-dos above.
4. After a week of real runs: read `stats`, tune `[curation.ranking]` from what `explain` shows,
   and revisit the deferred items in plan §23.
