# Web site and operator dashboard

**Date:** 2026-09-03
**Repository:** `thallada/the-daily-epub`
**Status:** implementation plan, ready to execute
**Builds on:** `docs/plans/2026-09-02-personalized-curation-v2.md` (the ranker this dashboard inspects; its §23 deferred a "web dashboard for ratings and knobs" — this is that plan)

This plan is written so that a fresh implementation agent can execute it end to end. It records the decisions made in the 2026-09-03 brainstorming session with the operator, the verified facts about the current code and the production host, the target design in enough detail to code from, and the order in which to land it.

---

## 0. Decisions recorded from the brainstorm (2026-09-03)

These are settled. Do not reopen them during implementation.

| Topic | Decision |
|---|---|
| Purpose | Publicly, `https://daily.hallada.net` is an aggregator: every issue as a stripped HTML page of titles linking out, authors, sources and comment links. Privately, after login, it is the operator's dashboard: every article ever considered and why it was or was not in the paper, the full rating history with edits, every issue in full HTML with downloads, all config knobs, and pipeline actions. |
| Frontend | Server-rendered **askama** templates plus one hand-written CSS file and a few hundred lines of plain JavaScript. No node build, no SPA, no framework; assets are embedded in the binary with `include_str!`. |
| Auth | Username + password, sessions in a SQLite table, one `HttpOnly; Secure; SameSite=Lax` cookie. Users are created from the CLI (`daily-epub users add`). No third-party identity, no passkeys. **Use the ecosystem, not hand-rolled code**: `axum-login` over `tower-sessions` for login/logout/session lifecycle and route guards, `password-auth` for argon2 hashing, `tower_governor` for the login throttle. axum-login is taken from **git at the pinned rev `151c72d7a1b4646830f86b4332e6bd6e34d719a7`** (`main`, 2026-05-07: tower-sessions 0.15 and the finalized `Require` API), not from the 0.18.0 crates.io release; §2 records why and what was verified. The only auth code we write is a ~60-line `SessionStore` over our own sqlx pool (the official store pins sqlx 0.8; we are on 0.9) and the `AuthnBackend` glue (§6). |
| Roles | `user` and `admin`. **Signed-in users of either role** see every issue in full HTML (Brief, summaries, article bodies, discussions, World Briefing, Behind the paper) and can download the EPUB/XTC files. **Admins** additionally rate, browse the dashboard, edit settings and the profile, and start jobs. |
| Copyright boundary | Anonymous visitors never see generated or scraped text: no Brief, no summaries, no `why` lines, no article bodies, no comments, no World Briefing. Sections, reading time and word count are fine. The public renderer takes a dedicated `PublicIssue` type that cannot carry the private fields. |
| Personalization | One shared algorithm for now. `rating_events` gains a nullable `user_id` so a per-user algorithm is possible later; nothing else is per user. HMAC links from the EPUB stay unattributed (`user_id NULL`, `source = 'epub'`). |
| Settings | Every key of `config.toml`, grouped by table, editable from the UI and written back **in place with comments preserved** (`toml_edit`). A key overridden by a `DAILY_EPUB_*` environment variable is shown locked. API keys and the HMAC secret are shown as present/absent only. The page re-reads the file on every view, so hand edits show up. `data/profile.md` gets an editor with version history. |
| Actions | A Jobs page starts pipeline work through **systemd**: `systemctl start daily-epub-job@<name>.service`, permitted by a polkit rule for the `daily-epub` user. The server never runs the pipeline in-process (its unit has `MemoryDenyWriteExecute=yes`, which the XTC converter's Node JIT cannot live with). |
| Feed | A public Atom feed at `/feed.xml` with one entry per issue, carrying the same stripped list as the public page. |
| Existing routes | `/r/…`, `/opds…`, `/files/…`, `/healthz`, `/issues.json` keep working unchanged. `/files/*` additionally accepts a session cookie. |
| Retention | HTML issue pages are built from the database, so they outlive the EPUB files (`retention_days`); download links appear only while the file exists. The operator should raise `curation.ranking.telemetry_retention_days` if they want candidate history beyond 180 days; this plan does not change the default. |
| Design | Newspaper masthead in a serif, dashboard tables in the system sans stack, no external fonts or CDNs, light and dark via `prefers-color-scheme`, every wide table scrolls inside its own container. |

---

## 1. Read these first

- `docs/plans/2026-08-15-implementation-notes.md` — conventions (runtime sqlx queries, `jiff`, error style, askama, no network in tests). All of it applies here.
- `docs/plans/2026-09-02-personalized-curation-v2.md` §6 (feedback), §7 (tables), §9–§13 (signals, admission, utility, editor — the numbers the dashboard explains), §15 (explain/stats), §19 (config).
- `src/server.rs` — the existing axum router, `AppState`, `handle_rating`, Basic auth, `safe_join`, the tiny e-ink pages. This plan extends this file's router and moves nothing.
- `src/auth.rs` — HMAC rating links (unchanged).
- `src/config.rs` — `Config`, `Config::load`, `Config::resolve_path`, `validate`, `check_report`, `providers_redacted`, `stale_toml_error`, `ENV_PREFIX`/`ENV_SPLIT`.
- `src/db.rs` — `Db`, `current_ratings`, `append_rating_event`, `upsert_issue`, `replace_issue_articles`, `start_run`/`finish_run`, `provider_spend_for_utc_day`, `get_article`.
- `src/curate/telemetry.rs` — `ExplainRow`, `resolve_run`, `explain_row`, `near_misses`, `render_explain`, `stats`, `SignalsJson`. The dashboard renders the same data as HTML.
- `src/curate/signals.rs` — `decay`, `gate`, `direct_feeds`, `PreferenceState::load/summary`, used by the ratings page.
- `src/curate/profile/mod.rs` — `parse_profile_str`, `load_profile`, `load_or_build`, `rebuild`, `KV_LEARNED_ADJUSTMENTS`.
- `src/epub/chapters.rs`, `src/epub/templates/` — what an issue contains and how it is rendered; `prepare_body`, `social_line`, `behind_*_line`, `render_in_this_issue`.
- `src/epub/fixtures.rs` — `fixtures::issue()` builds a full `Issue` for tests; the web tests use it.
- `src/main.rs` — CLI layout (`clap` derive), `lock_holder`, `cmd_*` helpers.
- `systemd/`, `README.md` §"Reverse proxy", `docs/runbooks/curation-v2-migration.md` — how the host is set up.

---

## 2. Verified facts (2026-09-03)

Host and deployment (checked on the production box, which is also the dev checkout):

- nginx site `daily` proxies `/` → `http://127.0.0.1:3499` with `Host`, `X-Forwarded-For` and `X-Forwarded-Proto` set, TLS from Let's Encrypt, HTTP→HTTPS redirect. `snippets/security-headers.conf` adds `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy: strict-origin-when-cross-origin`, a `Permissions-Policy`, and HSTS. **No Content-Security-Policy**, so inline `<script>`/`<style>` would work, but this plan serves CSS/JS from `/static/` anyway and adds its own CSP header from the app (§16).
- `daily-epub.service` and `daily-epub-generate.timer` are active. Both units run as `daily-epub` (uid 995, no supplementary groups) with `ProtectSystem=strict`, `ProtectHome=read-only`, `ReadWritePaths=/home/thallada/bookorbit/books/daily-epub /var/lib/daily-epub/xtc`, `StateDirectory=daily-epub`. **`/etc/daily-epub` is therefore read-only inside the server** — the settings writer needs `ReadWritePaths=/etc/daily-epub` (§18). The server unit alone has `MemoryDenyWriteExecute=yes`.
- `/etc/daily-epub/config.toml` is `0640 daily-epub:daily-epub` (per the unit header and runbook); `/etc/daily-epub/env` holds the API keys and `DAILY_EPUB_SERVER__HMAC_SECRET`.
- polkit is version 124 (JavaScript rules in `/etc/polkit-1/rules.d/` are supported). `journalctl -u <unit>` as `daily-epub` requires membership in `systemd-journal` (gid 999).
- `systemctl show -p ActiveState,SubState,Result,ExecMainStatus,ExecMainStartTimestamp,ExecMainExitTimestamp <unit>` works unprivileged and is the status source for jobs.

Code:

- `issues.report_json` is **never written**: `pipeline::record_issue` passes `None` to `upsert_issue`, and nothing else sets it, so `GET /issues.json` returns `"report": null` for every issue. `runs` keeps only counters. This plan fixes both (§5.3).
- The World Briefing and the discussion chapters are built at run time and never persisted; `Issue` (with `Lineup`, `Editorial`, `WorldBriefing`, `Colophon`, `BehindThePaper`, `Pick.discussion`) derives `Serialize`/`Deserialize`. Article bodies live in `articles.content_html` for every article ever ingested; nothing prunes `articles`.
- `Config::load(explicit)` reads `--config` or `./config.toml`, then `DAILY_EPUB_*` env (`ENV_PREFIX = "DAILY_EPUB_"`, `ENV_SPLIT = "__"`), accepts `DAILY_EPUB_SECRET` as an alias for `server.hmac_secret`, then `validate()`. `Config::resolve_path` returns the path `load` used. `Config` and every sub-struct derive `Serialize`/`Deserialize` with `deny_unknown_fields`; `Config::default()` is the shipped example key for key (a test enforces it).
- `main::serve` receives `(config, db)` only; the config path is not passed through. `AppState { db, config }` is cloned per request.
- Crates already present: `axum 0.8`, `askama 0.16`, `tower-http 0.7` (`trace`, `fs`), `sqlx 0.9` (sqlite, runtime queries), `jiff`, `serde_json`, `sha2`, `hex`, `hmac`, `base64`, `rand 0.10`, `ammonia`, `figment`, `libc`. `toml_edit` and `toml` are in `Cargo.lock` transitively (via figment) but not direct dependencies; `tower` is transitive only.
- Current crates.io versions (checked with `cargo search` on 2026-09-03): `toml_edit 0.25.13`, `tower 0.5.3`, `askama 0.16.0`. Add with `cargo add` and let Cargo resolve; do not hand-pin older versions.

Auth ecosystem (crates.io dependency metadata and docs.rs, checked 2026-09-03):

- **`axum-login` 0.18.0** (crates.io, released 2025-07-20, ~1M downloads): `axum ^0.8.1`, `tower-sessions ^0.14` (re-exported as `axum_login::tower_sessions`), `tower-cookies ^0.11`. Exports `AuthUser` (`id()`, `session_auth_hash()`), `AuthnBackend` (`authenticate`, `get_user`), `AuthzBackend` (`get_user_permissions`), the `AuthSession` extractor, `AuthManagerLayerBuilder::new(backend, session_layer).build()`, and the `login_required!`, `permission_required!`, `predicate_required!` route-layer macros. Login cycles the session id (fixation defence) and stores the user's `session_auth_hash`, so changing a password invalidates every session of that user.
- **`axum-login` `main` at `151c72d7a1b4646830f86b4332e6bd6e34d719a7`** (2026-05-07, "Upgrade tower-sessions to 0.15.0 (#315)"; 13 commits past `v0.18.0`, none since; the default branch is `main`, there is no `master`). What changed since 0.18.0 (CHANGELOG "Unreleased" + diff): `tower-sessions ^0.15`; the new builder-based `require` module (`Require::<Backend>::builder().decision(PermissionsPredicate).unauthenticated(RedirectHandler::new().login_url(..).redirect_field(..)).build()`) with the macros rewritten as thin wrappers over it; `permission_required!` answers 401 when unauthenticated (302 to `login_url` when one is given) and 403 only when authenticated without the permission; `AuthSession` is now `Clone` over an `Arc<Mutex<_>>` with `login(&self, &user)`, `logout(&self)`, `authenticate(&self, creds)`, **`user().await -> Option<User>` (a method, no longer a public field)** and `backend()`; the `session` field is no longer public (use tower-sessions' own `Session` extractor for flashes); the `async-trait` dependency is gone (native async fn in traits). The user id is stored in the session under the key `"axum-login.data"` (configurable with `AuthManagerLayerBuilder::with_data_key`).
  **Verified locally on 2026-09-03** (nightly 1.100, 2026-09-01) at that rev: `cargo clippy --lib --all-features` clean; `cargo test --doc` 15/15; `cargo test --lib` 79/79 once the unit-test modules use `MemoryStore` instead of `tower_sessions_sqlx_store::SqliteStore`; `tests/feature_gates.rs` passes. A scratch consumer crate depending on the git rev together with `tower-sessions 0.15`, `sqlx 0.9` (sqlite, single copy, single `libsqlite3-sys 0.37`), `axum 0.8.9`, `tower_governor 0.8`, `password-auth 1.0` and the §6.2 store implementation compiles (`cargo check`, `cargo tree -d` shows no second sqlx or sqlite).
  **Why the repository's CI is red** (the push run for that commit fails at the `check` job's clippy step and skips the tests): the crate's dev-dependency and its examples use `tower-sessions-sqlx-store 0.15.0`, which depends on `tower-sessions-core 0.14`; against `tower-sessions 0.15` that store no longer implements the (0.15) `SessionStore` trait, so the test targets and the examples fail to compile and the integration tests (which spawn the example binaries) fail. That is a harness/examples problem in the stores repository lagging behind, not a library defect; our own store is written against whatever `tower-sessions` axum-login re-exports, so it is unaffected either way.
  **Maintenance status** (discussion #330, opened 2026-07-12 by the author): "I don't have the time I would like to maintain axum-login"; three volunteers (one collaborator offering co-maintenance, one asking about updating the stores for 0.15 on 2026-08-06) and no release or new commit since 2026-06. Expect a 0.19 eventually; pinning the rev means the plan's API is the one 0.19 will ship, and a later switch to the crates.io release is a one-line `Cargo.toml` change.
  **Fallback** if the git dependency is ever unwanted: `axum-login = "0.18"` from crates.io with `tower-sessions 0.14`; the only source differences are `auth.user` (field) instead of `auth.user().await`, `&mut auth` for `login`/`logout`, and `pub session` on `AuthSession`.
- **`tower-sessions` 0.15.0** (`SessionManagerLayer::new(store).with_secure(..).with_same_site(..).with_name(..).with_expiry(Expiry::OnInactivity(time::Duration))`): lazy — no cookie is set unless a handler touches the session, so anonymous pages stay cookie-free. `SessionStore` is a trait with `save`, `load`, `delete` (and a provided `create`) over `Record { id: Id, data: HashMap<String, serde_json::Value>, expiry_date: time::OffsetDateTime }`; the trait is unchanged from 0.14. 0.15 over 0.14 is only the `rand 0.9` bump and a memory-ordering fix in the session's modified-flag handling (#254), which is the other reason to take the git rev of axum-login rather than 0.18.0.
- **`tower-sessions-sqlx-store` 0.15.0** pins **`sqlx ^0.8`** (and `tower-sessions-core 0.14`). This crate is on `sqlx 0.9`; two sqlx versions would each bundle SQLite and collide on the `links = "sqlite3"` native key, so the official store **cannot** be used. A store over our own pool is ~60 lines (§6.2).
- **`password-auth` 1.0.0** (`generate_hash(password) -> String` PHC/argon2id, `verify_password(password, hash) -> Result<(), VerifyError>`; `argon2 ^0.5` inside). The crate axum-login's own SQLite example uses.
- **`tower_governor` 0.8.0** (`governor 0.10`, `axum` feature for `axum ^0.8`): `GovernorConfigBuilder::default().per_second(n).burst_size(n).key_extractor(SmartIpKeyExtractor)`, `GovernorLayer::new(config)`, a periodic `limiter.retain_recent()` cleanup, and `into_make_service_with_connect_info::<SocketAddr>()` on the server. `SmartIpKeyExtractor` reads `x-forwarded-for`, `x-real-ip`, `forwarded`, then the peer address.
- **`axum_csrf` 0.11.0** (`axum-core ^0.5`, `cookie 0.18`): a signed-cookie token with a `CsrfToken` extractor (`authenticity_token()`, `verify()`). Compatible, but it is a second cookie plus per-form plumbing that `SameSite=Lax` and a fetch-metadata origin check already cover; kept as the fallback option (§6.4).
- **`axum_session` 0.21 / `axum_session_sqlx` 0.11 / `axum_session_auth` 0.21** (released 2026-08-10): match `sqlx ^0.9` and `axum ^0.8.9` exactly and would need no custom store, but they add `chrono`, `dashmap` and `async-recursion`, create their own table outside our migrations, cache users in memory (`cache_clear_user` after a role change), and set a cookie for every visitor unless the session mode is changed. Rejected in favour of the larger and lazier `axum-login`/`tower-sessions` pair; recorded here so the choice is not re-litigated.
- `askama.toml` has `dirs = ["src/epub/templates"]`; the EPUB templates are `.xhtml` with `escape = "html"` declared per template. `.html` templates are HTML-escaped by default.
- `telemetry::resolve_run` treats the latest run with `status != 'dry_run'` as the run of a date; `RunStatus` is `running | ok | degraded | failed | dry_run`.
- `db::current_ratings_including_cleared(lookback_days)` returns the latest explicit event per article joined with title, feed, latest summary and deep facets. `profile::MAX_RATINGS_IN_REBUILD` (200) and `RATINGS_LOOKBACK_DAYS` (36,500) are private constants; make the first `pub`.
- `signals::decay(age_days, half_life)`, `signals::gate(n, floor, full)`, `signals::direct_feeds(&article)` and `PreferenceState::load(..)` + `summary()` exist and are `pub`.
- Rating attribution today: `RatingEvent { source: "epub" | "cli" | "migration", .. }`; `"dashboard"` is already reserved in the plan's `source` comment.
- `publish::issue_filename(date, edition, "epub")` gives the EPUB names the OPDS feed and `/files/epub/{name}` use; `issues.epub_path/x4_path/xtc_path` hold full paths.
- `server.rs` unit tests call handlers and helpers directly (12 tests); `tests/m7_server.rs` drives the real binary over TCP with a hand-rolled HTTP client and an env-only configuration. Both patterns continue.

---

## 3. Site map and access levels

| Route | Anonymous | Signed in (user) | Admin |
|---|---|---|---|
| `GET /` | latest issue, public rendering | latest issue, full rendering | same + rating buttons |
| `GET /issues` | archive by month | same | same |
| `GET /issues/{date}` | public issue page | full issue page + downloads | same + rating buttons |
| `GET /issues/{date}/articles/{article_id}` | 302 → `/login?next=` | article chapter (body, discussion) | same + rating buttons |
| `GET /issues/{date}/world`, `/behind` | 302 → login | the chapter | same |
| `GET /feed.xml` | Atom, public content | same | same |
| `GET /robots.txt` | allow `/`, `/issues`; disallow `/dashboard`, `/login`, `/files`, `/r`, `/opds` | | |
| `GET /static/{file}` | `app.css`, `app.js`, `favicon.svg` | | |
| `GET /login`, `POST /login`, `POST /logout` | login form | | |
| `GET /account`, `POST /account/password` | 302 → login | change own password, sign out everywhere | same |
| `GET /files/epub/{name}`, `/files/xtc/{name}` | Basic auth (existing) | session cookie **or** Basic auth | same |
| `POST /rate` | 403 | 403 | append a rating event |
| `GET /dashboard` | 302 → login | 403 page | overview |
| `GET /dashboard/runs`, `/dashboard/runs/{id}` | | 403 | runs list, run detail with candidate table |
| `GET /dashboard/articles`, `/dashboard/articles/{id}` | | 403 | every article, article detail |
| `GET /dashboard/ratings`, `POST /rate` | | 403 | rating history, contributions, edit |
| `GET /dashboard/profile`, `POST /dashboard/profile`, `POST /dashboard/profile/restore` | | 403 | `profile.md` editor, prompt, history |
| `GET /dashboard/stats` | | 403 | the `stats` numbers as tables + sparklines |
| `GET /dashboard/settings`, `POST /dashboard/settings`, `POST /dashboard/settings/providers`, `GET /dashboard/settings/history` | | 403 | every config key |
| `GET /dashboard/jobs`, `POST /dashboard/jobs/{name}`, `GET /dashboard/jobs/{id}` | | 403 | start and watch jobs |
| `GET /dashboard/users` | | 403 | list users and sessions (read-only; CLI edits) |
| `GET /r/{date}/{article_id}/{vote}?t=` | HMAC (existing) | | |
| `GET /opds…`, `/healthz`, `/issues.json` | existing behaviour | | |

Every `/dashboard/*` page is `Cache-Control: no-store`. Every HTML response carries `Vary: Cookie`.

Dates in paths are `YYYY-MM-DD` issue dates; ids are database ids. An unknown date or id is a 404 page in the site layout, not a bare string.

---

## 4. Architecture

### 4.1 Module layout

```text
src/server.rs                  existing router + handlers; `router()` now merges `web::router()`
src/web/
├── mod.rs                     WebState, `Html<T>` response helper, WebError, layout context, pagination, time formatting
├── session.rs                 `SqliteSessionStore` (tower-sessions `SessionStore` over our pool), `Backend` (axum-login `AuthnBackend` + `AuthzBackend`), `Viewer` helper, origin check middleware
├── users.rs                   User/Role types, password-auth hash/verify wrappers, `daily-epub users …` implementation
├── public.rs                  `/`, `/issues`, `/issues/{date}` (public branch), `/feed.xml`, `/robots.txt`, `PublicIssue`
├── issue.rs                   IssueView loader (issue_json → fallback), full issue page, article/world/behind pages, downloads gate
├── rate.rs                    `POST /rate` (form + JSON), rating widget data
├── dashboard/
│   ├── mod.rs                 overview
│   ├── runs.rs                runs list, run detail, candidate table, config diff
│   ├── articles.rs            articles list, article detail
│   ├── ratings.rs             ratings page, contributions
│   ├── profile.rs             profile editor, history, prompt view
│   ├── stats.rs               stats page (renders `telemetry::StatsData`)
│   ├── settings.rs            settings schema, form, toml_edit writer, providers, history
│   ├── jobs.rs                jobs page, JobRunner trait, systemctl/journalctl runner
│   └── users.rs               users list page
├── static/
│   ├── app.css
│   ├── app.js
│   └── favicon.svg
└── templates/                 askama `.html` templates (§4.3)
src/jobs.rs                    job catalogue (name → CLI equivalent), `daily-epub job run`, jobs table lifecycle
migrations/0004_web.sql
systemd/daily-epub-job@.service
systemd/50-daily-epub.rules    polkit rule
docs/runbooks/web-dashboard-rollout.md
```

`askama.toml` becomes `dirs = ["src/epub/templates", "src/web/templates"]`. Web templates end in `.html`; there is no name collision with the `.xhtml` EPUB templates.

### 4.2 State

```rust
pub struct AppState {
    pub db: Db,
    pub config: Arc<RwLock<Arc<Config>>>,     // swapped after a settings save or when the file's mtime changes
    pub config_path: Option<PathBuf>,        // Config::resolve_path(cli.config); None ⇒ settings read-only
    pub web: Arc<WebState>,
}
pub struct WebState {
    pub jobs: Arc<dyn JobRunner>,            // SystemdRunner in production, MockRunner in tests
    pub started_at: Timestamp,
    pub config_mtime: Mutex<Option<SystemTime>>,
}
impl AppState { pub fn config(&self) -> Arc<Config> }   // read-lock + clone the Arc
```

`main::serve` gains the config path: `server::serve(config, config_path, db)`. Handlers that today read `state.config.server.*` call `state.config()` instead (mechanical change; keep the tests).

**Config reload.** `WebState::reload_if_changed(&AppState)` compares the file's mtime with the cached one on every `/dashboard/settings` GET and every job start, re-runs `Config::load(config_path)` and swaps the Arc when it changed and validates. A file that fails validation is reported on the settings page (`! config.toml on disk does not load: <error>`) and the previous config stays live.

### 4.3 Templates and rendering

- `layout.html`: `<head>` (charset, viewport, `<title>`, `/static/app.css?v={VERSION}`, Atom `<link rel="alternate">`, favicon), masthead ("The Daily EPUB", serif), primary nav (Latest · Archive · Feed · Sign in / account name), admin nav bar when the viewer is an admin (Overview · Runs · Articles · Ratings · Profile · Stats · Jobs · Settings · Users), `<main>`, footer (`daily-epub {VERSION}`), `/static/app.js` at the end.
- Every page template `{% extends "layout.html" %}` and receives a `Page` context: `title`, `viewer: Option<Viewer { username, role }>`, `flash: Option<Flash>`, `active_nav`.
- `Html<T: Template>` implements `IntoResponse`: renders, sets `text/html; charset=utf-8`, and on a render error logs and returns the 500 page.
- `WebError { NotFound, Forbidden, Unauthenticated { next }, BadRequest(String), Csrf, Db(sqlx::Error), Internal(anyhow::Error) }` → `IntoResponse`. `Unauthenticated` redirects HTML requests to `/login?next=<path>` and returns 401 JSON to `Accept: application/json`.
- Flash messages live in the tower-sessions session (`session.insert("flash", Flash { kind, text })`, removed when the next page reads it), so POST-redirect-GET can confirm "Saved 3 settings" or "Rated: Loved it". Only signed-in flows set flashes, so anonymous visitors never get a session cookie from this.
- Templates (all under `src/web/templates/`): `layout.html`, `error.html`, `login.html`, `account.html`, `issue_public.html`, `issue_full.html`, `issue_list.html`, `article.html`, `world.html`, `behind.html`, `dashboard/overview.html`, `dashboard/runs.html`, `dashboard/run.html`, `dashboard/articles.html`, `dashboard/article.html`, `dashboard/ratings.html`, `dashboard/profile.html`, `dashboard/stats.html`, `dashboard/settings.html`, `dashboard/settings_history.html`, `dashboard/jobs.html`, `dashboard/job.html`, `dashboard/users.html`, plus partials `_rating_widget.html`, `_pagination.html`, `_signals_table.html`, `_candidate_row.html`, `_sparkline.html`.
- Static assets: `include_str!` the three files; `GET /static/{file}` sets `Content-Type`, `Cache-Control: public, max-age=86400`, and an `ETag` of the sha256 (respond 304 to `If-None-Match`). The `?v={VERSION}` query busts caches across releases.

### 4.4 Design system (the one CSS file)

- Tokens: `--bg`, `--fg`, `--muted`, `--rule`, `--accent`, `--loved`, `--good`, `--down`, redefined under `@media (prefers-color-scheme: dark)`.
- Masthead and issue pages: Georgia/serif headings, `max-width: 72ch` reading column, hairline rules like the EPUB front page.
- Dashboard: system sans, `max-width: 1200px`, dense tables (`font-size: .9rem`, sticky `<thead>`), every `<table>` wrapped in `.scroll-x { overflow-x: auto }`.
- Components: `.badge` per stage/reason/label (`selected`, `shortlisted`, `assessed`, `admitted`, `triaged`, `eligible`, `excluded`, `loved`, `good`, `down`, `cleared`), `.kv` definition lists, `.funnel` (a horizontal bar per stage, widths proportional to counts), `.spark` (inline SVG polyline, server-rendered), `.rating` button group with the active verdict filled.
- No layout JavaScript. `app.js` does exactly: intercept `.rating` forms and `POST` with `fetch` + `Accept: application/json`, then update the button state; confirm dialogs on `data-confirm` forms (jobs, provider removal, clear rating); auto-refresh `dashboard/job.html` every 5 s while the status is `requested`/`running`; a "filter as you type" on tables with `data-filter` (client-side, current page only); collapse/expand for `<details>` state kept in `localStorage` (guard with try/catch).

---

## 5. Data model — migration `0004_web.sql`

Do not edit earlier migrations. Plain SQL, no Rust bootstrap.

### 5.1 Tables

```sql
CREATE TABLE users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT NOT NULL,                 -- PHC string from argon2
    role          TEXT NOT NULL CHECK (role IN ('user', 'admin')),
    disabled      INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL,
    last_login_at TEXT
);

-- tower-sessions records (§6.2). `id` is the session Id's string form (a random
-- 128-bit value the cookie carries), `data` the JSON-encoded record map.
-- `expiry` is unix seconds because tower-sessions hands us a `time::OffsetDateTime`
-- and the store compares it in SQL; `user_id` is denormalized from the
-- axum-login entry in `data` so the Users page and `users logout` can find a
-- user's sessions without decoding every row.
CREATE TABLE sessions (
    id         TEXT PRIMARY KEY,
    data       TEXT NOT NULL,
    expiry     INTEGER NOT NULL,
    user_id    INTEGER REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX idx_sessions_user ON sessions(user_id);
CREATE INDEX idx_sessions_expiry ON sessions(expiry);

ALTER TABLE rating_events ADD COLUMN user_id INTEGER REFERENCES users(id) ON DELETE SET NULL;

CREATE TABLE config_changes (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    INTEGER REFERENCES users(id) ON DELETE SET NULL,
    key        TEXT NOT NULL,                     -- dotted path, e.g. curation.ranking.deep_keep
    old_value  TEXT,                              -- TOML literal as it was in the file (NULL = absent)
    new_value  TEXT,                              -- NULL = removed
    changed_at TEXT NOT NULL
);
CREATE INDEX idx_config_changes_at ON config_changes(changed_at);

CREATE TABLE profile_versions (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    content  TEXT NOT NULL,                       -- the profile.md text *before* the save (and one row for the first save's original)
    saved_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    saved_at TEXT NOT NULL
);

CREATE TABLE jobs (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    name         TEXT NOT NULL,                   -- catalogue name, e.g. generate, dry-run, generate-2026-09-01
    unit         TEXT NOT NULL,                   -- daily-epub-job@<name>.service
    requested_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    requested_at TEXT NOT NULL,
    started_at   TEXT,
    finished_at  TEXT,
    status       TEXT NOT NULL CHECK (status IN ('requested', 'running', 'ok', 'failed')),
    message      TEXT,                            -- error text or one-line summary
    run_id       INTEGER REFERENCES runs(id) ON DELETE SET NULL
);
CREATE INDEX idx_jobs_requested_at ON jobs(requested_at);

ALTER TABLE runs ADD COLUMN report_json TEXT;
ALTER TABLE issues ADD COLUMN issue_json TEXT;
```

### 5.2 `issues.issue_json`

The full `Issue` the pipeline assembled, serialized with `serde_json`, **with every `pick.article.content_html` replaced by an empty string** (bodies are already in `articles.content_html`; the discussion threads and the World Briefing are the parts that exist nowhere else). Written by `pipeline::record_issue` alongside the lineup. A regenerated date overwrites it. Expected size: 100–300 KB per issue.

The web loader (`web::issue::load`) rehydrates each pick's body from `articles` by id. Issues published before this change have `issue_json IS NULL`; the loader then builds a reduced `Issue` from `issues` (`front_page_html` as the Brief, `issue_number`, `generated_at`), `issue_articles` (section, position, `is_lead`, `summary`, `why`) and `articles` — with `world_briefing = None`, `discussion = None`, `behind = Default`. Section order for the fallback is `config.curation.sections` order, then any unknown section names in first-seen order.

### 5.3 `runs.report_json` and `issues.report_json`

`db::finish_run` writes `serde_json::to_string(report)` into `runs.report_json`. `pipeline::record_issue` passes the same string as `report_json` to `upsert_issue` (fixing the never-written column; `GET /issues.json` starts returning real reports). `RunReport` already serializes; nothing in it is secret (`config_json` is `providers_redacted()`).

### 5.4 Job attribution and rating attribution

- `POST /rate` appends `RatingEvent { source: "dashboard", user_id: Some(viewer.id), .. }`. Extend `RatingEvent` with `pub user_id: Option<i64>` (default `None`; the `epub`/`cli` writers leave it `None`) and `db::append_rating_event` binds it. `db::current_ratings*` and `rated_article_from_row` gain the column so the ratings page can show who rated.
- `jobs.run_id` is set by `daily-epub job run` for the `generate*` jobs from `GenerateOutcome` (add `pub run_id: i64` to `GenerateOutcome`; `pipeline::generate` already has it as `run_id`).

---

## 6. Authentication and sessions

Sessions, login, logout and route guards come from **`axum-login` at the pinned git rev** (§2) over **`tower-sessions` 0.15** (the version it re-exports; use `axum_login::tower_sessions`, do not add `tower-sessions` separately). Passwords go through **`password-auth` 1.0**. The login throttle is **`tower_governor` 0.8**. What we write ourselves is limited to: the session store over our sqlx pool (§6.2), the `AuthnBackend`/`AuthzBackend` impl over the `users` table (§6.3), the origin check (§6.4), and the CLI.

### 6.1 Users and passwords (`src/web/users.rs`)

```rust
pub enum Role { User, Admin }              // as_str: "user" | "admin"; parse; Display
#[derive(Clone)]
pub struct User { id: i64, username: String, password_hash: String, role: Role, disabled: bool, created_at, last_login_at }
impl std::fmt::Debug for User { /* redact password_hash */ }
pub fn hash_password(plain: &str) -> String            // password_auth::generate_hash (argon2id, random salt, PHC string)
pub fn verify_password(hash: &str, plain: &str) -> bool // password_auth::verify_password(..).is_ok()
```

Rules: usernames are 1–32 chars of `[A-Za-z0-9._-]`, compared case-insensitively (`COLLATE NOCASE`); passwords are at least 12 characters and at most 1,024. `password_auth::verify_password` is a blocking argon2 computation; call it inside `tokio::task::spawn_blocking` from the backend.

CLI (`src/main.rs` + `web::users`), none of which take the run lock:

```text
daily-epub users add <username> [--admin] [--password-stdin]   # hidden double prompt (rpassword), or one line from stdin
daily-epub users passwd <username> [--password-stdin]          # also deletes the user's sessions (session_auth_hash changes anyway)
daily-epub users role <username> user|admin
daily-epub users disable <username> | enable <username>        # disable also deletes the user's sessions
daily-epub users list                                           # username · role · created · last login · open sessions
daily-epub users logout <username>                              # delete that user's sessions
```

`users add` refuses a duplicate name; `--admin` on the very first user is the intended bootstrap (§18).

### 6.2 Session store (`src/web/session.rs`)

```rust
#[derive(Clone, Debug)]
pub struct SqliteSessionStore { pool: SqlitePool }

#[async_trait]
impl tower_sessions::SessionStore for SqliteSessionStore {
    async fn save(&self, record: &Record) -> session_store::Result<()>;          // INSERT … ON CONFLICT(id) DO UPDATE SET data, expiry, user_id, updated_at
    async fn load(&self, id: &Id) -> session_store::Result<Option<Record>>;      // SELECT data FROM sessions WHERE id = ? AND expiry > strftime('%s','now')
    async fn delete(&self, id: &Id) -> session_store::Result<()>;
}
impl SqliteSessionStore {
    pub async fn delete_expired(&self) -> Result<u64>;                            // on server start and once an hour from a tokio task
    pub async fn delete_for_user(&self, user_id: i64) -> Result<u64>;             // users passwd/disable/logout, "sign out everywhere"
}
```

`data` is `serde_json::to_string(&record.data)`; `expiry` is `record.expiry_date.unix_timestamp()`. `user_id` is read best-effort from `record.data["axum-login.data"]["user_id"]` (the key axum-login uses) and stored alongside, so the Users page and the CLI can list and delete a user's sessions without decoding every row; a record without that key (an anonymous session that set flash data) stores `NULL`. Map every sqlx error to `session_store::Error::Backend(e.to_string())`. This is the whole store: ~60 lines plus tests. It mirrors what `tower-sessions-sqlx-store` does, which we cannot link because it pins sqlx 0.8 (§2).

Layer, built in `server::router`:

```rust
let session_layer = SessionManagerLayer::new(SqliteSessionStore::new(db.pool().clone()))
    .with_name("daily_session")
    .with_http_only(true)
    .with_same_site(SameSite::Lax)
    .with_secure(config.server.public_url.starts_with("https://"))
    .with_expiry(Expiry::OnInactivity(time::Duration::days(config.server.session_days as i64)));
let auth_layer = AuthManagerLayerBuilder::new(Backend::new(db.clone()), session_layer).build();
```

`Expiry::OnInactivity` is the sliding lifetime. tower-sessions only writes a cookie when a handler touches the session, so anonymous page views create no rows and no cookie. Flash messages (§4.3) use tower-sessions' own `Session` extractor (`session.insert("flash", ..).await` / `session.remove(..).await`), since `AuthSession` no longer exposes its inner session.

### 6.3 Backend (`src/web/session.rs`)

```rust
#[derive(Clone)] pub struct Backend { db: Db }
pub struct Credentials { username: String, password: String, next: Option<String> }

impl AuthUser for User {
    type Id = i64;
    fn id(&self) -> i64 { self.id }
    fn session_auth_hash(&self) -> &[u8] { self.password_hash.as_bytes() }   // password change ⇒ every session of the user is invalid
}
impl AuthnBackend for Backend {   // native async fn; no async-trait at this rev
    type User = User; type Credentials = Credentials; type Error = BackendError;
    async fn authenticate(&self, creds) -> Result<Option<User>> {
        // SELECT by username COLLATE NOCASE; verify against the row's hash, or against
        // a fixed dummy hash when the row is missing (equal timing); None when disabled.
    }
    async fn get_user(&self, id: &i64) -> Result<Option<User>> { /* None when disabled */ }
}
impl AuthzBackend for Backend {
    type Permission = Role;   // Hash + Eq + Clone
    async fn get_user_permissions(&self, user: &User) -> Result<HashSet<Role>> {
        Ok(match user.role { Role::Admin => [Role::User, Role::Admin].into(), Role::User => [Role::User].into() })
    }
}
pub type AuthSession = axum_login::AuthSession<Backend>;
```

Guards, applied as `route_layer` on the sub-routers (§3):

```rust
// any signed-in user: full issues, article/world/behind pages, /account
.route_layer(login_required!(Backend, login_url = "/login", redirect_field = "next"))
// admins: everything under /dashboard and POST /rate
.route_layer(permission_required!(Backend, login_url = "/login", redirect_field = "next", Role::Admin))
```

`permission_required!` redirects anonymous requests to `/login?next=…` and answers 403 to a signed-in user without the permission; the 403 is turned into the site's error page by a small `map_response` on the dashboard router. Pages that render differently for anonymous and signed-in viewers (`/`, `/issues/{date}`) take `AuthSession` and branch on `auth.user().await`. `/files/*` (§8) checks `auth.user().await.is_some()` before falling back to Basic auth.

`Viewer { user: User }` is derived from `auth.user().await` for templates; the layout shows the username and role from it.

Login: `GET /login?next=` renders the form. `POST /login` calls `auth.authenticate(creds)`; `Ok(Some(user))` → `auth.login(&user).await` (axum-login cycles the session id), `UPDATE users SET last_login_at`, redirect to `next` when it is a same-site absolute path (starts with `/`, not `//`), else `/`; `Ok(None)` → the form again with "invalid username or password" (status 401); `Err` → 500 page. `POST /logout` → `auth.logout().await`, redirect `/`. `/account` shows the viewer and offers "change password" (verify current, `hash_password`, update the row; the new `session_auth_hash` logs out every other session automatically) and "sign out everywhere" (`delete_for_user`).

### 6.4 CSRF: SameSite + origin check

The session cookie is `SameSite=Lax`, so browsers do not attach it to cross-site `POST`s at all. On top of that, one middleware (`web::session::require_same_origin`, applied to every `POST` route except `/r/*` which has no session) rejects with 403 when: `Sec-Fetch-Site` is present and is neither `same-origin` nor `none`; or, when it is absent, the `Origin` header (or the origin of `Referer`) does not equal the origin of `server.public_url` or of the request `Host`. That is the OWASP "fetch metadata + origin verification" defence and is enough for cookie-authenticated forms; there is no per-form token to render or check. `app.js` `fetch` calls send `credentials: "same-origin"` and get `Sec-Fetch-Site: same-origin` from the browser automatically.

If a per-form token is ever wanted (e.g. for a very old browser without fetch metadata), add `axum_csrf` 0.11 as a second layer; nothing in this plan precludes it.

### 6.5 Login throttle

`tower_governor` on the `POST /login` route only:

```rust
let governor = Arc::new(GovernorConfigBuilder::default()
    .per_second(config.server.login_window_minutes * 60 / config.server.login_attempts)   // e.g. 90 s per token
    .burst_size(config.server.login_attempts)                                            // e.g. 10
    .key_extractor(SmartIpKeyExtractor)
    .finish().expect("governor config"));
// route_layer(GovernorLayer::new(governor.clone())) on POST /login; a tokio task calls
// governor.limiter().retain_recent() every 60 s.
```

`SmartIpKeyExtractor` trusts `X-Forwarded-For`; that is acceptable because `server.bind` is loopback and nginx is the only peer (README §"Reverse proxy" says so; add a sentence). `server::serve` binds with `into_make_service_with_connect_info::<SocketAddr>()` so the peer address is available as the fallback key. Over the limit the layer answers 429; the login page template is not involved. Note the limiter counts attempts, not failures — ten login POSTs per fifteen minutes per IP is plenty for one operator.

## 7. Public site

### 7.1 `PublicIssue` — the only thing the public templates receive

```rust
pub struct PublicIssue {
    pub date: Date, pub issue_number: i64, pub display_date: String,
    pub article_count: i64, pub reading_minutes: i64,
    pub sections: Vec<PublicSection>,
    pub generated_at: Timestamp,
}
pub struct PublicSection { pub name: String, pub entries: Vec<PublicEntry> }
pub struct PublicEntry {
    pub title: String, pub url: String,           // canonical_url; the link target
    pub author: Option<String>,
    pub source: String,                            // feed_title
    pub domain: String,                            // host of `url`, "www." stripped
    pub reading_minutes: i64, pub word_count: i64,
    pub comment_links: Vec<CommentLink>,           // {label, url, meta}
    pub is_lead: bool,
}
```

`PublicIssue::from(&Issue)` is the only constructor, and it copies exactly these fields. There is deliberately no `summary`, `why`, `body`, `brief`, or `world` field, so the template cannot render them. Test `public_issue_carries_no_generated_text` (§17) renders the fixture issue publicly and asserts that the Brief, every summary, every `why`, every article body sentence and every comment string are absent from the HTML.

`comment_links`: one per `SocialRef` with an `item_url` — label `Hacker News` / `Lobsters` / `Reddit`, meta `"342 points · 210 comments"` — plus `Comments` for `entries.comments_url` when it is set and differs from the article URL. Links carry `rel="noopener"` and `target="_blank"`; article title links are plain `<a href>` (the aggregator's purpose is to send readers to the source, so no `nofollow`).

### 7.2 Pages

- `GET /` — the latest issue by date, rendered as `issue_public.html` (or `issue_full.html` when signed in, §8), with a strap line ("A personal morning paper, assembled daily from N feeds; the selection is the reader's, the words are the authors'") and a link to the archive. When no issue exists: an empty-state page.
- `GET /issues` — every issue date, newest first, grouped by month: date, weekday, `issue_number`, article count. Anonymous and signed-in alike.
- `GET /issues/{date}` — `issue_public.html`: masthead, dateline `Friday, September 3, 2026 · No. 20`, `stats_line` (articles · read time · sections), then sections in `Lineup.section_order` with entries: title → source link, `author · source (domain) · 8 min`, comment links. The World Briefing is **not** listed publicly (its entries are Wikipedia text). The lead story is marked with `.lead`.
- `GET /feed.xml` — Atom 1.0, last 30 issues: `<id>tag:daily.hallada.net,2026:issue/{date}</id>` (host from `server.public_url`), `<title>The Daily EPUB — {date}</title>`, `<updated>` = `generated_at`, `<link rel="alternate" href="{public_url}/issues/{date}">`, `<content type="html">` = the public section list as escaped HTML (the same `PublicIssue`, rendered by `feed_entry.html`). `Content-Type: application/atom+xml; charset=utf-8`, `Cache-Control: public, max-age=300`.
- `GET /robots.txt` — as in §3.

Public pages send `Cache-Control: public, max-age=300` only when no session cookie is present; with a cookie they are `private, no-store`.

---

## 8. Full issue views for signed-in users

`web::issue::load(state, date) -> Result<Option<IssueView>>` (§5.2). `IssueView { issue: Issue, downloads: Vec<Download>, from_json: bool }` where `Download { label, href, size_bytes }` lists the standard EPUB, the X4 EPUB and the XTC file **only when the file at `issues.*_path` (or `publish::issue_filename` in `epub_dir`) exists**; `href` is the existing `/files/epub/{name}` or `/files/xtc/{name}`.

- `GET /issues/{date}` (signed in) — `issue_full.html`: masthead and dateline; **The Brief** (`editorial.front_page_html`, already sanitized XHTML, rendered `|safe`); download buttons; the index exactly like the EPUB's In-this-issue page: per section, title (→ `/issues/{date}/articles/{id}`), `source · N min read`, summary, `Why it's here`, the rating widget for admins; then links to World Briefing and Behind the paper; the colophon facts (models, cost, counts) in a footer block.
- `GET /issues/{date}/articles/{article_id}` — `article.html`: `article-header` (title linking to the source, byline, meta line `feed · 1,850 words · ~8 min`, `Why it's here`, social line via `chapters::social_line`, summary), the body (`ammonia::clean(articles.content_html)` — reuse the same ammonia configuration the EPUB uses; do **not** run `to_xhtml`; `<img>` tags keep their remote `src` and get `loading="lazy" referrerpolicy="no-referrer"`), the discussion (`comments::render_xhtml(discussion, title)` when `pick.discussion` is present; it is XHTML and renders fine as HTML), prev/next links in issue order, "Read online ↗", and the rating widget for admins. 404 when the article is not in that issue.
- `GET /issues/{date}/world` — `world::render_xhtml` in the layout; 404 when the issue has no briefing (fallback issues).
- `GET /issues/{date}/behind` — the same lines as the EPUB chapter (`chapters::behind_*_line`, near misses linking to `/dashboard/articles/{id}` for admins).

`/files/epub/{name}` and `/files/xtc/{name}`: `serve_file` accepts either a valid session (any role) **or** the existing Basic auth; when neither is present and Basic auth is configured, it challenges as today; when Basic auth is not configured it redirects HTML clients to `/login?next=` and returns 401 to others. OPDS clients are unaffected.

The HMAC confirmation page (`server::confirmation_page`) gains one line: `<a href="{public_url}/issues/{date}">Open this issue on the site</a>`.

### 8.1 Rating widget (`_rating_widget.html`, `web::rate`)

A `<form class="rating" method="post" action="/rate">` with hidden `article_id`, `issue_date`, `next`, and four buttons `label=loved|good|down|cleared` (the last styled as a link "clear"); the current verdict button is filled. An optional `note` text field appears on the dashboard variants (`/dashboard/articles/{id}`, `/dashboard/ratings`), not on issue pages. `POST /rate`:

1. Admin route layer (§6.3) and the origin check (§6.4) have already passed; read the viewer from `AuthSession`.
2. Validate `article_id` exists; `issue_date` optional (issue pages pass it; the dashboard passes the latest `issue_articles` date for the article, if any, like the CLI does).
3. Append `RatingEvent { kind: "explicit", source: "dashboard", label, value: vote.value(&cfg.curation.feedback) (0.0 for cleared), note, user_id, event_at: now }`.
4. `Accept: application/json` → `200 {"article_id", "label", "event_id"}`; otherwise set a flash and `303 See Other` to `next` (validated same-site path).

---

## 9. Dashboard: overview, runs, articles

All dashboard queries live in `src/web/dashboard/*.rs` as runtime `sqlx` queries with manual row mapping. Page size 50 (articles) / 100 (candidates), `?page=N`, `_pagination.html`. Sort and filter parameters are query-string, validated against allow-lists (never interpolated into SQL).

### 9.1 Overview (`/dashboard`)

- **Last run** card: date, status badge, started/duration, the four-line block (`curation:`, `admission:`, `preference:`, `providers:`) built from `runs.report_json` (fall back to the counters when NULL), warnings count with a link to the run.
- **Budget today** card: per provider (from `config.referenced_providers()` + voyage): spent today (`db::provider_spend_for_utc_day(now)` + the last run if it is today) vs `max_daily_usd`, as a bar.
- **Sparklines** (server-rendered SVG, last 30 non-dry runs): cost per run, selected per run, generation seconds.
- **Ratings this week**: counts by label, and **"Unrated picks"** — the last three issues' picks with no explicit event, title + feed + inline rating widget. (The operator forgets to rate; this is the nudge.)
- **Jobs**: any `requested`/`running` job, and the last five finished.
- **Config on disk**: the `Config::check_report` lines that start with `! ` (missing keys/files), if any.

### 9.2 Runs (`/dashboard/runs`, `/dashboard/runs/{id}`)

List: one row per `runs` row, newest first, 50 per page: id, date (→ issue page), status badge, started (config tz), duration, `considered → eligible → triaged → assessed → shortlisted → selected` from `report_json.counts` (or `candidates`/`selected` when NULL), cost per provider, total, dry-run marker, warnings count. Filter `?status=`.

Detail:

1. Header: date, status, started/finished/duration, link to the issue, link to the previous and next run of that date.
2. **Funnel**: `SELECT stage, excluded_reason, COUNT(*) FROM candidate_runs WHERE run_id = ? GROUP BY 1, 2` rendered as bars in pipeline order (`excluded` → `eligible` → `triaged` → `admitted` → `assessed` → `shortlisted` → `selected`) with the reason breakdown under each.
3. **Admission mix** (`report_json.counts.admitted_by`), **preference state** (`rated_with_embeddings`, `knn_gate`, `feed_gate`, `verdicts_in_prompt`), **timings** table (`report_json.timings`), **provider usage** (tokens in/cached/write/out, cost), **warnings** list, **per-feed counts** top 20.
4. **Config diff**: flatten `runs.config_json` of this run and of the previous non-dry run into dotted keys; show keys whose values differ (`key · before → after`). This is how a tuning change is tied to its first paper.
5. **Near misses**: `telemetry::near_misses(db, run_id, 10)`.
6. **Candidates** table: every `candidate_runs` row joined to `articles`/`entries`/`article_assessments`: title (→ article detail), feed, words, stage badge, reason, `admitted_by`, utility, rank, cluster (`id · rank`), triage `interest`, deep `quality`/`fit`, exploration/auto-include flags, `editor_why`. Filters: `stage`, `reason`, `admitted_by` (prefix match on the JSON array's first element via `json_extract(admitted_by, '$[0]')`), `q` (title LIKE), `flag=exploration|auto`. Sort: utility desc (default), rank, triage, quality, fit, title. A `<details>` per row expands `_signals_table.html` (raw · norm · weight · present for each signal, top interests, neighbours, notes) from `signals_json`.

### 9.3 Articles (`/dashboard/articles`, `/dashboard/articles/{id}`)

List: `articles` LEFT JOIN `entries` (best entry) LEFT JOIN the article's **latest** `candidate_runs` row (`MAX(run_id)` subquery) LEFT JOIN triage/deep `article_assessments` LEFT JOIN the current rating (latest explicit event) LEFT JOIN the latest `issue_articles` row. Columns: first seen, title, feed, words, last stage/reason (with run date), utility, triage, quality/fit, rating badge, published date. Filters: `q` (title or canonical_url LIKE), `feed` (feed_id), `stage`, `reason`, `rated=any|loved|good|down|cleared|none`, `published=yes|no`, `from`/`to` (first_seen dates), `kind` (triage kind). Sort: first_seen desc (default), utility, quality, fit, triage, words, title. Add index `CREATE INDEX idx_candidate_runs_article_run ON candidate_runs(article_id, run_id DESC)` in the migration so the latest-row join stays fast.

Detail — everything the system knows, in this order:

1. **Article**: title → source, canonical URL, feed (+ category), author, published, first seen, words, excerpt-only, image count, extract method (from `sources_json`? no — from the best entry; show what `Article` carries), sources list (`SourceRef` kind + feed), social refs with links and scores, "in issues" (date · section · position · lead), current rating badge, the rating widget with note field, "Explain (text)" `<details>` containing `telemetry::render_explain` for the latest run row, verbatim in `<pre>`.
2. **Assessments**: triage (interest, kind, why, model, prompt version, assessed_at) and deep (quality, fit, category, rationale, paywalled guess, facets as a table, model, assessed_at); a `provider_rejected` row is shown as such.
3. **Run history**: every `candidate_runs` row for the article, newest first: run (→ run detail), date, stage, reason, admitted_by, utility, rank, cluster, editor why. Clicking a row expands its `_signals_table.html`.
4. **Neighbours and interests** from the latest row's `signals_json`: top interests (z, cos) and nearest rated neighbours (label, cos, → their article page).
5. **Embedding**: model, dimension, created_at, input hash — never the vector.
6. **Rating events**: all events for this article (including superseded ones), with source, user, note, value.

---

## 10. Ratings page (`/dashboard/ratings`)

Two tabs (query `?tab=current|events`).

**Header** (from `PreferenceState::load(db, &config.curation.ranking, now).summary()` — reuse whatever the pipeline calls): `N rated articles with embeddings → neighbour signal at {knn_gate:.0%} (floor {knn_floor}, full {knn_full}) · {attributable_feed_ratings} attributable feed ratings → feed affinity at {feed_gate:.0%} · {verdicts_in_prompt} verdicts in the prompt · {n} in the weekly rebuild set`. When a rated article has no embedding, a notice: "M rated articles have no embedding and cannot act as neighbours — run the `features backfill --rated-only` job."

**Current** tab: `db::current_ratings_including_cleared(36_500)`, one row per article, newest first, filter by label/source/feed/q. Columns:

| Column | Definition |
|---|---|
| Verdict | badge; the rating widget inline (with note) to change it |
| Article | title → article detail, feed, issue date if any |
| When · by | `event_at` (config tz), `source`, username |
| Note | `note` |
| Age → decay | `age_days` and `decay = 0.5^(age/half_life)` (`signals::decay`) |
| Neighbour weight | `value × decay` when the article has an embedding; "no embedding" otherwise. This is exactly the `w_i` of plan §9.2 |
| Feed credit | each direct feed (`signals::direct_feeds`, else the best entry's feed) and `value / n` credited to it (plan §9.3) |
| In prompt | ✓ when its rank among current non-cleared verdicts (newest first) is `< curation.feedback.verdicts_in_prompt` |
| In rebuild | ✓ when that rank is `< profile::MAX_RATINGS_IN_REBUILD` |
| Used last run | from the latest non-dry run: the number of candidates whose `signals_json.neighbours` include this article id (`WHERE signals_json LIKE '%"article_id":<id>,%'` then parsed to confirm), and how many of those were selected |

A "How ratings enter the algorithm" `<details>` block at the top explains the four paths in prose (prompt verdict block, weekly learned adjustments, neighbour signal with decay and gate, feed affinity with gate) with the current config values inlined, and links to the settings groups that tune them.

**Events** tab: every `rating_events` row, newest first, 100 per page, with `superseded` marked when a later explicit event exists for the same article; filters by label, source, user, date range. Nothing here is editable — history is append-only, and the "clear" verdict is the way to retract.

---

## 11. Profile page (`/dashboard/profile`)

- **profile.md editor**: a monospace `<textarea>` with the current file (`config.profile_path`), a Save button (an origin-checked POST, §6.4), and a live preview of what the loader parses: the sections that pass through verbatim and the `## Interests` lines it extracted (`profile::parse_profile_str`). Saving: validate the text parses (it always does; reject only empty or > 64 KB), insert the **previous** content into `profile_versions`, write atomically (`<path>.tmp` + rename, preserve mode), flash "Saved; the next run rebuilds the system prompt". The server unit needs the profile's directory writable (it is under `/var/lib/daily-epub`, the `StateDirectory`; §18 confirms).
- **History**: `profile_versions` newest first with a diff-less preview (first 200 chars) and a "Restore" button (writes that content back, recording the current one as a new version).
- **Standing interests**: the OPML interests (`profile::parse_interests`) grouped by `themes::group_into_themes` — read-only, with the file path and count; a note that the union with `## Interests` is what the prompt uses.
- **System prompt**: `kv.taste_profile` (`TasteProfile { text, version, built_at, verdicts }`) in a collapsed `<pre>`, the learned adjustments block (`kv.taste_profile_learned`) shown separately with its age and whether a weekly rebuild is due (`profile::is_stale`), and a "Rebuild profile now" button that starts the `profile-rebuild` job (§14).

---

## 12. Stats page (`/dashboard/stats`)

Refactor `telemetry::stats(db, days, now) -> String` into `telemetry::stats_data(db, days, now) -> StatsData` plus `telemetry::render_stats_text(&StatsData) -> String`; the CLI output is byte-identical (a test pins it against the current fixture). `StatsData` carries: issues, articles published, ratings by label, ratings per issue, per-retriever up/down, exploration yield, mean issue size, cost per day per provider (as a date → provider → usd map), mean generation time, and the per-run series used by the sparklines.

The page: `?days=14|30|90`, the same figures as tables, three sparklines (cost per day stacked per provider as bars, selected per issue, ratings per week by label), and the retriever yield as a small table with the up/down ratio.

---

## 13. Settings (`/dashboard/settings`)

### 13.1 The schema is derived, not hand-listed

`settings::schema(config: &Config, file: Option<&toml_edit::DocumentMut>) -> Vec<SettingGroup>`:

1. `let defaults = toml::Value::try_from(Config::default())` and `let current = toml::Value::try_from(config)`; walk both in parallel, depth-first, producing one `SettingField` per leaf:

```rust
pub struct SettingField {
    pub path: String,                  // "curation.ranking.deep_keep"
    pub group: String,                 // "curation.ranking" (the enclosing table)
    pub kind: FieldKind,               // Bool | Integer | Float | Text | TextList | Enum(&'static [&'static str]) | Secret | Path
    pub current: String,               // rendered value (TextList: one per line)
    pub default: String,
    pub source: Source,                // Default | File | Env(String /* the var name */)
    pub help: Option<&'static str>,
    pub restart_required: bool,        // server.bind, database_path
}
```

2. `source`: `Env` when `std::env::var_os(env_name(path))` is set, where `env_name` = `ENV_PREFIX + path.to_uppercase().replace('.', ENV_SPLIT)` (`DAILY_EPUB_SECRET` also counts for `server.hmac_secret`); else `File` when the key exists in the parsed document; else `Default`.
3. `kind`: from the default's TOML type; `TextList` for arrays of strings; `Enum` from a static table keyed by path suffix — `providers.*.kind` → `openai|anthropic`, `providers.*.effort` → `low|medium|high|xhigh|max` (anthropic) or free text (openai), `editorial.summary_model` → `editor|bulk`, `xtc.format` → `xtc|xtch`, `llm.bulk`/`llm.editor` → the provider names (plus `""` for editor); `Secret` for paths ending in `api_key`, `hmac_secret`, `basic_auth_pass`; `Path` for `database_path`, `out_dir`, `profile_path`, `interests_opml`, `publish.*_dir`, `xtc.settings`.
4. `help`: a static `SETTINGS_HELP: &[(&str, &str)]` seeded from the README's configuration table and the comments in `config.example.toml`. Every key in the shipped example must have an entry (test).
5. Groups render in this order: top level, `llm`, `providers.<name>` (one card each), `voyage`, `curation`, `curation.feedback`, `curation.ranking`, `curation.ranking.quotas`, `curation.ranking.weights.preliminary`, `curation.ranking.weights.utility`, `curation.ranking.diversity`, `editorial`, `publish`, `xtc`, `server`, `miniflux`.

`Secret` fields render as "set (from `DAILY_EPUB_…`)" / "not set" with no input. `Env` fields render disabled with "locked by `DAILY_EPUB_…` in the env file". Everything else is an input of its kind, showing the default beside it and a "reset to default" affordance. Weights render with a note that they are renormalized.

### 13.2 Saving

`POST /dashboard/settings` (admin, origin-checked) carries every editable field. The handler:

1. `reload_if_changed`; load `config_path` into `toml_edit::DocumentMut` (a missing file starts from an empty document; `config_path == None` → 400 "no config file is configured; start the server with --config").
2. For each posted field whose parsed value differs from the current **file** value (compare TOML values; a field equal to the default but absent from the file is still written when the user submitted a change to it — "explicit beats implicit"; a field unchanged from what the file holds is skipped): set it in the document at its path, creating intermediate tables as implicit tables; typed by `FieldKind` (integers → `i64`, floats → `f64` formatted with at most 6 decimals, bools, strings, `TextList` → a multi-line array with one item per line, trailing comma). Parsing failures (letters in a number, unknown enum value) collect into a field-error list and abort before writing.
3. Render the document to a string, write to `<path>.tmp.<pid>`, and validate by `Config::load(Some(&tmp))` (the same loader the CLI uses, env included). On error: remove the temp file, re-render the form with the message. On success: copy the original file's permissions onto the temp file, `rename` it over the original, and swap `state.config`.
4. Insert one `config_changes` row per changed key with the TOML literal before/after (`NULL` when absent/removed) and the viewer's id; flash "Saved N settings — they apply to the next run" (plus "restart the server for: …" when a `restart_required` key changed).

Because `generate` runs as its own process and reads `config.toml` at start, a saved setting reaches the next paper with no further plumbing. `runs.config_json` continues to snapshot what each run actually used, and the run page's config diff (§9.2) shows it.

### 13.3 Providers

The `providers.*` cards each have a "Remove" button (`POST /dashboard/settings/providers` with `action=remove&name=`; refused when an `[llm]` role names it) and the page has an "Add provider" form (`action=add&name=&kind=`; name validated as `[a-z0-9_]+`, must not exist). Adding inserts a `[providers.<name>]` table whose keys are `ProviderConfig::default()` with the chosen `kind` and a `base_url`/`model` placeholder, via `toml::to_string` → parsed into `toml_edit::Item`. Both go through the same validate-and-rename path and log `config_changes` rows (`providers.<name>` with the whole table as the literal).

### 13.4 History

`GET /dashboard/settings/history`: `config_changes` newest first, 100 per page — when, who, key, before → after. Linked from the settings page and from the run page's config diff.

---

## 14. Jobs

### 14.1 Catalogue (`src/jobs.rs`)

```rust
pub enum Job {
    Generate { date: Option<Date> },   // "generate" | "generate-YYYY-MM-DD"
    DryRun,                            // "dry-run"           → generate --dry-run
    ProfileRebuild,                    // "profile-rebuild"   → profile rebuild
    FeaturesBackfill,                  // "features-backfill" → features backfill --days 30 --yes
    BackfillSocial,                    // "backfill-social"   → backfill-social --days 7
    FeaturesPrune,                     // "features-prune"    → features prune
}
impl Job {
    pub fn parse(name: &str) -> Option<Job>;      // names match ^[a-z0-9-]+$ and the forms above
    pub fn name(&self) -> String;
    pub fn unit(&self) -> String;                  // format!("daily-epub-job@{}.service", name)
    pub fn description(&self) -> &'static str;    // shown on the Jobs page
    pub fn takes_lock(&self) -> Option<&'static str>; // same names lock_holder uses
    pub fn dangerous(&self) -> bool;               // Generate (republishes an issue) → confirm dialog
}
```

### 14.2 `daily-epub job run <name>`

A new subcommand in `main.rs`. It:

1. Parses the name (unknown → exit 2 with the catalogue).
2. Takes the run lock when `takes_lock()` says so (via `lock_holder`, so "generate is already running" behaves as today).
3. Finds the newest `jobs` row with this `name` and `status = 'requested'`; if none, inserts one (`requested_by NULL` — the operator started the unit by hand). Sets `running` + `started_at`.
4. Runs the mapped command **in-process** by calling the same functions `main` uses (`pipeline::generate`, `cmd_profile_rebuild`, `cmd_features`, `cmd_backfill_social`, `telemetry::prune`), with the same logging.
5. On success sets `ok`, `finished_at`, `run_id` (for generate) and a one-line `message` (the `curation:` line for generate; counts otherwise). On error sets `failed` and `message = error`, exits non-zero so systemd records `Result=exit-code`.

### 14.3 Unit, polkit, groups

`systemd/daily-epub-job@.service` — a copy of `daily-epub-generate.service` with `Description=The Daily EPUB job %i` and `ExecStart=/usr/local/bin/daily-epub --config /etc/daily-epub/config.toml job run %i`. Same hardening, same `ReadWritePaths`, `TimeoutStartSec=45min`. The timer keeps driving `daily-epub-generate.service` unchanged.

`systemd/50-daily-epub.rules` (installed to `/etc/polkit-1/rules.d/`):

```js
polkit.addRule(function (action, subject) {
  if (action.id == "org.freedesktop.systemd1.manage-units" &&
      subject.user == "daily-epub" &&
      action.lookup("verb") == "start" &&
      /^daily-epub-job@[a-z0-9-]+\.service$/.test(action.lookup("unit"))) {
    return polkit.Result.YES;
  }
});
```

`systemd/daily-epub.service` gains `SupplementaryGroups=systemd-journal` (journal reads) and `ReadWritePaths=/etc/daily-epub` (settings writes). Nothing else in the hardening changes; `systemctl` talks to systemd over the system bus (`AF_UNIX`, already allowed).

### 14.4 Runner and page

```rust
#[async_trait] pub trait JobRunner: Send + Sync {
    async fn start(&self, unit: &str) -> Result<(), String>;                 // systemctl start --no-block <unit>
    async fn status(&self, unit: &str) -> Result<UnitStatus, String>;        // systemctl show -p ActiveState,SubState,Result,ExecMainStatus,ExecMainStartTimestamp,ExecMainExitTimestamp
    async fn log(&self, unit: &str, lines: usize) -> Result<String, String>; // journalctl -u <unit> -n <lines> --no-pager -o short-iso
}
```

`SystemdRunner` spawns the commands with `tokio::process::Command` (10 s timeout, stderr captured into the error). `MockRunner` records calls and returns scripted statuses for tests. `server.jobs_enabled = false` swaps in a `DisabledRunner` whose page says so.

`GET /dashboard/jobs`: the catalogue as cards (name, description, Start button with `data-confirm` for `dangerous()`; the `generate-<date>` card has a date input), then the jobs table newest first (name, requested by/at, started, finished, duration, status badge, message, run → run detail).

`POST /dashboard/jobs/{name}` (admin, origin-checked): parse; refuse when a job with the same unit is `requested`/`running` (409 with a flash); insert `requested`; `runner.start(unit)`; on error mark the row `failed` with the message; redirect to `/dashboard/jobs/{id}`.

`GET /dashboard/jobs/{id}`: the row, the live `UnitStatus`, and the journal tail (`server.journal_lines`, 300) in a `<pre>`. If the row is still `requested` but the unit is `inactive` with `Result=exit-code`/`failed` 30 s after `requested_at`, mark it `failed` ("unit exited before the job started; see the log"). `app.js` refreshes while `requested`/`running`.

---

## 15. Configuration additions

```toml
[server]
bind = "127.0.0.1:3499"
public_url = "https://daily.hallada.net"
session_days = 30                    # web login lifetime, sliding
login_attempts = 10                  # failed logins per IP per window before a 429
login_window_minutes = 15
jobs_enabled = true                  # the Jobs page starts daily-epub-job@<name>.service via systemctl
journal_lines = 300                  # log tail shown on a job page
# hmac_secret / basic_auth_* unchanged
```

Validation: `session_days ≥ 1`, `login_attempts ≥ 1`, `login_window_minutes ≥ 1`, `journal_lines ∈ 10..=5000`. Update `config.example.toml`, `Config::default()` (the key-for-key test enforces both), the README table, and `SETTINGS_HELP`. These keys appear on the settings page automatically.

New dependencies: `axum-login = { git = "https://github.com/maxcountryman/axum-login.git", rev = "151c72d7a1b4646830f86b4332e6bd6e34d719a7" }` (brings `tower-sessions` 0.15 and `tower-cookies`; use the re-exports, do not add `tower-sessions` separately — a second copy would be a different version; the server builds from source with GitHub access, as it does today), `password-auth`, `tower_governor` with the `axum` feature, `time` (the type tower-sessions' `Expiry` and `Record` use; no `formatting` feature needed since the store keeps unix seconds), `async-trait` (only for our `SessionStore` impl — tower-sessions' trait still uses it; axum-login's own traits no longer do), `rpassword`, `toml_edit`, `toml` (already transitive; promote to direct for `toml::Value`), dev: `tower` with `util` (router `oneshot` tests). Do **not** add `argon2` or `tower-sessions-sqlx-store` (§2). Confirm the sqlx SQLite build has `json_extract` (it does; bundled SQLite ships JSON1).

---

## 16. Security checklist

- Password hashes: `password-auth` argon2id PHC strings; `User`'s `Debug` redacts the hash; never logged. Session ids are tower-sessions' random ids stored server-side; the cookie is `HttpOnly`, `Secure` on https, `SameSite=Lax`; login cycles the id; a password change or `disable` invalidates the user's sessions (`session_auth_hash` + `delete_for_user`).
- CSRF: `SameSite=Lax` plus the fetch-metadata/origin middleware on every POST (§6.4). Redirect targets validated to same-site paths.
- Login: `tower_governor` per-IP limit on `POST /login` (§6.5); equal-timing dummy verification for unknown users; no username enumeration in messages ("invalid username or password").
- Authorization: `login_required!`/`permission_required!` route layers on whole sub-routers (§6.3), so a forgotten check in one handler still redirects or 403s; `POST /rate` sits under the admin layer.
- Headers on every app response (via a `tower-http` `SetResponseHeader` layer or a small middleware): `Content-Security-Policy: default-src 'self'; img-src * data:; style-src 'self'; script-src 'self'; frame-ancestors 'none'; form-action 'self'`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: strict-origin-when-cross-origin`. (`img-src *` because article pages show remote images for signed-in users.) nginx keeps adding its own.
- All user text is escaped by askama; the only `|safe` inputs are `front_page_html` (already sanitized by the pipeline), `ammonia::clean` output, `comments::render_xhtml` and `world::render_xhtml` (built from sanitized comment/portal HTML), and the server-rendered SVG sparklines.
- Settings: secrets never rendered, never written; validation through `Config::load` before rename; permissions preserved; every change attributed. `Path` fields are written as given — the operator is the admin, and the config validates paths on load.
- Jobs: unit names come only from the fixed catalogue regex; the polkit rule allows `start` only, on that regex only, for that user only.
- Files: `safe_join` unchanged. `robots.txt` disallows private paths. Public caching only without a cookie.
- Logs: never log passwords, tokens, cookies, or `Authorization`. The `TraceLayer` stays; add a request-id.

---

## 17. Tests

No test touches the network or systemd. Router-level tests use `tower::ServiceExt::oneshot` against `server::router(state)` with a temp database (`Db::open_and_migrate` on a `tempfile` path), a `MockRunner`, and `fixtures::issue()` written through `record_issue`'s helpers.

- **Migration**: temp DB through `0001`…`0004`; the new tables exist; `rating_events.user_id` present; old rows keep `user_id NULL`.
- **Users/passwords**: hash round-trips through `password-auth`; wrong password fails; malformed hash fails safely; `users add` rejects short passwords and duplicate names (case-insensitive); `role`, `disable`, `logout`, `passwd` behave and the last three delete sessions.
- **Session store**: `save`/`load`/`delete` round-trip a `Record`; `load` returns `None` past `expiry`; `user_id` is denormalized from the axum-login key and `NULL` otherwise; `delete_expired` and `delete_for_user` count correctly; a store error surfaces as `session_store::Error::Backend`.
- **Login flow (router oneshot)**: `POST /login` sets `daily_session` with `HttpOnly`, `SameSite=Lax`, and `Secure` iff the public URL is https; the cookie then reaches `/account`; a disabled user's cookie does not; a password change logs the other session out; logout clears; anonymous `GET /` sets no cookie at all; `next` is honoured only for same-site paths.
- **Guards**: anonymous `/dashboard` → 302 `/login?next=/dashboard`; signed-in `user` role → 403 site page; admin → 200; `POST /rate` follows the same three outcomes.
- **Origin check**: POST with `Sec-Fetch-Site: cross-site` → 403; absent fetch metadata and a foreign `Origin` → 403; same-origin passes; the HMAC rating route (GET) is untouched.
- **Throttle**: with a test governor config of burst 3, the fourth `POST /login` from one address → 429; another address still passes.
- **Public rendering**: `public_issue_carries_no_generated_text` (§7.1); comment links built from social refs and `comments_url`; the World Briefing is absent publicly; the feed validates as XML, has one entry per issue, and its content is the public list; `robots.txt` content; `Cache-Control` public without a cookie, `no-store` with one.
- **Full rendering**: signed-in `/issues/{date}` contains the Brief, summaries and `why`; article page contains the body and the discussion; the fallback loader (no `issue_json`) renders an issue from rows with no world/discussion links; downloads listed only for files that exist; `/files/epub` accepts a session, still challenges Basic auth when configured and no session.
- **issue_json**: `record_issue` writes it with empty bodies; the loader rehydrates bodies; `runs.report_json` and `issues.report_json` written; `/issues.json` returns real reports.
- **Rating**: `POST /rate` as admin appends a `dashboard` event with `user_id`; as user → 403; anonymous → redirect/401; `cleared` writes value 0; JSON and form variants; `next` validated.
- **Dashboard queries**: funnel counts match a seeded `candidate_runs` set; candidate filters and sorts are allow-listed (an unknown sort falls back, never errors); config diff finds changed dotted keys and ignores unchanged; articles list filters; article detail shows assessments, run history and rating events.
- **Ratings contributions**: decay/weight/feed-credit/in-prompt/in-rebuild computed against hand-checked values; "used last run" counts neighbours in `signals_json`.
- **Profile**: save writes the file and a `profile_versions` row; restore swaps; oversized rejected; the parsed preview matches `parse_profile_str`.
- **Settings schema**: every leaf of `Config::default()` appears exactly once; every shipped key has help text; secrets are `Secret` and never carry a value; env detection uses the derived name; enum options match `validate()`'s accepted values (a round-trip test over each option).
- **Settings writer**: starting from `config.example.toml`, changing three keys preserves every comment and the original key order (string compare of untouched lines); a new key in an absent table creates the table; `TextList` writes a multi-line array; an invalid value (e.g. `deep_keep < shortlist_keep`) is rejected by `Config::load` and the file is untouched; permissions preserved; `config_changes` rows written; provider add/remove; removing a referenced provider refused.
- **Stats refactor**: `render_stats_text(stats_data(..))` equals the previous `stats(..)` output for a seeded DB.
- **Jobs**: `Job::parse` accepts the catalogue and dated form, rejects `../x`, uppercase, and unknown names; `POST /dashboard/jobs/{name}` inserts a row and calls `MockRunner::start` with the right unit; a running duplicate is refused; a failed start marks the row; `job run` (in-process, `--skip-llm`-style config, mocked providers) flips `requested → running → ok` and sets `run_id`; the polkit rule file is present in the repo and matches the unit regex (string test).
- **Binary** (`tests/m7_server.rs` style): the shipped binary serves `/`, `/issues`, `/feed.xml`, `/login` from env-only config; `/dashboard` redirects to `/login`; `users add` then a real login over TCP works.

---

## 18. Deployment — `docs/runbooks/web-dashboard-rollout.md`

Write this runbook as part of step 7; its content:

1. Build and install the binary; `daily-epub --help` shows `users`, `job`.
2. `sudo install -m0644 systemd/daily-epub-job@.service /etc/systemd/system/`; `sudo install -m0644 systemd/50-daily-epub.rules /etc/polkit-1/rules.d/`; update `/etc/systemd/system/daily-epub.service` with `SupplementaryGroups=systemd-journal` and `ReadWritePaths=… /etc/daily-epub`; `daemon-reload`.
3. Confirm `/etc/daily-epub` and `config.toml` are owned by `daily-epub` (settings writes rename into that directory) and that `profile.md` lives under `/var/lib/daily-epub` (writable `StateDirectory`).
4. Add the new `[server]` keys to `config.toml` if overriding defaults (optional).
5. Restart `daily-epub.service`; the migration `0004` applies on start. Verify `curl -s https://daily.hallada.net/` shows the latest issue and `/feed.xml` parses.
6. `sudo -u daily-epub daily-epub --config /etc/daily-epub/config.toml users add tyler --admin` (hidden prompt). Log in, open `/dashboard`.
7. Smoke test jobs: start `features-prune` from the Jobs page; watch the log tail; `systemctl status daily-epub-job@features-prune.service`. If polkit refuses, `journalctl -u polkit` shows the denied action.
8. Smoke test settings: change `curation.ranking.utility_protected` by one, confirm `config.toml` kept its comments, read `/dashboard/settings/history`, change it back.
9. Optional: raise `curation.ranking.telemetry_retention_days` for longer article history.

---

## 19. Implementation sequence

Each step is a shippable commit or small series; `cargo fmt --check`, `cargo clippy --all-targets`, `cargo test` green at each. Do not combine steps.

1. **Foundation.** Migration `0004`, `RatingEvent.user_id`, `runs.report_json` + `issues.report_json` writes, `issues.issue_json` write and loader, `GenerateOutcome.run_id`. `web/` skeleton: `WebState`, `AppState.config()` refactor (config path passed into `serve`), layout, error pages, static assets with ETag, security headers layer. `SqliteSessionStore`, `Backend`, the axum-login/tower-sessions/tower_governor layers, CLI `users …`, `/login`, `/logout`, `/account`, the origin-check middleware. Public site: `/`, `/issues`, `/issues/{date}` (public branch), `/feed.xml`, `/robots.txt`, `PublicIssue` with its no-leak test. `/files/*` accepts sessions. README route table updated.
2. **Full issues.** `issue_full.html`, article/world/behind pages, downloads, prev/next, the rating widget and `POST /rate`, the HMAC confirmation link. `app.js` rating enhancement.
3. **Dashboard reads.** Overview, runs list/detail (funnel, admission, timings, providers, warnings, config diff, near misses, candidate table with filters and the signals partial), articles list/detail (with the run history and the text explain). Index `idx_candidate_runs_article_run`.
4. **Ratings and profile.** Ratings page with contributions and the events tab; profile editor, versions, prompt view. `MAX_RATINGS_IN_REBUILD` made `pub`.
5. **Settings.** Schema derivation, help table, form rendering, `toml_edit` writer with validate-and-rename, env locks, providers add/remove, history, config reload-on-mtime. Unit file `ReadWritePaths` change in `systemd/`.
6. **Jobs and stats.** `jobs.rs` catalogue, `job run` subcommand, `JobRunner` + `SystemdRunner`/`MockRunner`, unit template, polkit rule, `SupplementaryGroups`, the Jobs pages; the `stats_data` refactor and the stats page; overview sparklines.
7. **Docs and polish.** README (site, roles, users CLI, jobs, settings), `config.example.toml`, `docs/plans/2026-08-15-implementation-notes.md` (new verified facts with dates: polkit, unit changes, toml_edit, axum-login/tower-sessions, password-auth, tower_governor), the rollout runbook (§18), users page, `robots.txt`/feed links in the layout, a final pass over dark mode and narrow screens.

---

## 20. Acceptance criteria

1. Anonymous `GET /issues/{date}` shows every pick's title (linking to the source), author, source, comment links, section and reading time — and contains **no** Brief, summary, `why`, body, comment text or World Briefing; a test enforces it. `/feed.xml` is a valid Atom feed with the same content.
2. A signed-in user sees the complete issue in HTML — Brief, summaries, `why` lines, article bodies, discussions, World Briefing, Behind the paper — and can download the EPUB, X4 and XTC files while they exist. Issues from before `issue_json` still render (without world/discussion).
3. Only admins can rate; a dashboard rating appends a `rating_events` row with `source = 'dashboard'` and the admin's `user_id`; the EPUB's HMAC links keep working unchanged.
4. Every article the pipeline ever recorded in `candidate_runs` is reachable from `/dashboard/articles` with its stage, reason, signals, assessments, facets, utility, cluster, admission path, editor `why`, rating history and issue appearances — the same facts `explain` prints, plus history across runs.
5. Every run is listed with its funnel, admission mix, timings, provider costs, warnings, near misses, and the config keys that changed since the previous run.
6. The ratings page shows every current verdict with its decayed neighbour weight, feed credit, prompt/rebuild membership and last-run neighbour usage, and lets the admin change, clear or annotate it.
7. The settings page lists every key in `config.toml` (derived from `Config`, so a new key appears without UI work); saving writes the file with comments preserved, validates through `Config::load`, records the change, and a hand edit on disk shows up on the next page view. Secrets are never displayed; env-overridden keys are locked.
8. `profile.md` is editable with version history; the prompt and learned adjustments are viewable.
9. The Jobs page starts `daily-epub-job@<name>.service` through systemd + polkit, shows status and the journal tail, and links a generate job to its run; the server never runs the pipeline in-process.
10. Sessions come from axum-login/tower-sessions with password-auth hashes, `SameSite=Lax` cookies plus an origin check on every POST, a tower_governor login throttle, and a CSP; no password, token or API key is ever logged, rendered or stored in plain form.
11. Existing routes (`/r`, `/opds`, `/files`, `/healthz`, `/issues.json`) behave as before, and `/issues.json` now carries real reports.
12. `cargo test` covers all of §17 without network or systemd.

---

## 21. Deferred, with triggers

| Item | Trigger |
|---|---|
| Per-user personalization (per-user `rating_events`, profiles, issues) | A second real reader. `user_id` on `rating_events` is the hook; nothing else is prepared on purpose. |
| Editing `data/scour-interests.opml` from the UI | The operator asks; `## Interests` in `profile.md` already covers additions. |
| Search over article bodies (FTS5) | Title/URL `LIKE` feels slow or insufficient. |
| Live log streaming (SSE) on the job page | The 5-second refresh feels slow. |
| Passkeys / WebAuthn | A second admin or a phishing concern. |
| Public per-article "why" lines | The operator decides the second-person tone reads fine publicly (one field added to `PublicEntry`, one template line, one test change). |
| Backfilling `issue_json` for old issues from the EPUB files on disk | Only the last `retention_days` of EPUBs exist; the fallback renderer covers the rest. |
| Charts beyond sparklines | `stats` grows a question the tables cannot answer. |
