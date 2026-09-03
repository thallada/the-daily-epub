# Step 7 handoff — docs, users page, and polish

## Landed

- Added the admin-only, read-only `/dashboard/users` page with username, role, enabled/disabled
  status, created time, last login, and unexpired session count. The page directs edits to the
  existing `daily-epub users` CLI.
- Expanded the README with the public/user/admin boundary, user lifecycle commands, complete
  route table, systemd+polkit Jobs setup, comment-preserving Settings behavior, environment
  locks, shipped-provider removal limitation, public `/files/*` behavior without Basic auth,
  and the `X-Forwarded-For`/loopback trust boundary. Updated `[server]` comments in
  `config.example.toml` to match.
- Added the dated Web dashboard section to the implementation notes: polkit 124 and its narrow
  rule, service-unit changes, the `toml_edit` write path, the pinned auth/session/password/
  throttle stack, sandbox test caveats, and the implementation-time decisions from steps 1–6.
- Added `docs/runbooks/web-dashboard-rollout.md`, with the plan's nine production rollout steps,
  commands, permissions, public-files compatibility note, and smoke checks.
- Completed layout polish: Atom discovery and favicon remain in `<head>`, Users remains in the
  admin nav, both navs now wrap cleanly, narrow cards/forms/rating controls collapse, dark-mode
  form controls use the site colors, focus states are visible, and all wide tables (including
  the nested signals table and new users table) sit in `.scroll-x`.
- Unknown routes and template-render failures now use the site-layout 404/500 pages. Issue
  download buttons show human-readable B/KB/MB/GB sizes.
- Removed stale diff3 ancestor markers from the merged CSS/JavaScript and restored the missing
  table-filter closure; `node --check src/web/static/app.js` passes.
- Added the users guard/content test, file-size tests, site-layout error test, and one
  fixture-backed router smoke test that renders every dashboard route template.

## Acceptance criteria walk-through

1. **Public issue and Atom boundary — met.** `PublicIssue` cannot carry generated/body fields;
   existing fixture tests assert titles/authors/sources/comment links/metadata are present and
   private Brief/summary/why/body/comment/World text is absent. The feed parses as XML and uses
   the same public view.
2. **Complete signed-in issue — met.** Existing router tests cover Brief, summaries, why lines,
   article bodies/discussion, World and Behind pages, reduced pre-snapshot fallback, and
   existence-gated downloads. All three artifact slots share the same loader; sizes are now
   human-readable.
3. **Admin-only ratings — met.** Whole-router role guards and rating tests cover anonymous,
   user, and admin outcomes, attributed `source = 'dashboard'` events, clear events, and the
   unchanged HMAC path.
4. **Article history/explainability — met.** The articles list/detail routes expose the latest
   stage plus full run history, reasons, signals, assessments/facets, utility/cluster/admission,
   editor why, issue appearances, neighbours, embedding metadata, ratings, and text explain;
   allow-listed query tests and fixture rendering pass.
5. **Run history — met.** Run list/detail renders the cumulative funnel and reasons, admission,
   preference state, timings, provider costs, warnings, feeds, near misses, config diff, and
   candidate table; seeded counts, sorts, filters, links, and sparklines are tested.
6. **Ratings contributions/history — met.** Current and events tabs cover decay, decayed feed
   credit, neighbour weight, prompt/rebuild membership, last-run use, append-only edits/clear,
   annotations, filters, and superseded history with hand-checked tests.
7. **Settings — met.** The schema/default/help/env/secret tests cover every config leaf. Writer
   tests cover typed changes, comments/order/mode preservation, validation before rename,
   history, providers, and reload-on-mtime. The README records why shipped providers cannot be
   removed.
8. **Profile — met.** Save/restore/version tests cover the atomic editor and parsed preview;
   the page shows OPML interests, prompt, learned adjustments/staleness, and the rebuild job.
9. **Jobs — met.** Fixed-name parsing, polkit/unit files, MockRunner starts/status/logs, duplicate
   refusal, failure persistence, lifecycle/run linkage, journal tail, reload-before-start, and
   overview links are present and tested. Pipeline work remains in the separate job unit.
10. **Session/security stack — met.** The pinned axum-login/tower-sessions store,
    password-auth hashes, cookie properties, session invalidation, origin checks, login throttle,
    CSP/security headers, safe redirect checks, and secret redaction have focused tests.
11. **Existing routes — met.** HMAC, OPDS, files, health, and reports remain wired; issue reports
    are populated. The reviewed compatibility decision keeps files public only when Basic auth
    is absent, while configured Basic auth or a valid session still gates them.
12. **Test coverage — met, with sandbox execution caveat.** All new router tests use `oneshot`;
    no new test binds or invokes systemd. The filtered full run passed every runnable target.
    The orchestrator should run the complete suite outside this listener-restricted sandbox.

## Prior handoff follow-ups

- Step 1's `rpassword` and direct `toml_edit 0.25` follow-ups were already resolved by the
  orchestrator; its public-files deviation is now documented.
- Step 2's raw download sizes are resolved here.
- Step 3's Jobs/ratings targets and overview sparklines, Step 4's settings anchors and profile
  rebuild action, and Steps 5–6's config reload plus combined unit changes are all present after
  the merged steps and were included in the smoke/audit pass.
- Nothing remains open against §20. Production deployment itself is intentionally left to the
  operator following the new runbook. Plan §21's product deferrals remain intentional: per-user
  personalization, OPML editing, body FTS, SSE logs, passkeys, public why lines, old snapshot
  backfill, and charts beyond the current sparklines.

## Verification

- `cargo fmt`: pass.
- `cargo clippy --all-targets -- -D warnings`: pass.
- `node --check src/web/static/app.js`: pass.
- Focused new router/size/error tests: pass.
- Raw `cargo test`: **426 passed, 13 failed** before Cargo stopped at the library target; every
  failure was `PermissionDenied` while binding a loopback listener (the four documented
  Anthropic tests, three OpenAI tests using the same helper, the extractor test, and five server
  tests).
- `cargo test` with those 13 listener tests plus the two `tests/m7_server.rs` TCP tests filtered:
  **460 passed, 0 failed, 15 filtered out** across library, binary, integration, and doc targets.

