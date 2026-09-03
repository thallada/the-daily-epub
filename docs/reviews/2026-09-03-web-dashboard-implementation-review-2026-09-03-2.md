# Web dashboard step 6 implementation review

The finished step-6 implementation matches the jobs and stats brief. The fixed
job catalogue, CLI lifecycle, systemd runner and files, dashboard routes,
stats-data refactor, SVG sparklines, production runner wiring, and required
tests are all present. The review found and corrected two faulty test
assertions, restored the polkit file to the brief's verbatim rule, made job
messages reliably one-line, and strengthened the duplicate-start test to use a
genuinely running row. No outstanding correctness finding remains.

## Critical

None.

## High

None.

## Medium

None.

## Low

None.

## Nits

None.

## Plan Coverage

| Requirement | Status | Evidence |
|---|---|---|
| Fixed job catalogue and validated names | Implemented as planned | `src/jobs.rs`: `Job`, `parse`, `name`, `unit`, `description`, `takes_lock`, `dangerous` and traversal/uppercase/unknown-name tests. |
| `daily-epub job run <name>` lifecycle | Implemented as planned | `src/main.rs`: CLI dispatch, existing lock path, in-process command mapping, requested-row claim, terminal update, failure propagation and tests. `src/jobs.rs` owns row lifecycle helpers. |
| Unit template, polkit rule and journal group | Implemented as planned | `systemd/daily-epub-job@.service`, verbatim `systemd/50-daily-epub.rules`, and `SupplementaryGroups=systemd-journal` in `systemd/daily-epub.service`; repository-content tests cover their security-sensitive strings. |
| Production/test/disabled runners | Implemented as planned | `src/jobs.rs::SystemdRunner` uses bounded `tokio::process::Command`; `src/web/mod.rs` contains scripted `MockRunner` and `DisabledRunner`; `src/server.rs::serve` selects the production runner only when jobs are enabled while `AppState::new` remains test-compatible. |
| Jobs list/start/detail pages | Implemented as planned | `src/web/dashboard/jobs.rs` and the two jobs templates implement catalogue cards, date selection, history, duplicate refusal, failed-start persistence, live status, journal tail, the 30-second failure rule, authorization tests and five-second refresh. |
| Config reload integration boundary | Partially implemented by design | The required `// TODO(step 5 merge): reload_if_changed` is at the start site. Step 5 owns the helper and merge-time call per the brief's parallel-work constraint. |
| Stats data/text split | Implemented as planned | `src/curate/telemetry.rs` exposes `StatsData`, `stats_data`, and `render_stats_text`; seeded fixtures pin the complete legacy output byte-for-byte. |
| Stats page | Implemented as planned | `src/web/dashboard/stats.rs` and `dashboard/stats.html` provide the three allowed windows, tables, retriever yield, per-run data and the three requested charts. |
| Overview sparklines | Implemented as planned | `src/web/dashboard/mod.rs` loads the last 30 finished non-dry runs and renders cost, selected count and duration through `_sparkline.html`. |
| CSP-safe rendering | Implemented as planned | SVG geometry uses presentation attributes and stylesheet classes; changed templates contain no inline `style` attributes and no `safe` filter. |

## Testing Assessment

Meaningful tests cover every catalogue form and rejection case, job-row claim
and completion (including run-id persistence), real router authorization and
POST behavior with `MockRunner`, requested/running duplicate refusal, failed
starts, the unit-exited grace rule, disabled jobs, systemd output parsing,
polkit/unit contents, in-process job success and failure, exact
legacy stats text, stats aggregation, SVG geometry, page output, and overview
series.

The tests deliberately do not execute systemd or the network. The mapped
generate branch is therefore covered by its option construction and the
separate job run-id lifecycle test rather than a live pipeline invocation;
this follows the brief's offline-test rule. No additional step-6 test is needed.

## Open Questions

- During the step-5 merge, replace the required job-start TODO with the single
  `reload_if_changed` call supplied by step 5; do not duplicate that helper.
