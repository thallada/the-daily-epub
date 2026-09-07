# Feed discovery: propose new Miniflux subscriptions from aggregator hits

**Date:** 2026-09-07
**Repository:** `thallada/the-daily-epub`
**Status:** small implementation plan, ready to execute
**Builds on:** `docs/plans/2026-09-03-web-dashboard.md` (the admin dashboard this adds a page to), `docs/plans/2026-09-02-personalized-curation-v2.md` (the per-article signals this reuses)

Written for a fresh implementation agent. Facts about this repo were checked against `main` (`e69940f`) on 2026-09-07. Facts about the Miniflux API were checked against `https://miniflux.app/docs/api.html` the same day; anything marked *assumption* must be re-checked against the live instance before relying on it.

---

## 1. Goal

Most of the paper's articles arrive through aggregators (Hacker News, Lobsters, Reddit, Scour), not through the author's own feed. Every such article is a lead on a feed the operator does not subscribe to yet. This feature:

1. During `generate`, after articles are persisted, asks Miniflux to discover the feed(s) behind each aggregator-only article and records the ones the operator is not already subscribed to as **feed candidates**.
2. Adds an admin dashboard page, **Feeds** (`/dashboard/feeds`), listing candidates ranked by how likely the operator is to enjoy the feed, computed **algorithmically from telemetry the pipeline already persists** (utility / preliminary blend, stage reached, explicit ratings). No LLM calls, no new embeddings.
3. Each row has **Add** (with a Miniflux category picker; calls `POST /v1/feeds`) and **Dismiss**.

Over time the subscribed set grows and the pool of direct-feed articles each issue draws from grows with it.

Out of scope (deliberately, see §7): auto-subscribing, feed previews/health checks, Scour integration, OPML, per-feed LLM judgement, un-dismissing from the UI.

## 2. Verified facts

### About this repo

- **Pipeline order** (`src/pipeline.rs:354` `run_stages`): ingest → dedupe → extraction → `persist_articles` (stage 4, ~line 443, mints real article ids) → social enrichment (stage 5, best effort, ~line 448) → hygiene/embeddings/signals (stage 6) → … Failure policy is documented at the top of the file: *best effort* stages log, push a warning onto `report` (status `degraded`) and continue. The new stage is best effort.
- **How an article arrived** is stored per article in `articles.sources_json` as `Vec<SourceRef>` (`src/types.rs:64`), each with a `SourceKind` (`Scour | HnFrontpage | Lobsters | Reddit | Feed`, `src/types.rs:49`). Classification is `dedupe::classify_source_with_feed` (`src/dedupe.rs:177`). `Article::came_via(kind)` exists (`src/types.rs:147`). "Aggregator-only" therefore means `!article.sources.iter().any(|s| s.kind == SourceKind::Feed)`.
- **Ingest already loads the whole subscription list once per run**: `MinifluxClient::ingest_window` returns `(Vec<Entry>, HashMap<FeedId, FeedMeta>)` where `FeedMeta` has `feed_url` and `site_url` (`src/miniflux.rs:105`). That map is the "already subscribed" set; no extra call needed in the pipeline.
- **Miniflux client** (`src/miniflux.rs`) is currently GET-only: one private `get_json` helper with retry (`RetryPolicy`, retry on network/5xx/429), `X-Auth-Token` auth, `MinifluxError::{Http, Status, Decode, MissingApiKey}`. Module doc says "Reads only … read state is never mutated" — still true for the pipeline; the dashboard **Add** action is the only writer and only creates feeds.
- **Per-article ranking evidence is persisted** in `candidate_runs` (`migrations/0002_curation_v2.sql:59`): `(run_id, article_id) PK, stage, excluded_reason, signals_json, utility REAL, rank_utility, …`. `signals_json` decodes to `curate::telemetry::SignalsJson` (`src/curate/telemetry.rs:114`) whose `blend()` returns the preliminary blend on 0–100 and whose `top_interests: Vec<TopInterest{name, z, cos}>` names the matched interests. `utility` (0–100) is set only for articles that reached the deep set (`admitted`/`assessed`/`shortlisted`/`selected`). Stage names: `dashboard::STAGES` (`src/web/dashboard/mod.rs`). `runs.status` distinguishes finished runs; dry runs are not flagged in `runs` (a dry run still writes telemetry, which is fine to use).
- **Ratings**: `Db::current_ratings(lookback_days) -> Vec<RatedArticle>` (`src/db.rs:708`) returns one current verdict per article with `value` (`loved` 1.0, `good` 0.35, `not_for_me` −1.0) — the same numbers `signals::RatedExample` uses.
- **Dashboard conventions**: one submodule per page group under `src/web/dashboard/`, each exposing `routes() -> Router<AppState>` merged in `dashboard::router()` (`src/web/dashboard/mod.rs:41`). Admin gating is applied by the caller. POST-then-redirect with a session flash is the established pattern (`users::approve`, `src/web/dashboard/users.rs:122`, uses `set_flash(&session, "error"|"ok", text)` then `Redirect::to`). Nav tabs are hardcoded in `src/web/templates/layout.html:47–54` keyed on `page.active_nav`. Overview tiles live in `src/web/templates/dashboard/overview.html`. Templates are askama; shared pager is `dashboard/_pager.html`.
- **`AppState`** (`src/server.rs:79`) holds `db`, `config: Arc<RwLock<Arc<Config>>>` (read via `state.config()`), `web: Arc<WebState>`, `mailer`. Handlers construct outbound clients from config as needed (`bookorbit.rs` does this for OPDS); reuse whatever shared `reqwest::Client` `WebState` already carries, otherwise `http::build_client(http::DEFAULT_TIMEOUT)`.
- **Config**: sections are `#[serde(deny_unknown_fields, default)]` structs on `Config` with `Default` impls; env override is `DAILY_EPUB_<SECTION>__<KEY>`. The settings dashboard has a **hardcoded section list**, help text and a test asserting the list (`src/web/dashboard/settings.rs`, see the BookOrbit plan §2 for the exact spots) — a new section must be added there or the settings page/test breaks. `config.example.toml` and the README config table document every key.
- **Migrations**: `sqlx::migrate!("./migrations")`; latest is `0009_request_username.sql`, so the new file is `0010_feed_discovery.sql`.
- **Jobs / CLI**: catalogue jobs are in `src/jobs.rs` and run through `main.rs` (`Job::ImportRatings => imports::run(...)`). A one-off backfill only needs a CLI subcommand, not a job (§4 step 6).

### About Miniflux (docs, 2026-09-07)

- **Discover**: `POST /v1/discover` with body `{"url": "<page or site url>"}` → `[{"url": "<feed url>", "title": "…", "type": "rss|atom|json"}]`. Optional body fields (`username`, `password`, `user_agent`, `fetch_via_proxy`) are not needed. **Observed live on 2026-09-07** (operator ran it against several hosts):
  - When the page has no `<link rel="alternate">`, Miniflux falls back to probing well-known paths on the site root (`/atom.xml`, `/feed.atom`, `/feed.xml`, `/feed/`, `/index.rss`, `/index.xml`, `/rss.xml`, `/rss/`, `/rss/feed.xml`) and **returns every path that answered 200 without checking that the body is a feed**. A site that serves its SPA shell for any path (zombo.com, immich, BookOrbit) yields up to nine bogus results, each with `title == url`. **Discover results are leads, not facts; every one must be validated before it is stored (§3, §4 step 3).**
  - When the target site fails, the response is a 4xx/5xx with `{"error_message": "fetcher: bad gateway (502 status code)"}`, `"fetcher: unexpected status code (412 status code)"`, or `"resource not found"` (nothing found / 404). Treat all of these as "host checked, zero candidates" and move on; never retry within the run.
  - Real `<link rel="alternate">` hits work from an **article** URL. Observed for `https://blog.philz.dev/blog/language-server-db/`:
    `[{"title":"blog.philz.dev","url":"https://blog.philz.dev/feed/feed.xml","type":"atom"},{"title":"blog.philz.dev","url":"https://blog.philz.dev/feed/feed.json","type":"json"}]`.
    The same content is offered twice (Atom and JSON Feed) under one title, so link-tag hits need deduping by title too (§3). Miniflux substitutes the URL for the title when the tag has none, so `title == url` marks a result as a well-known-path guess in practice.
- **Create feed duplicate**: observed `HTTP 400` with `{"error_message":"This feed already exists."}`. The flash shows `error_message` verbatim; on that exact message the row is also flipped to `added` (without a `miniflux_feed_id`), since the operator subscribed by other means.
- **Create feed**: `POST /v1/feeds` with `{"feed_url": "…", "category_id": <int>}` → `{"feed_id": <int>}`. Errors come back as 4xx/5xx with a JSON `error_message`; a duplicate subscription is a 4xx (*assumption:* 400 with "This feed already exists"). Surface `error_message` in the flash rather than special-casing.
- **Categories**: `GET /v1/categories` → `[{"id", "user_id", "title", "hide_globally"}]`.
- **Feeds**: `GET /v1/feeds` elements carry `feed_url` and `site_url` (already modelled as `MinifluxFeed`).
- API keys are full-access; the existing key can create feeds. Nothing to provision.

## 3. Design decisions (settled)

| Topic | Decision | Why |
|---|---|---|
| Where discovery runs | A best-effort **pipeline stage right after social enrichment** (needs real article ids, before hygiene). Plus a CLI backfill (`daily-epub feeds discover --days N`) that runs the same function over recent `articles`, for seeding. | The user asked for "a step in article processing". The backfill is the same function behind a flag; without it the page is empty until tomorrow. |
| Which articles | Aggregator-only articles: no `SourceKind::Feed` in `sources`. Skip when the canonical URL's host is in `discovery.skip_hosts` or matches the host of any subscribed feed's `site_url`/`feed_url`. | Articles that already came through a direct feed prove nothing. Host match against subscriptions catches "same site, different feed URL" cheaply. |
| Discovery mechanism | **Miniflux `POST /v1/discover`** with the article URL, followed by **our own validation fetch** of each returned URL. No new crate. | Miniflux already does `<link rel=alternate>` + well-known paths + YouTube etc. and runs on the same host, but its well-known-path guesses are unverified (§2). Validating is ~40 lines; replacing discover with a home-built finder is far more. |
| Validation | `GET` each discovered URL with the shared client (10 s timeout, read at most 256 KB). Accept only if the body, after an optional BOM, whitespace and `<?xml …?>` declaration, starts with `<rss`, `<feed`, or `<rdf:RDF`, or is JSON containing `"https://jsonfeed.org/version/`. Take the feed's own `<title>` from that body (first `<title>…</title>` in the buffer, unescaped, trimmed) as the candidate title when Miniflux gave `title == url`. Any other body, a non-2xx status or an error rejects the URL. | Content-type alone is unreliable (SPAs return `text/html` for everything, some real feeds return `text/xml` or `application/octet-stream`). Sniffing the root element is cheap and decisive. |
| Duplicate results | After validation: group link-tag hits (`title != url`) by `title`, keeping one per group and preferring `type` `rss`/`atom` over `json`; then at most **one** `title == url` guess (the first in Miniflux's order); at most 3 per host overall. | blog.philz.dev offers the same feed as Atom and JSON Feed under one title; WordPress answers `/feed/` and `/rss/`, Hugo `/index.xml` and `/feed.xml`, all with the same content. One is enough. Real tag/category feeds arrive with distinct link-tag titles and are kept. |
| Per-host memo | Table `feed_discovery_hosts(host PK, checked_at, candidates)`. A host is looked up **once**; re-checked only after `RECHECK_DAYS = 90` (const). New articles on an already-checked host are just **linked** to that host's existing candidates (no network). | Keeps per-run lookups bounded and lets a candidate accumulate evidence for free. |
| Per-run cap | `discovery.max_lookups_per_run` (default 30), concurrency 4, 10 s timeout per lookup. Order: articles with the highest social score first. | A run already takes ~12 min; discovery must not add more than ~1 min. |
| Multiple feeds per host | Drop results whose URL or title contains `comment` before validating. Each kept result is one candidate keyed by canonicalized `feed_url` (unique). | Comment feeds are noise; tag/category feeds are a real choice the operator can make on the page. |
| Ranking | Pure function over the candidate's linked articles (§4 step 4): per-article evidence `s_i ∈ [−1, 1]` = explicit rating value if rated; else `0.85` if `stage = selected`; else `utility/100` if present; else `blend()/100` if present; else no evidence. Feed score = shrunk mean `(Σ s_i + prior·k) / (n + k)` with `prior = 0.3`, `k = 2`, shown as 0–100. Sort: score desc, then `n` desc, then `last_seen` desc. | Reuses exactly the signals the paper selects on (interest, knn, feed, social, heuristic, triage, quality, fit) with zero new cost. Shrinkage stops a single 95-utility article outranking three 80s. Constants live in one place; not configurable (YAGNI). |
| "Why" column | The three most frequent `top_interests[].name` across the linked articles, plus the linked article titles (top 3 by `s_i`) linking to `/dashboard/articles/{id}`. | Enough to decide at a glance; all read from `signals_json`, no extra work. |
| Add | Row form: `<select name="category_id">` + **Add** → `POST /dashboard/feeds/{id}/add` → Miniflux `POST /v1/feeds` → set `status = 'added'`, `miniflux_feed_id`, `decided_at`; flash "Added *title* to *category*". The select defaults to the last category used (`kv` key `feed_discovery_last_category`). | The operator will click Add many times in a row; remembering the category saves a click each time. |
| Categories | `GET /v1/categories` **on page render**, one call. If it fails: flash the error and render the table with Add disabled. | Categories change rarely but caching them is more code than a call. |
| Dismiss | `POST /dashboard/feeds/{id}/dismiss` → `status = 'dismissed'`, `decided_at`. No restore button. | Dismissals are cheap to undo with SQL if ever needed. |
| Reconciliation | At each run's discovery stage, any `candidate` whose `feed_url` (or host) now appears in the subscribed feed map becomes `added` (with `miniflux_feed_id`). | Feeds added directly in Miniflux disappear from the list without manual bookkeeping. |
| Page filter | `?status=candidate|added|dismissed` (default `candidate`), allow-listed; paginated with `_pager.html`, 50 rows. | One query param buys an audit view of what was added/dismissed. |
| Config | `[discovery] enabled = true, max_lookups_per_run = 30, skip_hosts = [...]`. | Exactly the knobs that need turning: kill switch, run-time bound, noise list. |

## 4. Implementation steps

1. **Migration** `migrations/0010_feed_discovery.sql`:
   ```sql
   CREATE TABLE feed_candidates (
       id               INTEGER PRIMARY KEY AUTOINCREMENT,
       feed_url         TEXT NOT NULL UNIQUE,
       host             TEXT NOT NULL,
       title            TEXT,
       status           TEXT NOT NULL CHECK (status IN ('candidate', 'added', 'dismissed')),
       first_seen       TEXT NOT NULL,
       last_seen        TEXT NOT NULL,
       miniflux_feed_id INTEGER,
       decided_at       TEXT
   );
   CREATE INDEX idx_feed_candidates_status_host ON feed_candidates(status, host);

   CREATE TABLE feed_candidate_articles (
       candidate_id INTEGER NOT NULL REFERENCES feed_candidates(id) ON DELETE CASCADE,
       article_id   INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
       PRIMARY KEY (candidate_id, article_id)
   );

   CREATE TABLE feed_discovery_hosts (
       host       TEXT PRIMARY KEY,
       checked_at TEXT NOT NULL,
       candidates INTEGER NOT NULL
   );
   ```
   `Db` methods (in a new `src/discovery.rs`, not `db.rs`, following `imports.rs`): `hosts_checked(hosts) -> HashMap<host, (checked_at, candidates)>`, `mark_host_checked`, `upsert_candidate(feed_url, host, title, now) -> id` (bumps `last_seen`, never demotes `added`/`dismissed`), `link_article(candidate_id, article_id)`, `candidates_for_host(host)`, `list(status, page)`, `set_status(id, status, miniflux_feed_id, now)`, `reconcile_added(subscribed: &[(feed_url, site_host)], now)`, `count(status)`.

2. **Miniflux client** (`src/miniflux.rs`): add a private `post_json<B: Serialize, T: DeserializeOwned>(path, body)` mirroring `get_json` (same retry predicate; **do not retry** `create_feed` on 5xx — a retried create can double-subscribe; pass `RetryPolicy::none()` or a flag). Wire types `Discovered { url, title, type }`, `Category { id, title }`, `CreatedFeed { feed_id }`. Methods: `discover(url) -> Vec<Discovered>`, `categories() -> Vec<Category>`, `create_feed(feed_url, category_id) -> i64`. Make `MinifluxError::Status` keep the `error_message` string when the body is JSON so the flash reads "This feed already exists" rather than raw JSON. Update the module doc's "reads only" sentence. Tests: deserialize fixtures for all three.

3. **Discovery stage** (`src/discovery.rs` + wiring in `src/pipeline.rs` after stage 5, timing key `"discovery"`, report count `feed_candidates_new`, warning on error):
   ```text
   select aggregator-only articles → drop skip_hosts / subscribed hosts
     → reconcile_added(subscribed)
     → for each distinct host: if checked within RECHECK_DAYS → link article to its candidates
                              else queue (cap max_lookups_per_run, highest social score first)
     → discover() ×4 concurrent (an error_message response = host checked, 0 candidates)
     → drop "comment" results and already-subscribed feed_urls
     → validate each remaining URL (GET, ≤256 KB, feed-root sniff, pull <title>)
     → one link-tag hit per title (xml over json) + at most one guessed URL, max 3
     → upsert_candidate + link_article + mark_host_checked
   ```
   The function signature is `pub async fn run(db, miniflux: &MinifluxClient, http: &reqwest::Client, cfg: &DiscoveryConfig, articles: &[Article], subscribed: &HashMap<FeedId, FeedMeta>, now) -> Result<Summary>` so the pipeline and the CLI share it. Host = `url::Url::host_str()` lowercased with a leading `www.` stripped; compare subscriptions the same way. Budget: one discover call plus up to nine validation GETs per host, so with the 30-host cap the stage is bounded at ~300 small requests, 4 in flight; log the count. Pure helpers (`aggregator_only`, `host_of`, `keep_result`, `sniff_feed(&[u8]) -> Option<FeedKind>`, `feed_title(&[u8])`, `dedupe_guesses`) get unit tests, including the observed zombo.com response (nine `title == url` guesses that all fail the sniff → zero candidates), the observed blog.philz.dev response (Atom + JSON under one title → exactly the Atom candidate), and an SPA shell body (`<!doctype html>…`) rejected by `sniff_feed`.

4. **Ranking** (`src/discovery.rs`, pure): `pub fn score(evidence: &[ArticleEvidence]) -> f64` and `pub fn why(evidence) -> Vec<String>` per §3. Evidence loading is one query per page render: for the page's candidates, `SELECT article_id, stage, utility, signals_json FROM candidate_runs WHERE article_id IN (…) ORDER BY run_id DESC` keeping the latest row per article, joined with `current_ratings`. The page holds ≤ 50 candidates × a handful of articles each, so ranking happens in Rust after loading **all** `candidate`-status rows (sorting must be global, not per page). If the candidate count ever exceeds ~2 000 this needs a stored score; not now.
   Unit tests: rated article dominates; shrinkage (one 0.95 article scores below three 0.8s); no evidence → prior; `why` picks the modal interests.

5. **Dashboard page** `src/web/dashboard/feeds.rs` + `src/web/templates/dashboard/feeds.html`:
   - Routes: `GET /dashboard/feeds`, `POST /dashboard/feeds/{id}/add`, `POST /dashboard/feeds/{id}/dismiss`. Merge into `dashboard::router()`. Nav tab **Feeds** in `layout.html` (`active_nav = "feeds"`). Overview tile "Feed candidates → review" linking to the page.
   - Table columns: Score (0–100, tabular numerals), Feed (title → `feed_url`, host beneath), Why (interest names + up to 3 article titles linking to the article page), Articles (n), Seen (first/last via `format_time`), Actions (category select + Add; Dismiss). `?status=` filter links at the top. Added rows show the category and a link to the Miniflux feed page (`{base_url}/feeds/{miniflux_feed_id}`); dismissed rows show only the date.
   - Add handler: read `category_id` from the form and pass it through (Miniflux validates it), call `create_feed`. On `Ok` set status + `kv` last category + flash ok. On a 400 whose `error_message` is `This feed already exists.` set status `added` and flash that message as info. On any other `Err` flash `error_message` (or the transport error) and leave the row as `candidate`. Requires `state.config().miniflux.api_key`; if the client fails to build, flash "Miniflux API key is not configured".
   - Server test in the existing dashboard test harness: page renders with an empty table; add/dismiss flip `status` (point `miniflux.base_url` at a `tokio` listener that answers the create call, as the BookOrbit tests do for OPDS; note the loopback-bind caveat in `memory/codex-sandbox-quirk.md`).

6. **CLI backfill** (`src/main.rs`): `daily-epub feeds discover [--days 14] [--limit 200]` — loads `articles` with `first_seen` in the window, fetches the feed map (`MinifluxClient::feed_map`), calls `discovery::run` with `max_lookups_per_run = limit`, prints the summary. Takes the pipeline run lock (it writes the same tables the run does). No job-catalogue entry.

7. **Config + docs**: `DiscoveryConfig { enabled: bool, max_lookups_per_run: usize, skip_hosts: Vec<String> }` with defaults `true, 30, ["news.ycombinator.com", "lobste.rs", "reddit.com", "github.com", "gist.github.com", "x.com", "twitter.com", "youtube.com", "en.wikipedia.org", "arxiv.org", "docs.google.com"]`. Register the section in the settings dashboard (list, help text, test). Document in `config.example.toml` and the README table. Add a short "Feeds" paragraph to the dashboard section of the README.

Order matters only for 1 → 2 → 3 → 4 → 5; 6 and 7 can be done alongside. Expect roughly 900 lines including tests.

## 5. Verification

- `cargo test` green; new pure-function tests in `discovery.rs`, wire-type tests in `miniflux.rs`, one handler test for the page.
- Live, on the server after deploy: run `daily-epub feeds discover --days 14 --limit 50`, open `/dashboard/feeds`, confirm the top rows look like feeds the operator would actually want, click **Add** on one, confirm it appears in Miniflux under the chosen category and the row flips to *added*. Run `generate --dry-run` and check the report has a `discovery` timing under ~60 s and no warning.
- Check the `journalctl` line for how many hosts were skipped as already subscribed; if it is near zero the `www.`/host normalisation is wrong.
- Check the line for discover results rejected by validation. A healthy day should reject a good share of them (SPA catch-alls); zero rejections means the sniff is too lenient, and near-total rejection means it is too strict (look for feeds served with a leading comment or DOCTYPE).

## 6. Rollout notes

- Migrations run automatically on the first `daily-epub` command after deploy.
- No new secrets: the existing `DAILY_EPUB_MINIFLUX__API_KEY` can create feeds.
- Seed the page once with the CLI backfill so it is useful on day one.
- Every Miniflux behaviour the design depends on was observed live on 2026-09-07 (§2); the recorded responses are the test fixtures. Nothing remains to confirm before implementation.

## 7. Non-goals (do not add)

- Auto-subscribing without a click, or any LLM judgement of feeds.
- Fetching or previewing feed contents, feed health checks, unread counts.
- Un-dismiss / undo buttons, per-row notes, bulk actions, sorting controls (the list is already sorted by the only order that matters).
- A stored/cached score column, a scheduled job, OPML export, Scour "recommended feeds" integration (ideas.md item, separate).
- Configurable ranking constants.
