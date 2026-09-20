# Dashboard tables template handoff

Implemented `02-templates.md` against `00-shared.md` and
`docs/dashboard-tables.md`. No columns, content, hooks, or attributes were
changed. The existing page-level `.scroll-x` wrappers already had the required
direct-child topology, so none moved.

## Cell audit by template

| Template | Cells that lost `cell-tight` | Cells that gained `cell-wrap` | Clamp added |
| --- | --- | --- | --- |
| `_candidate_row.html` | Feed, stage, reason, admitted by, flags | Flags (badge list) | None |
| `_signals_table.html` | None | None | None |
| `dashboard/article.html` | Run history: stage, reason, admitted by; nearest articles: feed, rating; rating events: label, kind, source, user | Top-interests interest name | Rating-event note: inner `line-clamp-3` with the full note in `title` |
| `dashboard/articles.html` | Feed, last stage (badge plus run date), reason, rating | None | None |
| `dashboard/feeds.html` | None | None (Feed and Why already had it) | None (existing Feed/Why inner clamps retained) |
| `dashboard/interests.html` | Category | None | None |
| `dashboard/jobs.html` | Job name, requested by, status | None (message already had it) | Message: inner `line-clamp-3` with the full message in `title` |
| `dashboard/profile.html` | Saved by, restore action | Preview | None |
| `dashboard/ratings.html` | Imports: verdict, status; events: verdict, source, by; current: when/by, feed credit, used last run | Current feed credit | Import message, event note, and current note: inner `line-clamp-3` with full text in `title` |
| `dashboard/run.html` | Funnel stage; near misses: feed, stage | None | None |
| `dashboard/runs.html` | Status (badge plus dry-run text) | None | None |
| `dashboard/settings_history.html` | Who | None | None |
| `dashboard/stats.html` | Runs status | None | None |
| `dashboard/users.html` | None | None | None |

`dashboard/feeds.html` also lost page-level `table-fixed`; its existing header
width hints and inner Feed/Why clamps remain. The card-local `table-fixed` feeds
table in `dashboard/run.html` remains as specified.

## Deviations

None. Templates whose cells were already compliant were left unchanged.

## Verification

- `cargo test --lib web::`: 135 passed; two Miniflux mock-server tests failed
  only because sandbox `bind()` returned `PermissionDenied` at
  `src/web/dashboard/feeds.rs:497`, as permitted by the brief.
- `cargo fmt`: passed.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `git diff --check`: passed.

The first clippy attempt exhausted the filesystem while writing generated Rust
metadata. Removing the disposable `target/debug/incremental` cache freed enough
space; the exact clippy command then completed successfully.
