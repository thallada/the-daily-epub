# Step 6 handoff — jobs and stats

## Landed

- Added `src/jobs.rs`: the fixed, regex-constrained job catalogue; jobs-table
  query/lifecycle helpers; single-line terminal messages; `SystemdRunner` with
  ten-second command timeouts, captured stderr and systemd status parsing.
- Added `daily-epub job run <name>`. It uses the existing command lock policy,
  claims or creates the requested row, calls the same in-process helpers as the
  ordinary CLI commands, records `ok`/`failed`, stores the generated run id,
  and propagates failures to a non-zero process exit. Existing profile,
  features and social CLI output retains its prior line content.
- Finalized `JobRunner`: production `serve` installs `SystemdRunner` when
  `server.jobs_enabled` is true; disabled servers use `DisabledRunner`;
  `AppState::new` keeps its existing test behavior; `MockRunner` records calls
  and scripts starts, statuses and logs.
- Added the admin Jobs list/start/detail routes and templates: catalogue cards,
  dated generation, job history, duplicate conflict handling, start-failure
  persistence, live unit fields, configured journal tail, 30-second
  pre-claim failure detection, disabled-state messaging and five-second active
  refresh.
- Added the systemd job template, the §14.3 polkit rule verbatim, and
  `SupplementaryGroups=systemd-journal` on the server unit.
- Split stats into `stats_data` plus `render_stats_text` while pinning the
  complete previous CLI output. Added the 14/30/90-day stats page, aggregate
  tables, retriever yield, per-run table, daily provider costs and the three
  requested SVG charts.
- Replaced the overview placeholder with cost-per-run, selected-per-run and
  generation-time sparklines over the last 30 finished non-dry runs. SVGs use
  presentation attributes and CSS classes only; no inline styles or template
  `safe` filters were added.
- Added/expanded tests for the catalogue, job lifecycle and run link, CLI lock
  mapping, in-process job success/failure, polkit/unit files, runner status
  parsing, every jobs route behavior, exact stats text/data, stats routes and
  both sparkline forms.

## Deviations and integration notes

- Per the parallel-step constraint, job start contains exactly the requested
  `// TODO(step 5 merge): reload_if_changed` marker and does not duplicate step
  5's helper. The step-5 merge must replace that marker with its helper call.
- The offline in-process job test uses `features-prune`; generate's `run_id`
  persistence is tested separately through the same lifecycle helper because a
  full generate would require Miniflux/network fixtures. Production generate
  and dry-run mappings both store `GenerateOutcome.run_id`.
- The server unit intentionally does not add `/etc/daily-epub` to
  `ReadWritePaths` here; step 5 owns that additive unit change.
- `StatsData.durations` retains finished dry runs because that is what the old
  CLI calculation included and the text must remain byte-identical. Dashboard
  per-run series and overview sparklines exclude dry runs as specified.

## Left for later

- Step-5 merge: call its `reload_if_changed` implementation at the marked job
  start site and combine its `/etc/daily-epub` unit path change.
- Step 7 owns user-facing documentation and deployment/runbook polish.

## Verification

- `cargo fmt`: pass.
- `cargo clippy --all-targets -- -D warnings`: pass.
- Raw `cargo test`: **406 passed, 13 failed** in the library before Cargo
  stopped; every failure was a sandbox-denied loopback listener (four named
  Anthropic tests, three pre-existing OpenAI fake-server tests documented in
  the step-1 handoff, the named extractor test and five named server tests).
- `cargo test` with those listener tests and both `tests/m7_server.rs` listener
  tests skipped: **440 passed, 0 failed, 15 filtered out** across all targets.
- Focused `cargo test jobs::`, `cargo test job_run`, and `cargo test stats`:
  pass.
- `node --check` on the polkit JavaScript (via stdin): pass.
- `systemd-analyze verify systemd/daily-epub-job@.service`: no diagnostics for
  the job unit.

