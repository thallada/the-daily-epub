# Implementation agent brief — The Daily EPUB, "Personalized Curation v2"

You are implementing ONE step of a multi-step plan in the Rust repository at
`/home/thallada/workspace/the-daily-epub` (branch `curation-v2`, already checked out).

The orchestrator hands you `docs/plans/curation-v2-briefs/00-preamble.md` (this file) followed by one
`stepN.md` from the same directory; `docs/plans/2026-09-02-curation-v2-progress.md` records what has
landed so far.

The plan is `docs/plans/2026-09-02-personalized-curation-v2.md`. Read it in full before
writing code: §0 records settled decisions (do not reopen them), §1 lists the files to
read first, §21 is the implementation sequence. Also read
`docs/plans/2026-08-15-implementation-notes.md` §"Cross-cutting implementation decisions"
(note: item 5 is stale — the code uses a hand-rolled `reqwest` client in `src/curate/llm.rs`,
keep that).

## Hard rules

- Implement ONLY the step assigned below. Later steps land separately; do not start them.
  Where this step needs a type or table that a later step fills in, create it now exactly as
  the plan specifies and leave it empty/unused with a short `// filled in by step N` comment.
- Conventions: sqlx *runtime* queries (`sqlx::query(...).bind(...)`, never the `query!` macros),
  `jiff` for time, RFC3339 UTC strings in SQLite, `thiserror`/`anyhow` error style, no `unwrap()`
  outside tests, tracing spans per stage, rustfmt defaults, askama templates.
- Tests never touch the network. Use mock backends following `MockBackend` in `src/curate/llm.rs`.
- Follow the plan's names for modules, functions, config keys, table and column names, enum
  strings and prompt constants exactly. If the plan is internally contradictory or impossible
  for this step, pick the closest behaviour, keep the paper publishable, and describe the
  deviation in your final report. Do not invent scope beyond the plan.
- API keys only from env vars; never in config files, logs, tests or the database.
- Prefer existing dependencies in `Cargo.toml`. Add a new crate only if there is no reasonable
  way without it, and say so in the report.
- Do NOT commit and do NOT create branches. Leave all changes in the working tree; the
  orchestrator reviews and commits. Do not edit anything under `docs/`.
- Keep `config.example.toml` and README in sync with any config keys you add or remove
  (the test `shipped_example_config_parses` must pass).
- Before finishing, run and make pass: `cargo fmt`, `cargo clippy --all-targets` (no new
  warnings), `cargo test`. Fix what you broke; do not delete or `#[ignore]` tests to get green
  unless the plan removes the feature they cover (then move/replace the tests as §18 says).

## Final report

End with a concise report (this is what the orchestrator reads): what you implemented, every
deviation from the plan and why, anything from the step you could not finish, and the exact
`cargo test` summary line(s). Keep it under 60 lines.

## Host notes

- The `openai-codex` Claude Code plugin sandbox works on this host (bubblewrap fixed 2026-09-02):
  `apply_patch`, the shell, `cargo build/test/clippy` and the warm `~/.cargo/registry` all work.
- Inside that sandbox `bind()` on 127.0.0.1 is forbidden, so ten pre-existing tests fail there for
  environmental reasons: the four `curate::llm::tests::anthropic_*` tests,
  `extract::tests::relative_urls_resolve_against_the_url_we_landed_on`, and the five
  `server::tests::*` (the two `tests/m7_server.rs` tests likewise). They are out of scope; do not
  touch them. Every other test must pass; the orchestrator runs the full suite outside the sandbox.
- Cargo's registry is already warm; do not add dependencies that would need a network fetch.
- Work autonomously to completion. Do not stop to ask questions; make the closest-to-plan choice
  and record it in the final report.
