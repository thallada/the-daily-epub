# Step 1 — Foundation

Read `docs/plans/briefs/web-dashboard/00-shared.md` first, then the plan
`docs/plans/2026-09-03-web-dashboard.md`. This step is plan §19 item 1. Sections
that govern it: §2 (verified crate facts — read carefully, especially the
axum-login git rev API), §3 (routes and access levels), §4 (architecture, state,
templates, design system), §5 (migration and data model), §6 (auth, sessions,
CSRF, throttle), §7 (public site), §15 (config keys and dependencies), §16
(security checklist), §17 (tests for the parts you build).

## Deliverables

1. **Dependencies** (§15): `axum-login` from git at rev
   `151c72d7a1b4646830f86b4332e6bd6e34d719a7`, `password-auth`, `tower_governor`
   (feature `axum`), `time`, `async-trait`, `rpassword`, `toml_edit`, `toml`
   (promote to direct), dev `tower` with `util`. Use `cargo add`; do not hand-pin
   old versions; do not add `tower-sessions` or `argon2` separately. Confirm
   `cargo tree -d` shows a single sqlx and a single `libsqlite3-sys`.
2. **Migration `migrations/0004_web.sql`** exactly as §5.1 (users, sessions +
   indexes, `rating_events.user_id`, `config_changes`, `profile_versions`,
   `jobs`, `runs.report_json`, `issues.issue_json`), plus the index
   `idx_candidate_runs_article_run ON candidate_runs(article_id, run_id DESC)`
   from §9.3 (put it in this migration so step 3 needs no new file).
3. **Data model changes** (§5.2–§5.4): `RatingEvent.user_id: Option<i64>`
   (bound by `append_rating_event`, read back by `current_ratings*` /
   `rated_article_from_row`); `db::finish_run` writes `runs.report_json`;
   `pipeline::record_issue` writes `issues.report_json` (fixing the never-written
   column) and `issues.issue_json` (the `Issue` with every
   `pick.article.content_html` emptied); `GenerateOutcome.run_id: i64`. Add the
   `db` accessors the loader needs (`issue_by_date`, `issue_dates`/listing for
   the archive, `latest_issue_date`).
4. **`src/web/` skeleton** (§4): `mod.rs` (`WebState`, `Html<T>`, `WebError`,
   `Page` layout context, pagination + time helpers), `session.rs`
   (`SqliteSessionStore`, `Backend`, `Credentials`, `AuthSession` alias, `Viewer`,
   `require_same_origin` middleware), `users.rs` (Role, User with redacting
   Debug, hash/verify wrappers, username/password rules, the CLI operations),
   `public.rs` (`PublicIssue` + `/`, `/issues`, `/issues/{date}` public branch,
   `/feed.xml`, `/robots.txt`), `issue.rs` (the `IssueView` loader with
   `issue_json` → row fallback per §5.2; the *full* page template is step 2 —
   for now a signed-in viewer on `/` and `/issues/{date}` may see the public
   rendering plus the downloads list, and the loader must be complete and
   tested), `static/{app.css,app.js,favicon.svg}` served from `/static/{file}`
   with sha256 ETag / 304 and `Cache-Control: public, max-age=86400`, and the
   templates `layout.html`, `error.html`, `login.html`, `account.html`,
   `issue_public.html`, `issue_list.html`, `feed_entry.html`, `_pagination.html`.
   `app.css` implements the §4.4 design tokens, masthead, reading column, the
   dashboard table/badge/kv/funnel/spark/rating classes (so later steps only add
   to it), light and dark. `app.js` can be minimal now (the `data-confirm` and
   `<details>` persistence bits); the rating fetch enhancement is step 2.
5. **`AppState` refactor** (§4.2): `config: Arc<RwLock<Arc<Config>>>`,
   `config_path: Option<PathBuf>`, `web: Arc<WebState>`, `AppState::config()`;
   `server::serve(config, config_path, db)`; `main::serve` passes
   `Config::resolve_path(cli.config)`. Existing handlers call `state.config()`.
   `WebState { jobs: Arc<dyn JobRunner>, started_at, config_mtime }` — define a
   minimal `JobRunner` trait + `DisabledRunner`/`MockRunner` in `src/web/mod.rs`
   or a small `src/web/dashboard/jobs.rs` stub now so the state shape is final
   (step 6 fills in `SystemdRunner` and the pages).
6. **Auth** (§6): session layer, auth layer, governor on `POST /login`
   (`into_make_service_with_connect_info::<SocketAddr>` in `serve`; a periodic
   `retain_recent` task; a periodic `delete_expired` task), `/login` GET/POST,
   `/logout`, `/account` (change password, sign out everywhere), the origin-check
   middleware on every POST except `/r/*`, `login_required!` /
   `permission_required!` route layers on the sub-routers (a `/dashboard` stub
   router with an admin-only placeholder overview page is fine so the guard
   tests in §17 can run now), and the 403 → site error page `map_response`.
   `/files/*` accepts a session or Basic auth per §8's last paragraph.
7. **Security headers** (§16) on every app response: CSP, nosniff, referrer
   policy; `Cache-Control: no-store` on `/dashboard/*`; `Vary: Cookie` on HTML;
   public pages `public, max-age=300` only without a session cookie.
8. **CLI** `daily-epub users add|passwd|role|disable|enable|list|logout` (§6.1),
   none of which take the run lock; `users add --admin` bootstraps.
9. **Config** (§15): the new `[server]` keys with validation, `Config::default()`,
   `config.example.toml`, README table (the key-for-key test must pass).
10. **README**: the route table gains the new public routes and the
    session-or-Basic note on `/files/*`; a sentence that `SmartIpKeyExtractor`
    trusts `X-Forwarded-For` because only nginx reaches the bind address.
11. **Tests** from §17: migration, users/passwords, session store, login flow
    (cookie flags, disabled user, password change logs out other sessions,
    logout, anonymous `/` sets no cookie, `next` validation), guards, origin
    check, throttle (burst 3 test config), public rendering
    (`public_issue_carries_no_generated_text`, comment links, no World Briefing,
    feed is valid XML with one entry per issue, robots.txt, cache headers),
    issue_json round trip + fallback loader + `runs.report_json` /
    `issues.report_json` written and `/issues.json` returning real reports, and
    the `/files/epub` session-or-Basic behaviour. The binary-level test in
    `tests/m7_server.rs` (serves `/`, `/issues`, `/feed.xml`, `/login`;
    `/dashboard` redirects; `users add` then a real login over TCP) — add it, but
    remember it cannot run inside your sandbox.

## Notes and traps

- Read the axum-login source at the pinned rev under `~/.cargo/git/checkouts`
  after `cargo add` to confirm the `Require` builder, `AuthSession::user().await`,
  `login(&self, &user)`, the macros' signatures, and the session data key
  `"axum-login.data"`. §2 describes what was verified; trust the source over the
  plan's sketch if they differ, and record the difference in your handoff.
- `tower-sessions` 0.15 `SessionStore` still uses `async_trait`; axum-login's
  own traits do not.
- `password_auth::verify_password` blocks — run it in `spawn_blocking`, and use
  a fixed dummy hash for unknown usernames so timing does not reveal existence.
- The governor's `per_second` takes seconds per token: `login_window_minutes *
  60 / login_attempts`.
- The `sessions.user_id` column is denormalized best-effort from
  `record.data["axum-login.data"]["user_id"]` — verify the actual shape axum-login
  stores (it may be a struct with `user_id` and `auth_hash`).
- `PublicIssue::from(&Issue)` is the only constructor and carries no generated
  text; the no-leak test renders `fixtures::issue()` publicly and asserts the
  Brief, every summary, every `why`, article body sentences and comment strings
  are absent.
- The `serve_file` Basic-auth fallback must keep the existing OPDS client
  behaviour byte for byte when no session cookie is present.
- Keep `server.rs` as the router root: `router()` merges `web::router()`
  sub-routers; do not move the existing handlers.
