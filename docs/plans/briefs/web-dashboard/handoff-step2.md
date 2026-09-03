# Step 2 handoff — full issues and ratings

## Landed

- Signed-in `/` and `/issues/{date}` now render the full issue: Brief, download
  links for artifacts that still exist, the section/article index with summaries
  and `why`, admin rating widgets, World/Behind links when their snapshot data is
  present, and colophon facts.
- Added login-protected article, World Briefing, and Behind the paper pages.
  Article pages use the EPUB's ammonia cleaning path without XHTML conversion,
  retain remote images with lazy/no-referrer attributes, include rendered
  discussions, previous/next navigation, source links, and admin ratings.
- Replaced the Step 1 `/rate` stub with the admin-only form/JSON handler. Events
  are append-only `dashboard` events attributed to the viewer; missing issue
  dates use `latest_issue_date_for_article`; clear events use the CLI's exact
  `cleared`/`0.0` representation; form redirects validate `next` and carry a
  flash, while JSON returns the event id.
- Added the no-JS rating partial and JavaScript enhancement, active-state
  updates, issue/article styling, and the signed-in private cache policy.
- The e-ink HMAC confirmation page now links to the corresponding site issue.
- Added router tests for full/fallback rendering, article discussions and image
  handling, World/Behind pages, 404s and login protection, artifact gating,
  both rating representations, role guards, attribution, fallback dates,
  validated redirects, clear values, admin widget state, and near-miss links.

## Deviations and notes

- The pinned `axum-login` dependency disables tower-sessions' `axum-core`
  feature, so its re-exported `Session` does not implement an Axum extractor in
  this dependency graph. Handlers use `Extension<Session>` to read the exact
  session already installed by the auth layer; no second tower-sessions
  dependency was added.
- The web form and JSON response use `down`, as specified for the widget, while
  the persisted event label is `not_for_me`, matching `cmd_ratings` and the
  existing learned-rating queries exactly.
- No migration was needed. Step 3 can construct `RatingWidget` with
  `show_note = true` for dashboard article variants.

## Verification

- `cargo fmt`: pass.
- `cargo clippy --all-targets -- -D warnings`: pass.
- Focused `cargo test web:: -- --nocapture`: **22 passed, 0 failed**.
- `cargo test` with the documented sandbox listener tests skipped:
  **395 passed, 0 failed, 15 filtered out**. The filtered tests were the four
  Anthropic listener tests, three OpenAI fake-server listener tests noted in the
  Step 1 handoff review, the relative-URL listener test, five `server::tests`
  listener tests, and both `tests/m7_server.rs` tests.

## Orchestrator review (2026-09-03)

- Accepted as is. `Extension<Session>` for flashes is fine (the auth layer's
  session manager inserts it); no second tower-sessions dependency.
- Cosmetic follow-up for step 7: download buttons show raw byte counts;
  render them human-readable (KB/MB).
- Full suite outside the sandbox: 376 lib + all integration tests green.
