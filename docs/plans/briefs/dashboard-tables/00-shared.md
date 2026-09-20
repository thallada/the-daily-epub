# Shared brief — dashboard tables (2026-09-20)

The operator sees horizontal scrollbars on most dashboard tables, even on an
ultrawide monitor, because every table is capped at the 80rem page column and
`min-width: max-content` forbids wrapping. The fix is a small, reusable table
framework described in **`docs/dashboard-tables.md` — read it first; it is the
spec.** Two tasks run in parallel in separate git worktrees:

- `01-css.md` — the CSS side (`src/web/tailwind.css`, rebuilt `app.css`).
- `02-templates.md` — the template audit (every `td` gets the right cell class).

## Ground rules

- Work only in the worktree named in your brief. **Never touch the main checkout**
  at `/home/thallada/workspace/the-daily-epub`. **Do not `git commit`** (the sandbox
  cannot write `.git/worktrees`); leave the work in the tree, the orchestrator
  commits.
- Styling lives in **`src/web/tailwind.css`** (Tailwind v4, `@apply` inside
  `@layer components`, semantic colour tokens). After every CSS change run
  `npm run css` (writes `src/web/static/app.css`, which the binary embeds with
  `include_str!`). `npm run css:check` must print no diff at the end.
- CSP forbids inline `<style>`, `<script>` and `style=""` attributes. No new JS,
  no new dependencies, no table library: this is plain CSS + four cell classes.
- Keep every hook `app.js` relies on: `table[data-filter]`, `.table-filter`,
  `input[data-table-filter]`, `.scroll-x` (the filter script does
  `table.closest(".scroll-x")`), `form[data-confirm]`, `details[id]`, `[data-refresh]`.
- Keep `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and the tests
  green (`cargo test --lib web::` while iterating, one full `cargo test` at the
  end). **Sandbox note:** inside the Codex sandbox `bind()` on 127.0.0.1 is
  forbidden; ignore every test that fails with `PermissionDenied`/bind errors
  (about 17 pre-existing ones). Do not "fix" them.
- 4 cores / 7 GB shared with another agent building Rust at the same time: one
  cargo process at a time, be patient.
- KISS / YAGNI: no config knobs, no per-table width tables, no JS resizing, no
  responsive card-ification of rows. Where the brief and the code disagree,
  follow the code and note the deviation in your handoff.
- Finish by writing `docs/plans/briefs/dashboard-tables/handoff-<task>.md`:
  what changed (file by file), deviations, what you verified and how, anything
  left open.

## Previewing (optional — the sandbox usually cannot bind a port)

Seed a throwaway database with `cargo run --example seed_dev_db -- ./dev`
(git-ignored) and serve it with
`DAILY_EPUB_SERVER__HMAC_SECRET=$(head -c 48 /dev/urandom | base64) DAILY_EPUB_SERVER__BIND=127.0.0.1:<port> cargo run -- --config ./dev/config.toml serve`.
`measure.mjs` in this directory (needs `playwright` on the module path; a copy
lives at `~/.npm/_npx/705bc6b22212b352/node_modules`) prints per page and width
whether the document overflows and which table wrappers scroll. If you cannot
bind, rely on the router tests and say so; the orchestrator screenshots every
page at 390/768/1280/1920/2560 after merging.
