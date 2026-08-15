# The Daily EPUB — Project Plan


## 1. Overview

**The Daily EPUB** is a Rust service that generates a personalized daily newspaper as an EPUB. Every morning it:

1. Pulls the last ~24h of entries from a self-hosted **Miniflux** instance (300–500 articles/day).
2. Deduplicates, extracts full text, and enriches entries with social-proof signals (HackerNews, Lobsters, Reddit).
3. Applies cheap heuristic pre-filters, then uses **DeepSeek V4 Flash** to score, select, and organize ~15–25 articles into newspaper sections.
4. Generates editorial framing: a front-page "day in brief," section intros, and per-article summaries.
5. Builds two EPUB editions (standard + Xteink X4-optimized), converts the X4 edition to XTC/XTCH.
6. Publishes both EPUB editions into a folder that its own built-in OPDS feed serves (and that **BookOrbit** can optionally watch for a richer library UI). *(Amended: the built-in feed serves EPUBs, not XTC — see §3.11.)*
7. Collects 👍/👎 feedback via rating links inside the EPUB to continuously improve curation.

**Reader profile (bake into curation prompts):** prefers long-form, high-effort, well-written articles on *any* topic; uses social proof (HN/Reddit upvotes+comments) as a quality proxy; wants tech news, light general/US world news (prefers Wikipedia Current Events for world news), Boston-area news, and ultra-niche community news. The full interest list lives in `data/scour-interests.opml` (~220 Scour interests: Rust, systems programming, e-ink, self-hosting, PKM, sci-fi, creative coding, space, running, board games, Boston Tech, etc.) — compile it into the taste profile at build time.

### Existing infrastructure (all on this server)

| Service | Local | External | Notes |
|---|---|---|---|
| Miniflux | `127.0.0.1:8082` | `miniflux.hallada.net` | Installed via PPA. API auth via `X-Auth-Token` header. |
| BookOrbit | `127.0.0.1:3498` | `bookorbit.hallada.net` | NestJS/Vue/Postgres. Supports multiple isolated libraries, per-library watched folders, OPDS at `/api/v1/opds` (Basic auth, `opds_access` permission). |
| The Daily EPUB (new) | `127.0.0.1:<port, e.g. 3499>` | `daily.hallada.net` (reverse proxy to be added) | Rating endpoints + OPDS catalog + downloads. |

### Secrets/config the operator must provide

- `MINIFLUX_API_KEY` (create in Miniflux: Settings → API Keys)
- `DEEPSEEK_API_KEY`
- `DAILY_EPUB_SECRET` (random 32+ bytes; HMAC for rating links)
- BookOrbit: create a "The Daily EPUB" library with its own folder, enable **Watch folders** for it; note the folder path for config.
- Reverse proxy entry for `daily.hallada.net` → `127.0.0.1:3499` (rating links must be reachable from devices on the internet; TLS via existing setup).
- Node.js 18+ and `epub-to-xtc-converter` CLI installed (`npm i -g` per its README) for XTC output.

### Prior art studied

- **feedpaper** (heyjonny.dev / jonashonecker/feedpaper): Feedbin → filter unsuitable feeds → EPUB → manual copy to X4. Lesson: filter out YouTube/link-only/JS-heavy sources early; simplicity works.
- **inkfeed** (adhamsalama/inkfeed): Go backend, Mozilla Readability extraction, MOBI/EPUB export, special handling for Reddit JSON and Google News redirects. Lesson: robust content extraction is the hard part.
- **Calibre news system** (manual.calibre-ebook.com/news.html): recipe model — masthead, per-section feeds, article cleanup hooks, index pages. We mirror its structure: cover → front page → sections → articles.

---

## 2. Architecture

Single Rust binary crate `daily-epub` (workspace not needed yet) with clap subcommands:

```
daily-epub generate [--date YYYY-MM-DD] [--dry-run] [--out DIR] [--max-articles N] [--skip-llm]
daily-epub serve                # long-running: rating endpoints + OPDS catalog + downloads
daily-epub profile rebuild      # regenerate taste profile from ratings (also runs weekly inside generate)
daily-epub backfill-social      # re-poll social scores for recent entries (optional helper)
daily-epub db migrate           # run sqlx migrations (also auto-run on start)
```

`generate` is invoked by a systemd timer each morning; `serve` runs as a persistent systemd service. Both share one SQLite database.

### Pipeline (inside `generate`)

```
Miniflux ingest → normalize/dedupe → content extraction → social enrichment
  → heuristic pre-filter (500 → ~120) → LLM scoring (batched) → LLM selection (~120 → 15–25)
  → comment fetching for selected → LLM editorial (summaries, section intros, front page)
  → EPUB build (standard + X4 editions) → XTC conversion → publish (EPUB dir, XTC dir) → OPDS feed served from the EPUB dir
  → retention pruning → run report logged + stored
```

Every stage writes to SQLite so the run is resumable/idempotent per date: re-running `generate --date X` replaces that issue.

### Repository layout

```
the-daily-epub/
├── Cargo.toml
├── config.example.toml
├── data/scour-interests.opml          # (already present)
├── docs/plans/
├── migrations/                        # sqlx sqlite migrations
├── systemd/
│   ├── daily-epub.service             # serve
│   ├── daily-epub-generate.service    # oneshot
│   └── daily-epub-generate.timer
├── src/
│   ├── main.rs                        # clap dispatch
│   ├── config.rs                      # figment: TOML + env overrides
│   ├── db.rs                          # sqlx pool, queries
│   ├── miniflux.rs                    # API client
│   ├── extract.rs                     # readability + sanitization + word counts
│   ├── social/
│   │   ├── mod.rs                     # SocialRef model, orchestrator
│   │   ├── hn.rs                      # Algolia search + item tree
│   │   ├── lobsters.rs                # /s/{id}.json
│   │   └── reddit.rs                  # api/info.json + comments .json
│   ├── curate/
│   │   ├── prefilter.rs               # heuristics + feed priors
│   │   ├── llm.rs                     # DeepSeek client (async-openai, custom base)
│   │   ├── score.rs                   # batched scoring stage
│   │   ├── select.rs                  # lineup selection stage
│   │   ├── editorial.rs              # summaries, intros, front page
│   │   └── profile.rs                # taste profile build/rebuild
│   ├── comments.rs                    # comment tree → rendered XHTML
│   ├── epub/
│   │   ├── build.rs                   # epub-builder assembly
│   │   ├── templates/                 # askama XHTML templates + CSS
│   │   ├── images.rs                  # download, resize, grayscale, re-encode
│   │   └── x4.rs                      # X4 edition transforms + XTC CLI invocation
│   ├── publish.rs                     # copy to the publish dirs, OPDS feed rendering, retention
│   ├── server.rs                      # axum: /r/… ratings, /opds/daily.xml, /files/…
│   └── report.rs                      # run summary (counts, cost, timings)
└── tests/                             # integration tests with fixture JSON
```

### Crate choices (all popular, well-maintained)

| Concern | Crate |
|---|---|
| async runtime | `tokio` |
| HTTP client | `reqwest` (rustls-tls, gzip, cookies off) |
| HTTP server | `axum` + `tower-http` (trace, fs) |
| serialization | `serde`, `serde_json` |
| DB | `sqlx` (sqlite, runtime-tokio, migrations) |
| CLI | `clap` (derive) |
| config | `figment` (TOML file + `DAILY_EPUB_*` env) |
| errors | `thiserror` (lib-ish modules) + `anyhow` (top level) |
| logging | `tracing` + `tracing-subscriber` (env-filter) |
| time | `jiff` (tz-aware; day boundaries in `America/New_York`) |
| feeds/URLs | `url`; (feed parsing not needed — Miniflux does it) |
| readability | `dom_smoothie` (Rust port of Mozilla Readability; fallback `readability` crate if issues) |
| HTML manipulation | `scraper` (select/rewrite), `ammonia` (sanitize to safe XHTML subset) |
| templating | `askama` (typed XHTML templates) |
| EPUB | `epub-builder` (EPUB3, nav TOC, resources, cover) |
| images | `image` (decode, resize, grayscale, JPEG encode) |
| LLM | `async-openai` with `OpenAIConfig::new().with_api_base("https://api.deepseek.com/v1")` |
| auth tokens | `hmac` + `sha2`, `hex` |
| retry | `backoff` or hand-rolled with `tokio::time` (jittered exponential, max 3) |

---

## 3. Stage details

### 3.1 Miniflux ingestion (`miniflux.rs`)

- Client for `http://127.0.0.1:8082/v1`, header `X-Auth-Token`.
- `GET /v1/entries?order=published_at&direction=desc&published_after=<unix>&limit=250&offset=…` — page through everything published in the window `[now - lookback_hours (default 26h), now]`, **regardless of read/unread status** (never mutate read state; this must not disturb normal reader usage).
- Also `GET /v1/feeds` once per run to map `feed_id → {title, site_url, category.title}`.
- Persist raw entries. Fields used: `id`, `feed_id`, `title`, `url`, `comments_url` (hnrss/lobsters populate this — free social linkage!), `author`, `published_at`, `content` (Miniflux's stored content — full text if the feed's "fetch original content" is on, else the feed summary).
- Watermark per run stored in `kv` table; the window overlap + upsert-by-entry-id makes re-runs safe.

### 3.2 Normalize & dedupe

- **Canonical URL:** lowercase host, strip fragments, strip tracking params (`utm_*`, `ref`, `fbclid`, `gclid`, `s`, `si`), trim trailing `/`, resolve known redirectors (Google News links → target param).
- **Cluster duplicates** (same story via HN frontpage feed + Scour feed + the blog's own feed): primary key = canonical URL; secondary fuzzy pass = normalized title (lowercased, alphanumeric-only) exact match within the window. Merge into one `article` row keeping: the richest content, the union of social refs, and a `sources` list (used as a curation signal — appearing in multiple feeds is itself social proof; specifically flag "came via Scour" and "came via HN frontpage").
- Drop obvious non-articles early: audio/video enclosure-only entries, entries whose URL host is youtube/vimeo/spotify, empty-title entries.

### 3.3 Content extraction (`extract.rs`)

Priority order per article:
1. Miniflux `content` if it looks like full text (word count ≥ 250 or ≥ 80% of a fetched version).
2. Fetch `url` (10s timeout, desktop UA, max 3 MB) → `dom_smoothie` readability → main content HTML.
3. Fallback: feed summary/excerpt with a "(excerpt only — read online)" note; such articles are penalized in pre-filter unless social score is high.

Then: sanitize with `ammonia` (allow: p, h1–h4, ul/ol/li, blockquote, pre, code, em, strong, a, img, figure, figcaption, table basics, hr, br), compute `word_count`, collect image URLs (cap 12/article), detect paywall heuristically (very short text + known paywall domains list) → mark `excerpt_only`.

### 3.4 Social enrichment (`social/`)

For every deduped article (cheap, parallel with a semaphore of ~8, aggressive caching in `social` table):

- **HackerNews** (Algolia, free, generous limits):
  - If `comments_url` is `news.ycombinator.com/item?id=N` → that's the story id.
  - Else `GET https://hn.algolia.com/api/v1/search?query=<canonical_url>&restrictSearchableAttributes=url` → take best hit. Store `points`, `num_comments`, `objectID`.
- **Lobsters:** only when the article arrived via a lobste.rs feed or `comments_url` points at `lobste.rs/s/<id>` (no public URL-search API) → later fetch `https://lobste.rs/s/<id>.json` for score + comments.
- **Reddit:** `GET https://www.reddit.com/api/info.json?url=<canonical_url>` with a descriptive User-Agent (`the-daily-epub/1.0 (personal rss digest; contact tyler@hallada.net)`) → best post by score; store `score`, `num_comments`, `permalink`. Respect ~1 req/sec pacing; on 429 back off and continue (social data is best-effort).
- **X/Twitter:** **not supported** — no free API. Documented limitation; the `source` enum leaves room to add it later.

Composite `social_score = log10(1 + hn_points) + 0.7*log10(1 + reddit_score) + log10(1 + lobsters_score) + 0.5*log10(1 + total_comments)`.

### 3.5 Heuristic pre-filter (`curate/prefilter.rs`) — 300–500 → ~120

Score each article 0–100; keep top `prefilter_keep` (default 120) plus all auto-includes:

- **Auto-include:** articles from feeds in the configured `always_include_feeds` list (the infrequent personal blogs Tyler always reads) skip filtering *and* LLM scoring is still run for section/summary purposes but they can't be dropped.
- `+` word count (long-form preference: 0 pts <300 words, scaling to max at ~2500+)
- `+` social_score (scaled)
- `+` came via Scour (it already matched his interests), `+` came via HN frontpage
- `+` feed prior (see §3.9: per-feed Bayesian upvote rate from ratings history)
- `−` excerpt_only, `−` title looks like link-roundup/release-notes/sponsor post (regex list), `−` domain on a configurable blocklist
- **Dedup vs. history:** exclude anything already included in a previous issue (`issue_articles`), and anything the LLM scored < 3 within the last 7 days (don't re-score churn).

This stage is pure Rust, free, and keeps LLM cost flat as feed volume grows.

### 3.6 LLM curation (`curate/llm.rs`, `score.rs`, `select.rs`) — DeepSeek V4 Flash

Client: `async-openai` against `https://api.deepseek.com/v1`, model id from config (default `deepseek-v4-flash` — **verify exact model id against DeepSeek docs at implementation time**), `response_format: json_object`, temperature 0.3 for scoring / 0.8 for editorial. DeepSeek automatically prefix-caches, so put the (identical, long) system prompt first in every request: cached input is $0.0028/M vs $0.14/M.

**Taste profile (system prompt core, `curate/profile.rs`):** a ~600-word document assembled from: (a) the interest names parsed out of `data/scour-interests.opml`, grouped into themes; (b) hard-coded stated preferences (long-form, effort, any topic if excellent, social proof matters, Boston local, ultra-niche community news, Wikipedia-style neutral world news); (c) a "learned adjustments" section regenerated weekly by an LLM call that summarizes recent 👍/👎 ratings ("consistently downvotes: crypto press releases; consistently upvotes: database internals deep-dives…"). Stored in the DB (`kv`) and versioned.

**Stage A — scoring (batched):** batches of 12 articles per request. Per article send: title, source feed, author, word count, social stats, sources list, and a ~200-word excerpt. Output JSON per article: `{id, score: 0-10, category, rationale (≤20 words), is_paywalled_guess}`. ~120 articles = 10 requests ≈ 90k input (mostly cache-miss article text) + ~4k output ≈ **$0.02**.

**Stage B — lineup selection (single call):** send the top ~40 by combined score (LLM score weighted with social + priors) with their rationales. Output: final 15–25 picks (`target_article_count` config, default 20), each assigned a **section**, an ordering, and one flagged `lead_story`. Sections chosen from a configured palette (LLM may only use these): *Top Stories; Tech & Engineering; Science & Space; AI & Machine Learning; Culture & Essays; Boston & Local; Niche Corner; From the Blogroll* (auto-includes land here by default); *World Briefing* is reserved (§3.8). Empty sections are omitted.

**Stage C — editorial:**
- Per selected article, one call with full text (truncated to ~5k tokens): 2–3 sentence summary written like a newspaper abstract (what it argues, why it's worth reading — not clickbait). 20 calls ≈ 100k input / 3k output ≈ **$0.015**.
- One call for the front page: given the lineup + summaries, write "**From the Editor**" — 250–400 words identifying the day's themes and guiding the read — plus a 2–3 sentence intro per section. Voice: warm, literate, a little playful; never fabricates facts not present in the summaries.

**Cost guardrail:** track token usage per run (returned in API responses) in `runs`; config `max_daily_usd` (default 2.00) — if exceeded mid-run, skip remaining editorial calls and fall back to feed excerpts as summaries, log loudly. Expected steady-state cost: **≈ $0.05–0.30/day**, far under the $5 ceiling, with headroom to feed more/fuller text later.

### 3.7 Comment chapters (`comments.rs`)

For each **selected** article with social refs:

- **HN:** `GET https://hn.algolia.com/api/v1/items/{objectID}` → full tree.
- **Lobsters:** `GET https://lobste.rs/s/{id}.json`.
- **Reddit:** `GET https://www.reddit.com{permalink}.json?limit=100&depth=3&sort=top`.

Rendering (heuristic, no LLM): pick top ~8 top-level threads by score, depth ≤ 3, ≤ 4 children per node, per-comment cap 1,200 chars (ellipsize), whole chapter cap ~4,000 words. Render as nested `<blockquote>`-style indentation with author + points + relative depth styling that reads well on e-ink (no color, border-left indent). Sanitize with `ammonia`. Each discussion becomes its own chapter titled "💬 Discussion: {article title} ({N} comments on {source})", placed immediately after its article and nested under it in the TOC. Multiple sources = one chapter with per-source subsections, ordered HN → Lobsters → Reddit.

### 3.8 World Briefing (Wikipedia Current Events)

Since Tyler prefers Wikipedia's Current Events portal for world news, include it directly rather than curating wire-service articles: fetch the day's portal page (`https://en.wikipedia.org/wiki/Portal:Current_events/{YYYY}_{Month}_{D}` via the MediaWiki REST HTML API), extract the day's bulleted events, strip citations/edit links, keep internal links as plain text, and render as a compact "World Briefing" section chapter with CC BY-SA attribution + link. Config-toggleable (`world_briefing = true`). Failure is non-fatal (skip section).

### 3.9 Feedback loop (`server.rs` + `curate/profile.rs`)

- Each article chapter ends with a footer:
  `Was this a good pick? [ 👍 Yes ] · [ 👎 No ]` + `Read online ↗` (original URL).
- Link format: `https://daily.hallada.net/r/{issue_date}/{article_id}/{up|down}?t={token}` where `token = hex(hmac_sha256(secret, "{issue_date}/{article_id}/{vote}"))[..16]`. GET (KOReader opens links in its built-in browser/prompt; GET is the only thing that works from an e-reader). Idempotent upsert; response is a tiny static HTML page ("Recorded 👍 — thanks!") sized for e-ink browsers.
- Ratings drive: (a) **feed priors** — per-feed `(upvotes+1)/(upvotes+downvotes+2)` beta-smoothed score used in pre-filter; (b) the weekly **learned adjustments** rewrite of the taste profile (§3.6).
- Future (out of v1 scope, schema-ready): embedding-based classifier — `fastembed` (bge-small ONNX) embeddings + `linfa` logistic regression over rated articles as an additional pre-filter signal once ≥ ~200 ratings exist.

### 3.10 EPUB assembly (`epub/`)

Built with `epub-builder` (EPUB3 + nav + NCX fallback), content pages from `askama` templates, all assets embedded (fully offline). Structure:

1. **Cover** — generated PNG: masthead "The Daily EPUB", date ("Friday, August 15, 2026"), issue number (days since first issue), article count. Render simple typographic SVG → rasterize (via `resvg`+`tiny-skia` — small, pure Rust) at 1200×1600 (standard) / 480×800 grayscale (X4).
2. **From the Editor** — front-page brief + issue stats line ("22 articles · ~1h 45m read · 6 sections").
3. **In This Issue** — the introduction chapter: per-section, each article's title, source, reading time, and its 2–3 sentence summary, linked to the chapter.
4. **Sections** — section title page (name + LLM intro), then article chapters: header (title, author, source, date, word count/reading time, social stats line "▲ 342 on HN · 210 comments"), cleaned body with embedded images, footer (rating links + read-online link). Discussion chapter follows when present.
5. **World Briefing** section (when enabled).
6. **Colophon** — generation timestamp, models used, token cost, source feed counts.

TOC: nav depth 2 (sections → articles, discussions nested). Metadata: `dc:title` "The Daily EPUB — 2026-08-15", `dc:creator` "The Daily EPUB", `dc:date`, `dc:language en`, EPUB3 `belongs-to-collection` = "The Daily EPUB" with `group-position` = issue number (BookOrbit/KOReader sort correctly). Deterministic chapter ids (`art-{entry_id}`) so rating links and TOC stay stable across regenerations.

**Images (`epub/images.rs`):** download (10s timeout, 5 MB cap, semaphore 8), re-encode with `image`:
- *Standard edition:* max width 1200px, JPEG q80 (PNG kept for line art/transparency after white-flatten), strip metadata (re-encode does), skip decorative images < 24px, drop SVG/WebP-source images unless decodable, per-issue asset budget ~25 MB.
- *X4 edition (`epub/x4.rs`):* grayscale (Luma8), fit within 480×800, JPEG q70, flatten transparency to white; simplified CSS (no floats/flex/grid, no embedded fonts, larger base font, generous line-height, hyphenation on); cover at native 480×800. (These mirror what `epub-to-xtc-converter` recommends, so the XTC conversion step has ideal input.)
- Every `<img>` gets `alt` preserved and a `<figcaption>` if source had one; failed downloads degrade to a "[image: alt text]" placeholder paragraph.

**CSS:** one small stylesheet per edition tuned for e-ink: serif body, no colors other than grayscale, `page-break-before` on chapters, blockquote-indent comment styling.

### 3.11 XTC conversion & publishing (`publish.rs`)

- Run the `epub-to-xtc-converter` CLI (Node 18+) on the X4 edition: invoke via `tokio::process::Command`, config keys `xtc.command` (default `epub-to-xtc`) and `xtc.args` (verify exact CLI name/flags from the repo README at implementation time; support both `.xtc` 1-bit and `.xtch` 4-level grayscale via config, default XTCH for image quality). Non-zero exit → log error, continue (XTC is a bonus artifact).
- **Publish standard + X4 EPUBs** by atomic copy (`write temp + rename`) into the BookOrbit "The Daily EPUB" library watched folder (`publish.epub_dir`), filenames `The Daily EPUB - 2026-08-15.epub` and `The Daily EPUB - 2026-08-15 (X4).epub`. BookOrbit's watcher auto-imports; the library appears as its own section in BookOrbit's OPDS catalog (`/api/v1/opds`, Basic auth with an OPDS account) — KOReader on Kindle/Palma and CrossPoint on the X4 browse that. Main library stays uncluttered.
- **XTC delivery** *(superseded — see the amendment below; kept for the record)*: copy `.xtch/.xtc` into `publish.xtc_dir`; regenerate a static **OPDS 1.2 acquisition feed** (`xtc.xml`, entries typed `application/octet-stream`, newest first, last 14) served by `daily-epub serve` at `/opds/xtc.xml` with files under `/files/xtc/` (optional Basic auth from config). CrossPoint's OPDS browser can fetch these; worst case the X4 uses the X4 EPUB from BookOrbit instead.
- **Retention:** delete issue files older than `retention_days` (default 21) from both dirs (BookOrbit's scan removes the DB entries); SQLite issue/rating history is kept forever (it's the training data).

> **Amended 2026-08-15 (post-M8), after testing against a real X4.** The XTC
> delivery bullet above does not work and has been replaced. CrossPoint's OPDS
> browser cannot acquire XTC at all — see the implementation notes' "Verified
> external facts" for the two firmware reasons — so:
>
> - The built-in feed lists **EPUBs, both editions**, from `publish.epub_dir`,
>   with every acquisition link typed exactly `application/epub+zip` and files
>   served from `/files/epub/{name}`. Canonical path `/opds/daily.xml`, with
>   `/opds` and `/opds/` as aliases.
> - The feed is **rendered per request** from the directory rather than written to
>   disk, so it cannot go stale behind a failed publish and the retention sweep has
>   no generated index to step around.
> - This makes BookOrbit **optional**: the feed needs only the folder, and it puts
>   the day's issue one screen from the X4's home rather than several clicks down a
>   library tree.
> - XTC is still generated and still published to `publish.xtc_dir`, just not
>   advertised. It stays fetchable at `/files/xtc/{name}` for sideloading.
> - `publish.xtc_dir` is swept by **count** (`xtc_retention_count`, default 5)
>   rather than by age: an XTCH issue measured 79–104 MB, so disk is the binding
>   constraint. EPUBs keep the dated `retention_days` sweep.
> - `publish.bookorbit_dir` was renamed **`publish.epub_dir`** to match: the feed
>   needs the directory, not BookOrbit. The old key is rejected outright
>   (`deny_unknown_fields`) rather than silently falling back to the default,
>   which would publish into a directory the feed does not read.

### 3.12 Server (`server.rs`)

axum on `127.0.0.1:3499`:
- `GET /r/{date}/{article_id}/{vote}?t=` — verify HMAC, upsert rating, tiny HTML response. No auth beyond the token (links live inside a private EPUB; tokens are per-article+vote and unguessable).
- `GET /opds/daily.xml` (aliases `/opds`, `/opds/`), `GET /files/epub/{name}`, `GET /files/xtc/{name}` — optional Basic auth. *(Amended: was `/opds/xtc.xml` + `/files/xtc/` only; see §3.11.)*
- `GET /healthz`, `GET /issues.json` (recent run reports; handy for debugging).
- `tower-http` request tracing; graceful shutdown on SIGTERM.

### 3.13 Database schema (sqlite, `migrations/`)

```sql
entries(id INTEGER PRIMARY KEY,            -- miniflux entry id
  feed_id INT, feed_title TEXT, category TEXT, title TEXT, url TEXT,
  canonical_url TEXT, author TEXT, published_at TEXT, comments_url TEXT,
  raw_content TEXT, fetched_at TEXT);
articles(id INTEGER PRIMARY KEY AUTOINCREMENT,   -- deduped cluster
  canonical_url TEXT UNIQUE, title TEXT, best_entry_id INT REFERENCES entries(id),
  content_html TEXT, word_count INT, excerpt_only BOOL, image_count INT,
  sources_json TEXT, first_seen TEXT);
social(article_id INT, source TEXT CHECK(source IN ('hn','lobsters','reddit','x')),
  item_id TEXT, score INT, num_comments INT, item_url TEXT, fetched_at TEXT,
  PRIMARY KEY (article_id, source));
scores(article_id INT, run_date TEXT, prefilter_score REAL, llm_score REAL,
  llm_category TEXT, rationale TEXT, PRIMARY KEY (article_id, run_date));
issues(date TEXT PRIMARY KEY, issue_number INT, generated_at TEXT,
  epub_path TEXT, x4_path TEXT, xtc_path TEXT, front_page_html TEXT, report_json TEXT);
issue_articles(issue_date TEXT, article_id INT, section TEXT, position INT,
  is_lead BOOL, summary TEXT, PRIMARY KEY (issue_date, article_id));
ratings(issue_date TEXT, article_id INT, vote INT CHECK(vote IN (-1,1)),
  rated_at TEXT, PRIMARY KEY (issue_date, article_id));
feed_priors(feed_id INT PRIMARY KEY, upvotes INT, downvotes INT, included INT);
runs(id INTEGER PRIMARY KEY AUTOINCREMENT, date TEXT, started_at TEXT, finished_at TEXT,
  entries_fetched INT, candidates INT, selected INT,
  input_tokens INT, cached_tokens INT, output_tokens INT, cost_usd REAL, status TEXT, error TEXT);
kv(key TEXT PRIMARY KEY, value TEXT);      -- watermark, taste_profile, profile_version
```

### 3.14 Configuration (`config.example.toml`)

```toml
timezone = "America/New_York"
lookback_hours = 26
target_article_count = 20
prefilter_keep = 120
retention_days = 21
max_daily_usd = 2.0
world_briefing = true

[miniflux]
base_url = "http://127.0.0.1:8082"
# api_key via DAILY_EPUB_MINIFLUX__API_KEY env

[deepseek]
base_url = "https://api.deepseek.com/v1"
model = "deepseek-v4-flash"        # verify exact id
# api_key via env

[curation]
always_include_feeds = []           # miniflux feed ids or site urls
blocked_domains = []
sections = ["Top Stories", "Tech & Engineering", "Science & Space",
  "AI & Machine Learning", "Culture & Essays", "Boston & Local",
  "Niche Corner", "From the Blogroll"]

[publish]
epub_dir = "/srv/bookorbit/libraries/daily-epub"
xtc_dir = "/var/lib/daily-epub/xtc"

[xtc]
enabled = true
command = "epub-to-xtc"             # verify CLI name/flags from repo
format = "xtch"                     # xtc | xtch

[server]
bind = "127.0.0.1:3499"
public_url = "https://daily.hallada.net"
# hmac_secret via env; optional basic auth user/pass for OPDS
```

### 3.15 Deployment (systemd, `systemd/`)

- `daily-epub.service`: `ExecStart=/usr/local/bin/daily-epub serve`, `Restart=on-failure`, hardening (`DynamicUser` or dedicated user, `StateDirectory=daily-epub`, `ProtectSystem=strict` with write access to publish dirs).
- `daily-epub-generate.service` (oneshot) + `daily-epub-generate.timer`: `OnCalendar=*-*-* 05:30:00 America/New_York`, `Persistent=true` (catch up after downtime), `RandomizedDelaySec=300`.
- Install: `cargo build --release`, copy binary, `systemctl enable --now`. Reverse-proxy `daily.hallada.net` → `127.0.0.1:3499`.

---

## 4. Implementation milestones (each independently verifiable)

1. **M1 — Skeleton & ingest:** crate scaffold, config, migrations, `miniflux.rs`, `generate --dry-run` prints fetched entry stats. *Verify: run against live Miniflux, see ~daily volume.*
2. **M2 — Dedupe + extraction + social:** articles table populated with full text, word counts, HN/Reddit/Lobsters scores. *Verify: spot-check known HN stories carry correct points.*
3. **M3 — Pre-filter + LLM scoring/selection:** end-to-end lineup JSON printed in dry-run; token/cost report. *Verify: lineup is sane; cost < $0.50.*
4. **M4 — EPUB standard edition + publish:** full issue EPUB with cover, front page (temporary plain summaries), sections, articles, images; lands in BookOrbit, visible via OPDS on Kindle. *Verify: epubcheck clean; opens in KOReader with working TOC.*
5. **M5 — Editorial + comments:** DeepSeek summaries/intros/front page wired in; discussion chapters. *Verify: read an issue; comments legible on e-ink.*
6. **M6 — X4 edition + XTC + OPDS:** second edition, converter invocation, OPDS feed. *Verify: X4 fetches and renders both.*
7. **M7 — Feedback loop:** `serve` rating endpoints, links in chapters, feed priors in pre-filter, weekly profile rebuild. *Verify: tap 👍 in KOReader → row in `ratings` → prior changes next run.*
8. **M8 — Hardening & ops:** systemd units, retention, cost guardrail, run reports, `issues.json`, README.

## 5. Verification (end-to-end)

- `cargo test` — unit tests: URL canonicalization, dedupe clustering, HMAC round-trip, comment-tree truncation, prefilter scoring; integration tests over fixture JSON (recorded Miniflux/Algolia/Reddit responses) with the LLM stage mocked (`--skip-llm` uses prefilter order).
- `daily-epub generate --dry-run --out ./out --max-articles 6` with real keys → inspect `./out/*.epub` in Calibre + run `epubcheck` (if installed) → zero errors.
- Full live run: `daily-epub generate` → both editions appear in the publish dir (and in BookOrbit's UI if it is running) → browse `daily.hallada.net/opds/daily.xml` from KOReader (Kindle/Palma) or the X4's CrossPoint, download, read.
- Tap a rating link on the Kindle → confirmation page loads → `sqlite3 … 'select * from ratings'` shows the vote.
- Watch `runs` for a week: cost per day, selection quality; tune `prefilter_keep`/prompts.

## 6. Future ideas (explicitly out of v1 scope; don't constrain the design)

Weekly "Sunday Edition" retrospective; LLM editorials/opinion columns on the day's themes; discussion summarization for 500+ comment threads; embedding-based personal ranker (fastembed + linfa) once ratings accumulate; weather/on-this-day front-page ear boxes; a puzzle page; per-section reading-time budgets; Miniflux starred-entry import as implicit positive signal; TTS audio edition; X/Twitter comments if API access ever becomes viable.

## 7. Known limitations & notes

- X/Twitter comments are omitted (no free API).
- Lobsters linkage only works when the entry originated from a lobste.rs feed (no URL-search API).
- Paywalled articles degrade to excerpt + link; they're penalized but not banned (social proof can still surface them).
- The X4 can't follow rating links (no browser) — accepted; rating happens from KOReader devices.
- DeepSeek exact model id and `epub-to-xtc-converter` CLI flags must be confirmed against current docs during implementation (both noted inline).
- Pricing basis (Aug 2026): DeepSeek V4 Flash ≈ $0.14/M input (cache-miss), $0.0028/M cached input, $0.28/M output — steady-state ≈ $0.05–0.30/day, hard-capped by `max_daily_usd`.
