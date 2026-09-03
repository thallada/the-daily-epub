# Shared brief — web dashboard implementation agents

You are implementing one step of `docs/plans/2026-09-03-web-dashboard.md` in the
`daily-epub` Rust crate. Read that plan **in full** before writing code — every
decision in it is settled (§0), and §2 records verified facts about the crates and
the host. Also read `docs/plans/2026-08-15-implementation-notes.md` (conventions:
runtime sqlx queries with manual row mapping, `jiff` for time with RFC3339 UTC
strings in SQLite, `thiserror`/`anyhow` errors, askama templates, no network in
tests, rustfmt defaults, no `unwrap()` outside tests, tracing spans).

## Ground rules

- Work on the current branch (`web-dashboard`) in place. Do **not** commit; the
  orchestrator reviews and commits. Do not create branches or stash.
- Do not edit earlier migrations (`0001`–`0003`). Step 1 creates
  `migrations/0004_web.sql`; later steps may append a new migration file only if
  the plan says so.
- Templates for the web live in `src/web/templates/` (`.html`, HTML-escaped by
  default); `askama.toml` lists both template dirs. EPUB templates in
  `src/epub/templates/` are untouched.
- Every `|safe` in a template must be one of the sanitized inputs listed in plan
  §16; user text is never `|safe`.
- The existing routes (`/r/…`, `/opds…`, `/files/…`, `/healthz`, `/issues.json`)
  and their tests keep working unchanged.
- Tests: unit tests inline per module; router tests with
  `tower::ServiceExt::oneshot` against `server::router(...)` over a temp DB
  (`tempfile`), never the network, never systemd. Add the tests the plan's §17
  lists for your step.
- Keep `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test`
  green. Finish with those three commands and report their output.
- **Sandbox note:** inside your sandbox `bind()` on 127.0.0.1 is forbidden, so
  exactly these pre-existing tests fail there and must be ignored:
  `curate::llm::tests::anthropic_*` (4), `extract::tests::relative_urls_resolve_against_the_url_we_landed_on`,
  `server::tests::*` (5 that bind a listener), and any test in `tests/m7_server.rs`.
  Do not "fix" them. Any **new** router test must use `oneshot` and not bind.
  The orchestrator runs the full suite outside the sandbox.
- Don't gold-plate: implement what the plan says for your step and nothing from
  later steps beyond stubs the plan explicitly asks for. Where the plan's
  sketch and the real crate API disagree, follow the real API (read the crate
  source in `~/.cargo/registry` or `~/.cargo/git`) and note the deviation.
- When you finish, write a short handoff at
  `docs/plans/briefs/web-dashboard/handoff-step<N>.md`: what landed, deviations
  from the plan and why, anything left for the next step, test counts.
