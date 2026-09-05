# "Read in BookOrbit" link on the issue page

**Date:** 2026-09-05
**Repository:** `thallada/the-daily-epub`
**Status:** small implementation plan, ready to execute
**Builds on:** `docs/plans/2026-09-03-web-dashboard.md` (the signed-in issue page this adds a button to)

Written for a fresh implementation agent. Every fact about BookOrbit below was verified on 2026-09-05 against its source (`github.com/bookorbit/bookorbit`, `HEAD`) and against the live instance on this host. Do not re-derive them; do re-check anything marked *assumption*.

---

## 1. Goal

On the signed-in issue page (`src/web/templates/issue_full.html`), next to the existing **Download EPUB / X4 EPUB / XTC** buttons, add a **Read in BookOrbit** button that opens the *normal* (Standard) edition of that issue in BookOrbit's in-browser EPUB reader at `https://bookorbit.hallada.net`. The operator reads on desktop through this link instead of downloading.

Out of scope: the X4 edition, the public (anonymous) issue page, the OPDS feed, the EPUB itself.

## 2. Verified facts

### About this repo

- The pipeline does **not** upload to BookOrbit. `publish::publish_issue` atomically copies both EPUB editions into `publish.epub_dir` (`/home/thallada/bookorbit/books/daily-epub` on the host); BookOrbit's *watched folder* scanner picks them up later. Nothing in the run ever learns a BookOrbit id.
- BookOrbit runs on the same host as Docker (`~/bookorbit/docker-compose.yml`): app container `bookorbit-app` published on `127.0.0.1:3498`, Postgres in `bookorbit-db` with **no published port**, and the `daily-epub` service user has no Docker socket access. Reading BookOrbit's database directly is therefore not an option.
- Issue titles: `Issue::title()` is `The Daily EPUB — <date>` (em dash), and `title_for(Edition::Standard)` is that exact string while `title_for(Edition::X4)` appends ` (X4)` (`src/types.rs` ~531–544). Filenames use a hyphen instead: `publish::issue_filename(date, edition, "epub")` → `The Daily EPUB - <date>.epub` / `... (X4).epub`.
- The signed-in issue view is built in `src/web/issue.rs`: `IssueView` (line ~33) carries `downloads: Vec<Download>`, assembled around line 258 from the `issues` row (`row.epub_path`, `row.x4_path`, `row.xtc_path`). The template renders them at `src/web/templates/issue_full.html:6` as `<a class="btn" href="{{ download.href }}">Download …</a>`.
- Signed-in issue routes live in the `full_issues` router in `src/web/mod.rs` (~line 659: `/issues/{date}/articles/{article_id}`, `/issues/{date}/world`, `/issues/{date}/behind`, behind `login_required!` and `map_forbidden`); public routes (`/issues`, `/issues/{date}`) are on the root router ~line 682. Put the new route in **`full_issues`**.
- `issues` table (`migrations/0001_init.sql:70`): `date TEXT PK, issue_number, generated_at, epub_path, x4_path, xtc_path, front_page_html, report_json` (+ `issue_json` added later). Migrations run via `sqlx::migrate!("./migrations")` (`src/db.rs:24`); latest file is `0004_web.sql`, so the new one is `0005_bookorbit.sql`. `Db::issue_by_date(date) -> Option<IssueRow>` is at `src/db.rs:537`; `upsert_issue` at `:496` (do not touch it, ids are resolved lazily, see §3).
- Config: sections are `#[serde(deny_unknown_fields, default)]` structs hung off `Config` (`src/config.rs:82–85`, e.g. `pub server: ServerConfig`), with `Default` impls. Env overrides are `DAILY_EPUB_<SECTION>__<KEY>`. `config.example.toml` documents every key; the README has a configuration table (~line 350).
- The settings dashboard (`src/web/dashboard/settings.rs`) has a **hardcoded section registry** (`const` list around line 248–259 ending `"publish", "xtc", "server", "miniflux"`), a secret-field list (`("server.basic_auth_pass", FieldKind::Secret)` ~line 219, plus `SECRET_SUFFIXES` at ~223), per-key help text (~line 360), and a test asserting the section list (~line 1683). A new section must be added to all of these or the settings page/test breaks.
- Shared HTTP client: `crate::http::build_client(timeout)` (`src/http.rs:14`), rustls + gzip, no cookies. reqwest 0.13 with the `query` and `json` features is already a dependency; **no XML parser is in the tree** — the OPDS response is tiny and regular, so either add `quick-xml` or match with a small hand parser (see §4).

### About BookOrbit

- Web reader route (client router, `client/src/router/index.ts`): **`/read/:bookId/:fileId`**. Both ids are required numeric integers. Book detail page is `/book/:bookId`.
- An unauthenticated visit to `/read/...` serves the SPA shell (200) and the client auth guard redirects to `/login?redirect=<fullPath>`, returning to the reader after sign-in. So the link can point straight at the reader.
- OPDS (server `server/src/modules/opds/opds.service.ts`, `BASE = '/api/v1/opds'`):
  - Auth is **HTTP Basic** against a dedicated *OPDS user* created in BookOrbit's settings (separate static credentials, not the web login). Verified live: `GET http://127.0.0.1:3498/api/v1/opds/catalog?q=Daily` → `401`, `www-authenticate: Basic realm="bookorbit OPDS"`.
  - Search: `GET /api/v1/opds/catalog?q=<terms>` (Atom acquisition feed; server-side `ILIKE` on title, accent-insensitive). Paged with `?page=N`.
  - Each `<entry>` contains `<title>…</title>`, `<id>urn:bookorbit:book:<bookId></id>`, and one acquisition link per file:
    `<link rel="http://opds-spec.org/acquisition" href="/api/v1/opds/<bookId>/download?fileId=<fileId>" type="application/epub+zip" title="EPUB"/>`.
    **That href carries both numbers the reader URL needs.** Each daily issue edition is its own book with a single EPUB file.
- The JSON API (`/api/v1/books/search?q=`, `/api/v1/books/:id` with `files[].id`) exists too but needs a JWT from `/auth/login` (5/min throttle, refresh cookie). Not worth it; OPDS is enough.
- Book titles come from the EPUB `dc:title`, so the Standard edition is titled `The Daily EPUB — 2026-09-05` and the X4 one `The Daily EPUB — 2026-09-05 (X4)`. *Assumption:* BookOrbit's metadata fetcher has not renamed them; the matcher in §4 tolerates a fallback to the filename form.

## 3. Design decisions (settled)

| Topic | Decision | Why |
|---|---|---|
| Where ids are resolved | **Lazily, on click**, by a new redirect route `GET /issues/{date}/read` in the signed-in router. | BookOrbit indexes the folder some time after the run ends, so the id does not exist at publish time. Rendering the issue page must stay free of cross-service calls (see perf work in `main` 17a8804 and earlier). |
| Caching | Two nullable columns on `issues`: `bookorbit_book_id INTEGER`, `bookorbit_file_id INTEGER`. Filled on first successful lookup; later clicks redirect from the cache with no network call. | One OPDS call per issue, ever. |
| Stale cache | If BookOrbit answers the *reader page*, we can't tell from a redirect. Instead: `GET /issues/{date}/read?refresh=1` clears the cached ids and re-resolves. Expose it as a tiny "wrong book?" link only if trivial; otherwise document the query param in the README. | Books can be deleted/re-added in BookOrbit, changing ids. |
| Not indexed yet | Route returns **503** with the existing error template (`src/web/templates/error.html`), message "BookOrbit has not indexed this issue yet. Try again in a minute." Nothing cached. | Honest, cheap, retry-friendly. |
| Which edition | Standard only. The entry whose `<title>` equals `issue.title_for(Edition::Standard)` **exactly**; entries ending in ` (X4)` are never chosen. | The whole point is desktop reading. |
| Auth to BookOrbit | HTTP Basic with an OPDS user's credentials from config. | Static creds, no token lifecycle; same mechanism KOReader already uses. |
| Feature flag | Whole feature off unless `[bookorbit] enabled = true` **and** the three credentials/URL keys are set. Button and route are absent when off. | Repo stays usable without BookOrbit, as today. |
| Link target | `<public_url>/read/<bookId>/<fileId>`, `public_url` from config (`https://bookorbit.hallada.net`), opened with `target="_blank" rel="noopener"`. | BookOrbit's own login redirect handles the signed-out case. |

## 4. Implementation steps

1. **Config** (`src/config.rs`, `config.example.toml`, README table):
   ```toml
   [bookorbit]
   enabled = false
   public_url = "https://bookorbit.hallada.net"   # what the browser opens
   api_url = "http://127.0.0.1:3498"               # where the server talks OPDS; same host
   opds_user = ""                                  # an OPDS user from BookOrbit → Settings → OPDS
   # opds_pass: environment only (DAILY_EPUB_BOOKORBIT__OPDS_PASS)
   ```
   `BookorbitConfig { enabled: bool, public_url: String, api_url: String, opds_user: Option<String>, opds_pass: Option<String> }` with `deny_unknown_fields, default`. Add `pub bookorbit: BookorbitConfig` to `Config` and its `Default`. Add a `fn is_active(&self) -> bool` (enabled + user + pass non-empty). Validate URLs have no trailing slash (or trim).

2. **Settings dashboard** (`src/web/dashboard/settings.rs`): add `"bookorbit"` to the section list, `("bookorbit.opds_pass", FieldKind::Secret)` (and `"opds_pass"` to `SECRET_SUFFIXES`), help text for the five keys, update the section-list test. Copy the pattern used for `server.basic_auth_pass`.

3. **Migration** `migrations/0005_bookorbit.sql`:
   ```sql
   ALTER TABLE issues ADD COLUMN bookorbit_book_id INTEGER;
   ALTER TABLE issues ADD COLUMN bookorbit_file_id INTEGER;
   ```
   Extend `IssueRow` and `issue_by_date`'s `SELECT` with the two columns; add `Db::set_bookorbit_ids(date, Option<(i64, i64)>)` (`None` clears).

4. **OPDS client** — new module `src/bookorbit.rs` (~100 lines):
   - `pub async fn find_issue(client: &reqwest::Client, cfg: &BookorbitConfig, issue_title: &str, date: Date) -> Result<Option<(i64, i64)>>`.
   - `GET {api_url}/api/v1/opds/catalog?q={date}` with `.basic_auth(user, Some(pass))` and `Accept: application/atom+xml`. Searching by the ISO date string avoids em-dash/hyphen and ILIKE-escaping questions, and returns at most the two editions.
   - Parse entries: split on `<entry>`…`</entry>`, take `<title>` (XML-unescape `&amp;` etc.), and the first acquisition `href` matching `^/api/v1/opds/(\d+)/download\?fileId=(\d+)$`. Choose the entry whose title equals `issue_title` exactly; if none, accept the title equal to the filename stem `The Daily EPUB - <date>` (hyphen form); never accept a title ending in `(X4)`. Prefer `quick-xml` if you'd rather not hand-roll; either is fine, keep it small and unit-tested against a fixture built from the real XML shape in §2.
   - Map `401/403` to a clear error ("BookOrbit rejected the OPDS credentials"), connection errors to "BookOrbit unreachable". Timeout 5 s via `http::build_client`.
   - Log at `info` on first resolution with both ids.

5. **Route** `GET /issues/{date}/read` (`src/web/issue.rs`, registered in `src/web/mod.rs` next to `/issues/{date}/behind`, same session guard as the other signed-in issue routes):
   - 404 if the feature is inactive or the issue row is missing.
   - If `refresh=1`, clear cached ids first.
   - Cached ids → `303 See Other` to `{public_url}/read/{book}/{file}`.
   - Else call `bookorbit::find_issue`; on `Some`, store and redirect; on `None`, 503 with the "not indexed yet" message; on `Err`, 502 with the error text (operator-only page, so the text can be specific).
   - Hold a `reqwest::Client` in `WebState` (`src/web/mod.rs:144`) or build one per call; per-call is acceptable at this frequency.

6. **Template**: add `read_href: Option<String>` to `IssueView` (set to `/issues/{date}/read` when the feature is active and `row.epub_path` is present, i.e. only alongside a real Standard EPUB). In `issue_full.html:6`, render `<a class="btn" href="{{ href }}" target="_blank" rel="noopener">Read in BookOrbit</a>` **before** the download buttons, inside the same flex row, and keep the row visible when it is the only button.

7. **Tests**: unit tests for the XML matcher (exact title, X4 skipped, hyphen fallback, no match, malformed href); a `src/web` test that the route is 404 when inactive and 303 to the right URL when ids are cached (no network). Follow `downloads_are_listed_only_while_the_files_exist` (`src/web/issue.rs:~2059`) for the fixture style.

8. **Docs/ops**: README configuration table rows for `[bookorbit]`; a note under Delivery: create an OPDS user in BookOrbit (Settings → OPDS), put its name in `config.toml` and the password in `/etc/daily-epub/env` as `DAILY_EPUB_BOOKORBIT__OPDS_PASS`, restart `daily-epub.service`. No systemd change is needed: `systemd/daily-epub.service` sets only `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX` and no `IPAddressDeny`, so loopback HTTP to port 3498 is allowed.

## 5. Acceptance

- With the section absent or `enabled = false`: no button, `/issues/<date>/read` → 404, all existing tests pass.
- With valid OPDS credentials: first click on today's issue → 303 to `https://bookorbit.hallada.net/read/<n>/<m>` and the `issues` row now has both ids; second click makes no request to BookOrbit (check the journal).
- Clicking before BookOrbit has scanned the new file → 503 "not indexed yet", nothing cached, later click succeeds.
- The X4 edition is never the target even though it matches the same search.
- `cargo fmt`, `cargo clippy`, `cargo test` clean.
