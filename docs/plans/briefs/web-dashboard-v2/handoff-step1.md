# Web dashboard v2 — step 1 handoff

## What landed

- Public issue pages and Atom entries now show each pick's trimmed summary and
  its “Why it's here” line. The public view model still has no article body,
  Brief, World Briefing, or discussion/comment content, and the public tests
  assert those fields do not leak.
- Legacy `issues.issue_json IS NULL` loads now recover their colophon and
  Behind the paper data from `issues.report_json`, matching the report to its
  run by `started_at` for near misses. If the issue report is absent, the loader
  uses the latest finished run for that issue date. Missing historical values
  render as `n/a` in the web colophon rather than misleading zeroes.
- Historical model names come from the resolved run config. Summary-provider
  resolution follows `editorial.summary_model`; missing model metadata falls
  back to the current config and is called out in the generator note.
- Legacy World Briefings are recovered from `OEBPS/world.xhtml` in the stored
  standard EPUB (or its canonical publish path). The EPUB body is used after
  removing its own leading heading; missing or corrupt files are debug-logged
  and treated as “no World chapter,” not as request errors.
- The development seed's 2026-09-01 issue now has a real report, a real EPUB
  path with fixture World Briefing content, `issue_json = NULL`, and three
  `candidate_runs` near misses. The 2026-09-02 issue retains a full snapshot.
- README and the dashboard plan now document the operator's 2026-09-03 public
  summary/why decision and list recovered EPUB XHTML among the trusted `|safe`
  inputs. `zip` moved from dev-only to runtime dependencies.

## Tests

- Focused web issue tests: **10 passed**, including report and run-only
  colophon fallback, recovered World/Behind routes, missing EPUB behavior, and
  public/feed privacy assertions.
- Public view-model test: **1 passed**.
- Development seed smoke test: successfully generated the database and EPUB;
  SQL verification showed the legacy row has a null snapshot, present report
  and EPUB path, plus **3** near-miss rows.
- `cargo fmt`: clean.
- `cargo clippy --all-targets -- -D warnings`: clean.
- Unfiltered `cargo test`: library result **427 passed, 13 failed**. Every
  failure was a pre-existing loopback-listener test rejected by the sandbox.
- Filtered sandbox run: **461 passed, 0 failed, 15 filtered** across library,
  binary, integration, and doc-test targets. The filters were the 13 loopback
  unit tests and both `tests/m7_server.rs` TCP tests.

## Deviations and follow-up

- `00-shared.md` lists four Anthropic listener tests, one extraction listener
  test, five server listener tests, and `m7_server` as sandbox-only failures.
  The code also has three pre-existing OpenAI mock-server tests that bind the
  same forbidden loopback listener; they failed with the identical
  `PermissionDenied` error. `docs/plans/2026-08-15-implementation-notes.md`
  already lists those three. They were therefore filtered for the complete
  sandbox verification and should run normally in the orchestrator.
- No migrations, routes, auth behavior, or security headers changed. No
  database backfill is performed.
- This sandbox mounts the worktree's shared Git metadata under
  `/home/thallada/workspace/the-daily-epub/.git/worktrees/the-daily-epub-v2a`
  read-only. The required `git add` failed while creating `index.lock`, so the
  verified changes remain unstaged and the single requested commit still needs
  to be created by the orchestrator with message `Web dashboard v2 step 1:
  public summaries, legacy issue fallback`. No source or data in the forbidden
  main checkout was changed.
- Beyond that commit and the orchestrator's unsandboxed full-suite run, nothing
  remains for step 1. This was not a design step, so no screenshots were taken.
