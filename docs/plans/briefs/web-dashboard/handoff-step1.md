# Step 1 handoff — foundation

## Landed

- Added migration `0004_web.sql`: users, SQLite-backed sessions and indexes,
  rating attribution, config/profile/job history, report/issue snapshots, and
  the candidate article/run index. `RatingEvent`/`RatedArticle` now carry a
  nullable user id.
- Persisted full `runs.report_json`, final `issues.report_json`, and a compact
  `issues.issue_json` with article bodies removed. The issue loader rehydrates
  bodies from `articles` and has a reduced-row fallback for pre-migration
  issues. `GenerateOutcome` exposes `run_id`.
- Added the `web` skeleton, final shared-state shape, Askama layout/error/public
  templates, embedded CSS/JS/favicon with SHA-256 ETags, security/cache/request
  headers, and `DisabledRunner`/`MockRunner` stubs.
- Added `axum-login` at the required git revision, the local sqlx 0.9 session
  store, password-auth users/backend, session/auth route layers, role guards,
  same-origin POST middleware, login governor and cleanup tasks, login/logout,
  account/password/session revocation, and session-or-Basic file downloads.
- Added public latest/archive/issue pages, the stripped `PublicIssue` boundary,
  Atom feed, and robots policy. Anonymous page loads remain session-cookie-free.
- Added all `daily-epub users` commands without the pipeline lock, new server
  configuration/defaults/validation, example config, and README command/route/
  reverse-proxy documentation.
- Expanded `tests/m7_server.rs` to cover env-only public routes, dashboard
  redirect, CLI admin bootstrap, and a real TCP login/account request.

## Deviations and follow-up

- `rpassword` could not be added: it is absent from the local Cargo cache and
  this sandbox cannot resolve `index.crates.io` (three retries failed). The CLI
  currently uses an equivalent Unix `/dev/tty` no-echo double prompt and keeps
  `--password-stdin`. Replace that helper with `rpassword::prompt_password`
  after adding the dependency in a network-enabled environment.
- Cargo resolved direct `toml_edit` to 0.22.27 rather than the plan's observed
  0.25.x release; it was selected by `cargo add` for the available toolchain and
  lock graph, not hand-pinned.
- The finished issue report is attached immediately after `finish_run`, rather
  than during the earlier issue snapshot write, because publish timing and the
  final run status are not complete at snapshot time. The stored observable
  value is the same final serialized report.
- The pinned axum-login source confirms `AuthSession::user().await`, immutable
  `login`/`logout`, the macro route layers, and session key
  `"axum-login.data"`; the implementation follows those real APIs.
- Full signed-in issue/article rendering and the real `POST /rate` handler are
  step 2. Step 1 supplies the protected dashboard overview and admin-only 501
  rate stub so route-guard tests exercise the final boundary.

## Verification

- `cargo fmt --check`: pass.
- `cargo clippy --all-targets -- -D warnings`: pass.
- `cargo test` with sandbox-bound tests skipped: **389 passed, 0 failed**.
  Excluded were the four named Anthropic listener tests, the relative-URL
  listener test, five `server::tests` listener tests, both `m7_server` TCP
  tests, and three existing OpenAI fake-server tests that also bind loopback.
- Focused `cargo test web:: -- --nocapture`: **16 passed, 0 failed**.
- `cargo tree -i` shows one `sqlx` version (0.9.0) and one
  `libsqlite3-sys` version (0.37.0).

## Orchestrator review (2026-09-03)

- `rpassword` added (the sandbox had no network); `read_password_hidden` is
  now `rpassword::prompt_password`. `toml_edit` moved to 0.25 as the plan says.
- **Deviation from plan §8 kept on purpose:** `/files/*` with *no* Basic auth
  configured stays public, exactly as before. The plan wanted a redirect to
  `/login` there, but the public OPDS feed's acquisition links point at those
  files and an e-reader cannot log in; acceptance criterion 11 ("`/files`
  behave as before") and "OPDS clients are unaffected" win. With Basic auth
  configured, a session cookie of any role bypasses it.
- A short new password on `/account/password` is a 400 form error, not a 500.
- Full suite outside the sandbox: 370 lib + all integration tests green.
