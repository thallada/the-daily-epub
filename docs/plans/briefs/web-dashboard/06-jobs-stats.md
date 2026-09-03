# Step 6 — Jobs and stats

Read `00-shared.md`, the plan (§2 host facts, §3, §4.2, §9.1 sparklines, §12,
§14 in full, §15 `jobs_enabled`/`journal_lines`, §16, §17 "Stats refactor" and
"Jobs"), `src/main.rs` (`lock_holder`, `cmd_*` helpers, `GenerateOptions`),
`src/curate/telemetry.rs` (`stats`), `systemd/`, and the handoffs for steps
1–5. This is plan §19 item 6.

## Deliverables

1. `src/jobs.rs` (§14.1): the `Job` enum, `parse` (regex `^[a-z0-9-]+$` and
   the listed forms only), `name`, `unit`, `description`, `takes_lock`,
   `dangerous`. Register in `lib.rs`.
2. `daily-epub job run <name>` (§14.2) in `main.rs`: parse (unknown → exit 2
   with the catalogue), take the lock via `lock_holder` when `takes_lock()`,
   find-or-insert the `requested` jobs row → `running`, run the mapped command
   in-process with the same functions `main` uses, set `ok`/`failed`,
   `finished_at`, `run_id` (generate, from `GenerateOutcome.run_id`), a one-line
   `message`; non-zero exit on failure. The job lifecycle helpers live in
   `src/jobs.rs` as db functions or on `Db`.
3. `systemd/daily-epub-job@.service` (copy of the generate unit with the `%i`
   description and `job run %i`), `systemd/50-daily-epub.rules` exactly as
   §14.3, `SupplementaryGroups=systemd-journal` on `daily-epub.service`.
4. `JobRunner` (§14.4): finalize the trait from step 1's stub; `SystemdRunner`
   with `tokio::process::Command`, 10 s timeout, stderr in the error;
   `MockRunner` with recorded calls and scripted statuses; `DisabledRunner`
   when `jobs_enabled = false`. `serve` picks the runner from config.
5. `src/web/dashboard/jobs.rs` + `dashboard/jobs.html` + `dashboard/job.html`:
   the catalogue cards (Start with `data-confirm` for `dangerous()`, a date
   input on the `generate-<date>` card), the jobs table, `POST
   /dashboard/jobs/{name}` (409 + flash for a duplicate `requested`/`running`
   unit; insert; `runner.start`; failed start marks the row), `GET
   /dashboard/jobs/{id}` (row, live `UnitStatus`, journal tail of
   `server.journal_lines` in `<pre>`, the 30-second "unit exited before the job
   started" rule). `reload_if_changed` on every job start (§4.2). `app.js`
   refreshes the job page every 5 s while `requested`/`running`.
6. Stats (§12): refactor `telemetry::stats` into `stats_data -> StatsData` +
   `render_stats_text(&StatsData) -> String` with byte-identical CLI output (a
   test pins it against a seeded DB before and after); `GET /dashboard/stats`
   with `?days=14|30|90`, the tables, three server-rendered SVG sparklines via
   `_sparkline.html`, the retriever yield table.
7. Overview sparklines (§9.1): cost per run, selected per run, generation
   seconds over the last 30 non-dry runs, using the same partial.
8. Tests (§17 "Stats refactor", "Jobs"): `Job::parse` accepts the catalogue and
   the dated form, rejects `../x`, uppercase and unknown names; `POST
   /dashboard/jobs/{name}` inserts a row and calls `MockRunner::start` with the
   right unit; a running duplicate is refused; a failed start marks the row;
   `job run` in-process (a `--skip-llm`-style config with mocked providers, e.g.
   `features-prune` or `profile-rebuild` against a temp DB) flips
   `requested → running → ok`; the polkit rule file is present and its regex
   matches the unit name format (string test); stats text unchanged.
