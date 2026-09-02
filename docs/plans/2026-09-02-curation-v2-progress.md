# Personalized Curation v2 — implementation progress and handoff

**Updated:** 2026-09-02 (end of session 1)
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
| 4. Triage replaces the gate | not started — brief ready: `step4.md` | |
| 5. Deep assessment, utility, diversity | not started — brief ready: `step5.md` | |
| 6. Paper telemetry, stats, lock | not started — brief ready: `step6.md` | |
| 7. Cleanup + implementation notes | not started — brief ready: `step7.md` | |

`cargo fmt --check`, `cargo clippy --all-targets` (only the three pre-existing `src/world.rs`
`needless_borrow` warnings) and `cargo test` (294 lib tests + 6 integration suites) are green at HEAD.

Nothing has been run against the production database or the real providers. No local
`config.toml`, database, or API keys exist on the dev box, so verification so far is tests only.

## Decisions and deviations made while implementing (read before step 4)

- **`scores` table not yet dropped.** Migration `0002_curation_v2.sql` created every new table, copied
  `ratings` → `rating_events`, and dropped `ratings` and `feed_priors`, but kept `scores` because the
  old Stage A scoring (`src/curate/score.rs`, `db::upsert_score`, `recently_low_scored_ids`) still
  uses it. Step 4 adds `migrations/0003_drop_scores.sql` and moves the churn rule to
  `article_assessments` (already in `step4.md`).
- **Old prefilter still gates.** Hygiene → embeddings → signals → preliminary blend now run for every
  article, and `candidate_runs` rows are written with the stage vocabulary mapped onto the old flow
  (`admitted_by` is `["prefilter"]`/`["auto"]` for now), but `prefilter::run` + `prefilter_keep`
  still decide the deep set until step 4.
- **Legacy `up` links.** `Vote::parse("up")` → `Loved`, and `auth::verify_token` also accepts tokens
  signed over the literal `up` segment so already-published issues keep working.
- **Same-date regeneration** no longer excludes its own picks (`published_before` uses issue dates
  strictly before the run date), per plan §8.1.
- **`VoyageConfig.api_key`** exists as a field (figment maps `DAILY_EPUB_VOYAGE__API_KEY` into it;
  `deny_unknown_fields` would otherwise reject the env var). Never document it in TOML.
- **`anthropic.max_concurrent_requests`** is validated but not consumed yet; summary concurrency is
  the constant `SUMMARY_CONCURRENCY = 4` in `editorial.rs`.
- `UsageMeter::new(&DeepseekConfig, ..)` survives as a compat constructor over `with_prices`.
- `tests/fixtures/deepseek_front_page.json` was replaced by `tests/fixtures/claude_brief.json`.
- `RATINGS_LOOKBACK_DAYS` in `profile/mod.rs` is effectively unbounded (36,500) for the prompt
  verdict block and the weekly rebuild; the knn/feed preference state uses
  `curation.ranking.rating_lookback_days` (180) as the plan says.
- Footer CSS: `.rating` has no `white-space: nowrap` (it would clip on narrow e-ink screens).

## Operator to-dos before the first real run

1. Set `DAILY_EPUB_ANTHROPIC__API_KEY` and `DAILY_EPUB_VOYAGE__API_KEY` in the systemd env file.
2. Set hard spend limits in the DeepSeek, Anthropic and Voyage dashboards (the meters are runaway
   guards, not accounting).
3. Copy `data/profile.md` to wherever `profile_path` points on the server (default is relative to
   `WorkingDirectory=/var/lib/daily-epub`, like `data/scour-interests.opml`).
4. Run `daily-epub db migrate` (0002 drops `ratings`/`feed_priors`; back up the DB first).
5. `daily-epub features backfill --rated-only` then `--days 30` to warm the embedding cache.
6. A `generate --dry-run` and read the paper; `explain --near-misses` once step 4 lands.

## How the work was run (so the next session can repeat it)

- Orchestrator: Claude Code (this repo), one implementation agent per step, review + commit by the
  orchestrator after independent `cargo fmt/clippy/test`.
- Codex: `codex exec -C <repo> --sandbox workspace-write --add-dir ~/.cargo -c
  sandbox_workspace_write.network_access=true -c model_reasoning_effort=high -o <last.md> - < <brief>`,
  detached with `setsid nohup`, exit code written to a file and watched with a monitor. The
  `openai-codex` Claude Code plugin's `task` runs fail on this host (bubblewrap cannot create user
  namespaces: `kernel.apparmor_restrict_unprivileged_userns = 1`); the CLI works if the brief tells
  the agent to edit files via shell commands instead of the `apply_patch` tool (see the preamble).
  The plugin's read-only `review --background --scope working-tree` does work and was used on step 1.
  Fix for the sandbox: `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`.
- Codex ran out of ChatGPT usage after ~560k tokens (step 1 complete, steps 2 and 3 cut off
  mid-way); Claude `general-purpose` sub-agents finished steps 2 and 3 from the partial trees and
  resolved the step 2/3 merge. Budget roughly one large step per Codex usage window.
- Steps 2 and 3 were run in parallel (step 3 in a git worktree); the merge cost ~19 conflict hunks
  in config/pipeline/report/main/README/config.example. Steps 4 → 5 → 6 → 7 are sequential.

## Next session: exact starting instructions

1. `git checkout curation-v2 && cargo test` (expect green).
2. Read this file, then `docs/plans/curation-v2-briefs/00-preamble.md` and `step4.md`.
3. Launch the step 4 agent with `cat 00-preamble.md step4.md` as the prompt (Codex CLI as above, or a
   Claude general-purpose agent — tell it to ignore the "host quirk" paragraph in that case).
4. Review the diff against plan §10–§11, run the checks, commit as "Curation v2 step 4: …".
5. Repeat for steps 5, 6, 7. After step 7: `git merge --no-ff curation-v2` into `main`, deploy, and
   do the operator to-dos above.
