# The Daily EPUB

A personalized daily newspaper, delivered as an EPUB.

Every morning a systemd timer wakes one Rust binary. It pulls the last ~26 hours
from a self-hosted [Miniflux](https://miniflux.app), deduplicates and extracts
the articles, enriches them with HackerNews/Lobsters/Reddit social proof, has
DeepSeek triage every eligible opening and closely assess a 120-article union,
then utility-ranks and diversity-caps a 60-item shortlist for Claude Opus 5 — the editor —
which assembles the issue
(no minimum size, a hard ceiling), writes a one-line *why* under every headline,
the summaries and *The Brief*. It assembles two EPUB editions (a standard one
and one tuned for the Xteink X4 e-ink reader), converts the X4 edition to XTC, and
publishes the lot over its own OPDS catalog — which doubles as a
[BookOrbit](https://github.com/thallada/bookorbit) watched folder if you run one.
Each article chapter ends with Loved it / Good / Not for me links that feed back
into tomorrow's curation, and a short *Behind the paper* chapter before the
colophon says what the run considered, how the deep set was admitted, whether
the learned signals were active, the ten highest-utility near misses, and what
it all cost.

Steady-state cost is roughly **$1/day**: $0.05–0.30 in DeepSeek tokens plus
~$0.50–0.80 for the Claude editor and a few cents of Voyage AI embeddings, each
provider with its own per-UTC-day ceiling (`providers.<name>.max_daily_usd` and
`voyage.max_daily_usd`). Those ceilings are runaway guards, not accounting — set
hard spend limits in the providers' dashboards as the real backstop. The two
LLM roles — *bulk* (triage, assessment, fallbacks) and *editor* — are assigned
by name to entries of a provider registry, so swapping DeepSeek or Claude for
Gemini (or anything OpenAI-compatible) is a config line plus an API key.

- Full design: [`docs/plans/2026-08-15-the-daily-epub.md`](docs/plans/2026-08-15-the-daily-epub.md)
- Implementation decisions: [`docs/plans/2026-08-15-implementation-notes.md`](docs/plans/2026-08-15-implementation-notes.md)
- Web dashboard rollout: [`docs/runbooks/web-dashboard-rollout.md`](docs/runbooks/web-dashboard-rollout.md)

---

## Pipeline

```
Miniflux ingest ─▶ dedupe ─▶ extraction ─▶ persist ─▶ social enrichment
  ─▶ hygiene ─▶ embeddings (Voyage) + cheap signals ─▶ triage (bulk LLM)
  ─▶ union admission ─▶ deep assessment (bulk LLM) ─▶ utility + diversity
  ─▶ editor (editor LLM) ─▶ comments ─▶ editorial (editor LLM)
  ─▶ world briefing ─▶ EPUB (standard + X4) ─▶ XTC ─▶ publish ─▶ report
```

Every stage writes to SQLite, so a run is idempotent per date: re-running
`generate --date 2026-08-15` replaces that issue rather than duplicating it.
Only one writer runs at a time: `generate`, `profile rebuild`, `features
backfill` and `backfill-social` take an advisory `flock` on
`<database_path>.lock`, and a second invocation exits with `generate is already
running` (naming whichever command holds it). `serve`, `explain`, `stats`,
`ratings`, `features prune` and `db migrate` never wait on it, and the kernel
releases the lock when the holder exits, however it exits.

Every run ends with a four-line summary in the log and on stdout:

```
curation: 412 considered → 398 eligible → 398 triaged → 120 assessed → 60 shortlisted → 17 selected
admission: triage 60 · interest 20 · knn 12 · exploration 5 · blend 23 · auto 0
preference: 14 rated w/ embeddings → knn 0.35 · feed off · 41 verdicts in prompt
providers: anthropic $0.62 · deepseek $0.11 · voyage $0.02 · total $0.75 · 23m12s
```

The full report (per-stage counts and timings — `embed`, `signals`, `triage`,
`admit`, `assess`, `rank`, `editor`, `summaries`, `brief` among them — and
per-provider usage) is stored on the `runs` row and in `issues.report_json`.

**Failure policy.** Miniflux ingest, SQLite writes, EPUB assembly and publishing
are fatal — without them there is no issue, and the `runs` row records why.
Social lookups, comment fetching, the world briefing, images and the XTC
conversion are best-effort: they log, add a warning (run status `degraded`) and
the run continues. Every LLM stage *degrades*: an editor call that fails, is
refused, or is over its provider's daily ceiling is retried with the same
prompt on the bulk provider; if the bulk provider is missing, dead or over
budget too, the run takes the `--skip-llm` shape (admission uses cheap signals
and feed excerpts stand in for summaries) instead of losing the day's issue.
Which provider plays which role is the `[llm]` table (`bulk = "deepseek"`,
`editor = "anthropic"` by default); a role whose key is absent is simply
unavailable, and `daily-epub config check` shows the resolved assignment before
a run. Anthropic's server-side refusal fallback (`fallbacks = "default"`) is
enabled on every request to an `anthropic`-kind provider.

---

## Prerequisites

| Requirement | Why | Notes |
|---|---|---|
| Rust (2024 edition toolchain) | building | `cargo build --release` |
| **Miniflux** with an API key | the only content source | Settings → API Keys. The client is read-only and never mutates read state. |
| A key for the **bulk** provider (DeepSeek by default) | triage and deep assessment, and the fallback for every editor call | <https://platform.deepseek.com>. `DAILY_EPUB_PROVIDERS__DEEPSEEK__API_KEY`. Optional: `--skip-llm` runs the whole pipeline without it. |
| A key for the **editor** provider (Anthropic by default) | the editor: selection, summaries, The Brief, the weekly profile rebuild | <https://console.anthropic.com>. `DAILY_EPUB_PROVIDERS__ANTHROPIC__API_KEY`. Optional: without it every editor call runs on the bulk provider. Set a dashboard spend limit; `providers.anthropic.max_daily_usd` is only a runaway guard. Any other `[providers.*]` entry (Gemini is shipped) can take either role — see *Switching providers*. |
| **Voyage AI API key** | article and interest embeddings behind the learned ranking signals | <https://www.voyageai.com>. Optional: without it (or with `--skip-embeddings`) the run uses cached vectors only and the learned signals are absent, never a penalty. |
| A 32+ byte random secret | signs the article rating links | `openssl rand -hex 32` |
| **BookOrbit** library + watched folder | *optional* — a richer library UI on top of the same folder | Delivery does not need it: `daily-epub serve` has its own OPDS catalog over `publish.epub_dir`. If you do run it, create a dedicated "The Daily EPUB" library, enable *Watch folders*, and point `publish.epub_dir` at it. |
| **Node.js 18+** and a clone of [`epub-to-xtc-converter`](https://github.com/bigbag/epub-to-xtc-converter) | XTC/XTCH output for the Xteink X4 | Optional (`xtc.enabled = false` turns it off). Needs `npm install` **inside `cli/`**, and a settings JSON naming a real TTF/OTF — see below. It has **no global npm bin** — it is invoked as `node <repo>/cli/index.js convert …`, which is why `xtc.command`/`xtc.args` are fully general. |
| A reverse proxy for `daily.hallada.net` → `127.0.0.1:3499` | rating links must be reachable from e-readers on the internet | TLS via your existing setup. |

`data/profile.md` is the hand-maintained reader profile; its optional interests are merged
with `data/scour-interests.opml`. Both paths are configurable.

---

## Install & build

```sh
git clone https://github.com/thallada/the-daily-epub && cd the-daily-epub
cargo build --release
cargo test                      # everything is offline; no keys needed
sudo install -m0755 target/release/daily-epub /usr/local/bin/
```

The web UI uses Tailwind CSS v4, but Node is only a development dependency: the
compiled stylesheet is committed and embedded in the Rust binary. After editing
`src/web/tailwind.css`, a web template, or `src/web/static/app.js`, run
`npm install` once and then `npm run css`. Commit both the source changes and
`src/web/static/app.css`; `npm run css:check` verifies that the committed output
is current.

### Commands

```
daily-epub generate [--date YYYY-MM-DD] [--dry-run] [--out DIR] [--max-articles N] [--skip-llm] [--skip-embeddings] [--rescore]
daily-epub serve                # public site, private dashboard, OPDS, ratings, downloads
daily-epub profile rebuild      # regenerate learned profile adjustments
daily-epub ratings list [--days 90] [--label loved|good|down|cleared]
daily-epub ratings set --article 42 --label loved --note "excellent"
daily-epub ratings clear --url https://example.com/article
daily-epub explain --date YYYY-MM-DD (--article ID | --url URL) [--run-id N]
daily-epub explain --date YYYY-MM-DD --near-misses [N]
daily-epub stats [--days 14]    # the evaluation framework, one fact per line
daily-epub features backfill [--days 30] [--rated-only] [--all] [--yes]
daily-epub features prune       # stale embeddings, old telemetry and assessments
daily-epub backfill-social [--days 7]   # re-poll social scores for recent articles
daily-epub feeds discover [--days 14] [--limit 50]  # seed feed candidates from recent aggregator articles
daily-epub db migrate           # run migrations (also automatic on every start)
daily-epub config check         # validate the config, print the resolved roles, keys and paths
daily-epub users add USER [--admin] [--password-stdin]
daily-epub users passwd USER [--password-stdin]
daily-epub users role USER user|admin
daily-epub users disable USER
daily-epub users enable USER
daily-epub users list
daily-epub users logout USER    # revoke all of USER's sessions
daily-epub job run NAME         # systemd job-unit entry point; normally not run by hand
```

On the server, run these through the `justfile` (`just --list`): every server
recipe wraps the binary in `systemd-run` as the `daily-epub` user with
`/etc/daily-epub/env` loaded, exactly as the units do, so API keys resolve.
`just de <subcommand...>` runs anything; `just feeds-discover 30`,
`just config-check`, `just deploy` and `just logs-generate` cover the common cases.
Plain `sudo -u daily-epub daily-epub ...` does **not** read the env file.

`--dry-run` does everything except deliver: it still ingests, persists entries and
articles, curates and **builds both EPUBs into `--out`**, but it does not copy to
BookOrbit, does not run the retention sweep, does not write the `issues` row and
does not advance the ingest watermark. It prints the lineup and the cost report.

`--skip-embeddings` reads the embedding cache but makes zero Voyage calls.
`--rescore` ignores reusable triage/deep assessments for this run.
`--max-articles N` is a ceiling, never a target: the hard maximum becomes
`min(curation.max_article_count, N)` and the soft target is lowered to fit.

`explain` answers "why was this (not) in the paper" from the `candidate_runs`
row the run persisted for every considered article: the stage it reached and the
reason it stopped, every raw and normalized signal with its presence and
effective weight, the top interests, the nearest rated neighbours, the triage
and deep assessments (quality, fit, category, rationale, facets), utility and
rank, the cluster it landed in and what suppressed it, and the editor's reason
for a pick. `--url` canonicalizes the address; an article that is not in the
database at all is reported as never ingested (a feed problem, not a ranking
one). `--near-misses` lists the highest-utility articles that were not selected
(by preliminary blend for articles the ranker never reached).

`stats` is the whole evaluation framework, on purpose: for the last `--days`
(14) it prints the issues and articles published, the mean issue size, explicit
ratings by label and per issue, the up/down ratio of rated picks per admitting
retriever (`admitted_by[0]` — triage, interest, knn, exploration, blend,
auto_include), the exploration yield (admitted, selected, rated positively),
cost per day per provider from `runs.provider_costs_json`, and the mean
generation time. Plain text, one fact per line. Tune from it and from reading
the paper; anything more waits for more ratings.

`features backfill` embeds the rated and published articles first (the learned
set), then the standing interests, then — only with `--all` — every other
article first seen in the window. It prints an estimate and asks before spending
more than 5M tokens unless `--yes`; a warm cache makes zero calls. `features
prune` drops embeddings of articles neither rated nor published that are older
than `curation.ranking.embedding_retention_days`, and `candidate_runs` rows and
`article_assessments` older than `curation.ranking.telemetry_retention_days`.
`generate` runs the same sweep once after publishing, best effort.

`config check` loads and validates the configuration exactly as `generate`
would and prints one fact per line: the config path, the database, profile,
interests and publish paths with `exists`/`MISSING`, each `[llm]` role with its
provider name, kind, model, effort, ceiling and whether its key is present, the
Voyage line likewise, and `editorial.summary_model`. Lines that need attention
start with `!`. It exits non-zero only on a validation error — a missing key or
file is a warning, since the run degrades rather than fails — and never opens
the database or takes the lock, so it is safe to run next to a live `generate`.

## Web site and dashboard

The server is both the public newspaper index and the private operator UI. An
anonymous visitor sees titles, authors, sources, metadata, AI summaries, why
lines and outbound comment links; article bodies, the Brief, the World Briefing
and comments stay private. A signed-in `user`
sees complete issues and article chapters and can download available formats
from a single download menu. An
`admin` can additionally follow direct dashboard links from issue entries and
article chapters, rate articles, and use every `/dashboard/*` page, including
settings and jobs. Personalization is shared across accounts for now.

Visitors can request an account with their preferred username at
`/request-access`; admins review open requests on `/dashboard/users`. Approving
a request creates a `user` account,
emails a random temporary password to the requester, and marks the request
done; the new user must choose a new password on first sign-in. Approval is
available only when `[mail]` is active, so an account is never created with a
password that cannot be delivered. Admins can instead create an account with
`daily-epub users add <username>` on the server and mark the request done.
Usernames are case-insensitive and passwords must be 12–1024 characters.
Bootstrap with `daily-epub users add <username> --admin`; use `users passwd`, `role`,
`disable`/`enable`, `list`, and `logout` for later administration. Password
changes and disabling a user revoke that user's sessions. `/dashboard/users`
also shows roles, status, login times, and open sessions.
When `[mail]` is active and `mail.notify_to` is set, each stored request queues
a plain-text SMTP notification with a direct dashboard review link; SMTP runs
in the background and never delays the visitor's response. Mail settings are
loaded at server startup, so changes require a service restart.

The Jobs page starts only the fixed job catalogue as
`daily-epub-job@<name>.service`; the web server never runs the pipeline inside
its own process. Install `systemd/daily-epub-job@.service` and the narrowly
scoped `systemd/50-daily-epub.rules` polkit rule, and put the server user in
`systemd-journal` so the page can show status and its configured journal tail.
Set `server.jobs_enabled = false` to make starts unavailable.

| Job name | Action |
|---|---|
| `generate` / `generate-YYYY-MM-DD` | Build and publish an issue. |
| `dry-run` | Run the pipeline without publishing or recording an issue. |
| `profile-rebuild` | Rebuild learned taste adjustments from ratings. |
| `features-backfill` | Embed rated and recently published articles and interests. |
| `backfill-social` | Refresh recent social scores. |
| `features-prune` | Remove stale embeddings and curation telemetry. |
| `import-ratings` | Fetch, embed, and rate URLs queued from the Ratings page. |

The Ratings page accepts up to 500 historical article URLs at a time with one
verdict and optional note. Imports run in the background and show per-URL
status in the page plus live logs on the job page. If dashboard jobs are
disabled, queueing still works; run `daily-epub job run import-ratings` by hand.

The Feeds page lists feeds discovered behind articles that reached the paper
only through an aggregator (Hacker News, Lobsters, Reddit, Scour), ranked by
the same per-article signals the paper selects on, so the strongest leads sit
at the top. Each row offers **Add**, which subscribes the feed in a chosen
Miniflux category, and **Dismiss**. Discovery runs as a best-effort stage
during `generate`; `daily-epub feeds discover --days 14 --limit 50` seeds the
page from articles already in the database.

The Settings page derives its fields from `Config`, rewrites `config.toml` in
place with `toml_edit`, preserves comments/order and file permissions, validates
before an atomic rename, and records attributed history. It re-reads hand edits
on the next view. `DAILY_EPUB_*` overrides appear locked, and secrets are shown
only as present or absent. Shipped providers (`deepseek`, `anthropic`,
`gemini`) cannot be removed because defaults would restore them; leave one
unreferenced or edit it. Custom providers can be removed after no `[llm]` role
references them. The service needs `/etc/daily-epub` in `ReadWritePaths` for
these writes.

An article detail page shows the ten closest articles across all stored,
compatible embeddings, independent of runs or ratings, alongside the rated
neighbours captured by the latest run. Its triage kind and deep-assessment
format also distinguish repositories, documentation, product pages,
discussions, papers, interviews, media, and fiction instead of forcing those
pages into essay or report labels. Run `features backfill --all` when recent
articles do not yet have embeddings to compare.

### Web routes

| Route | Access | Purpose |
|---|---|---|
| `GET /`, `/issues`, `/issues/{date}`, `/feed.xml` | Public | Latest issue, archive, stripped issue index, and equivalent Atom feed. Signed-in issue views expand to the complete issue. |
| `GET /issues/{date}/articles/{id}`, `/world`, `/behind` | User or admin | Private article, World Briefing, and Behind the paper chapters. |
| `GET /issues/{date}/read` | User or admin | Open the Standard edition in BookOrbit's web reader when the integration is enabled. |
| `GET /robots.txt`, `/static/{file}` | Public | Crawler policy and embedded CSS, JavaScript, and favicon. |
| `GET/POST /login`, `POST /logout` | Public/session | Sign in and out; login POSTs share a per-client-IP throttle budget with access requests. |
| `GET/POST /request-access` | Public | Request a reader account; POSTs share the login throttle, while GET stays unlimited. Requests are reviewed by an admin. |
| `GET /account`, `POST /account/password`, `/account/logout-all` | User or admin | Change the current password or revoke sessions; temporary-password users must change it before opening protected pages. |
| `POST /rate` | Admin | Append an attributed dashboard rating event. |
| `GET /dashboard` | Admin | Run, budget, rating, job, and config overview. |
| `GET /dashboard/runs[/{id}]`, `/articles[/{id}]`, `/ratings`, `/stats` | Admin | Pipeline history, article explanations, rating contributions/history, historical URL imports, and evaluation stats. |
| `POST /dashboard/ratings/import` | Admin | Queue historical URLs with a verdict and start the background import job. |
| `GET/POST /dashboard/profile`, `POST /dashboard/profile/restore` | Admin | Edit `profile.md`, inspect prompts/adjustments, and restore a version. |
| `GET/POST /dashboard/settings`, `POST /dashboard/settings/providers`, `GET /dashboard/settings/history` | Admin | Edit validated configuration and inspect its audit log. |
| `GET /dashboard/jobs`, `GET /dashboard/jobs/{id}`, `POST /dashboard/jobs/{name}` | Admin | Start fixed systemd jobs and inspect status and logs. |
| `GET /dashboard/users`, `POST /dashboard/users/requests/{id}/approve`, `POST /dashboard/users/requests/{id}/done` | Admin | Approve and email access requests, mark requests handled another way, and view users/open sessions; other account edits use the CLI. |
| `GET /files/epub/{name}`, `/files/xtc/{name}` | Public if Basic auth is unset; otherwise session or Basic auth | Published downloads. Keeping them public when Basic auth is absent preserves existing OPDS acquisition links. |
| `GET /opds`, `/opds/`, `/opds/daily.xml` | Existing optional Basic auth | OPDS acquisition feed. |
| `GET /r/...`, `/healthz`, `/issues.json` | Existing policy | HMAC rating links, health, and issue reports. |

---

## Configuration

Start from [`config.example.toml`](config.example.toml). Load order, later wins:

1. built-in defaults
2. the TOML file (`--config PATH`, else `./config.toml` when present)
3. `DAILY_EPUB_*` environment variables

Nested keys use a **double underscore**: `[miniflux] api_key` becomes
`DAILY_EPUB_MINIFLUX__API_KEY`, and a provider's key is
`DAILY_EPUB_PROVIDERS__<NAME>__API_KEY` with the `[providers.<name>]` table name
upper-cased — the shipped registry reads `DAILY_EPUB_PROVIDERS__DEEPSEEK__API_KEY`,
`DAILY_EPUB_PROVIDERS__ANTHROPIC__API_KEY` and
`DAILY_EPUB_PROVIDERS__GEMINI__API_KEY` (provider names are therefore lowercase
`a-z0-9_`). Top-level keys are just uppercased: `DAILY_EPUB_LOOKBACK_HOURS=30`.
As a convenience, plain **`DAILY_EPUB_SECRET`** is accepted as an alias for
`server.hmac_secret` (the explicit key wins if both are set).

Secrets belong in the environment file, never in the TOML. Stale configuration
fails at startup rather than silently curating without a provider: a
`[deepseek]` or `[anthropic]` table, a top-level `max_daily_usd`, a batch-size
or temperature key outside `[llm]`, or a `DAILY_EPUB_DEEPSEEK__*` /
`DAILY_EPUB_ANTHROPIC__*` environment variable is an error naming the new key.

### Switching providers

The roles are names, the providers are tables. To run the editor on Gemini:

```toml
[llm]
editor = "gemini"          # [providers.gemini] is already declared in config.example.toml
```

and put `DAILY_EPUB_PROVIDERS__GEMINI__API_KEY=…` in the env file. A one-off
A/B without touching the file, since every key is also an env var:

```sh
DAILY_EPUB_LLM__EDITOR=gemini daily-epub generate --dry-run --date 2026-09-02
```

The same works for `bulk`. Both roles may name one provider (they then share
one client and one `max_daily_usd`), `editor = ""` runs everything on bulk, and
a new endpoint is a new `[providers.<name>]` table: `kind = "openai"` for any
OpenAI-compatible chat-completions API (DeepSeek, Gemini, OpenAI, a local
server), `kind = "anthropic"` for the Messages API. `daily-epub config check`
prints what resolved.

### Reference

| Key | Default | Meaning |
|---|---|---|
| `timezone` | `America/New_York` | Day boundaries and `--date` interpretation. |
| `lookback_hours` | `26` | Size of the ingest window ending at the issue day's end (clamped to now). |
| `target_article_count` | `20` | Soft target the editor aims for. There is no minimum: a nine-pick issue is published as nine. |
| `retention_days` | `21` | EPUBs older than this are deleted from `publish.epub_dir`. SQLite history is kept forever. |
| `xtc_retention_count` | `5` | How many XTC issues to keep in `publish.xtc_dir`. Counted, not dated: each `.xtch` is ~80–100 MB, so the binding constraint is disk, not age. |
| `world_briefing` | `true` | Include the Wikipedia Current Events section. |
| `database_path` | `/var/lib/daily-epub/daily-epub.db` | SQLite file; parent dirs are created. |
| `out_dir` | `/var/lib/daily-epub/out` | Where `generate` writes artifacts before publishing. |
| `profile_path` | `data/profile.md` | Hand-maintained reader profile, loaded every run. |
| `interests_opml` | `data/scour-interests.opml` | Scour interests merged with the profile interests. |
| `miniflux.base_url` | `http://127.0.0.1:8082` | Miniflux root (no `/v1`). |
| `miniflux.api_key` | — | **`DAILY_EPUB_MINIFLUX__API_KEY`**. Required. |
| `miniflux.page_limit` | `250` | Entries per page; Miniflux caps this at 250. |
| `llm.bulk` | `deepseek` | The `[providers.*]` name that runs triage, deep assessment and every fallback. `""` ⇒ no bulk provider (those stages are skipped). |
| `llm.editor` | `anthropic` | The provider that assembles the lineup, writes the summaries and The Brief and rebuilds the profile. `""` or absent ⇒ everything runs on `bulk`. Naming the same provider as `bulk` shares one client and one ceiling. |
| `llm.triage_batch_size` | `25` | Articles per first-pass triage request. |
| `llm.deep_batch_size` | `8` | Articles per close-reading assessment request. The removed `score_batch_size` key is a startup error. |
| `llm.score_temperature` | `0.3` | Scoring temperature, sent only to `openai`-kind providers. |
| `llm.editorial_temperature` | `0.8` | Summaries and The Brief on an `openai`-kind provider. |
| `providers.<name>.kind` | — | `openai` (chat completions at `{base_url}/chat/completions`, bearer key) or `anthropic` (Messages API: `output_config.effort`, a cached system block, `fallbacks = "default"` with the `server-side-fallback-2026-07-01` beta). Shipped entries: `deepseek`, `anthropic`, `gemini`. |
| `providers.<name>.base_url` | — | Endpoint root. DeepSeek `https://api.deepseek.com/v1`; Anthropic `https://api.anthropic.com`; Gemini `https://generativelanguage.googleapis.com/v1beta/openai`. |
| `providers.<name>.model` | — | `deepseek-v4-flash` (verified 2026-08-15), `claude-opus-5`, `gemini-3.8-flash` (verified 2026-09-02). |
| `providers.<name>.api_key` | — | **`DAILY_EPUB_PROVIDERS__<NAME>__API_KEY`**, environment only. Absent ⇒ that role is unavailable and degrades (bulk ⇒ heuristic curation, editor ⇒ bulk). |
| `providers.<name>.effort` | `high` (anthropic, gemini) | `anthropic`: `low`, `medium`, `high`, `xhigh` or `max` → `output_config.effort`. `openai`: passed through as `reasoning_effort` (Gemini takes `minimal`–`high`); omit it for models without one (DeepSeek). |
| `providers.<name>.max_daily_usd` | `2.0` / `3.0` / `3.0` | That provider's ceiling per **UTC day** of the run's start, not per run — a re-run inherits what earlier runs that day already spent on it (`runs.provider_costs_json`). Tripping it skips that provider's remaining calls; in-flight requests finish and the paper still publishes. `0` disables the guard. |
| `providers.<name>.max_concurrent_requests` | `4` | Triage and deep-assessment batches in flight on the bulk provider; summaries in flight on the summary provider. |
| `providers.<name>.price_input_per_mtok` | `0.14` / `5.0` / `0.75` | USD per 1M cache-miss input tokens (cost guardrail arithmetic). |
| `providers.<name>.price_cache_read_per_mtok` | `0.0028` / `0.5` / `0.075` | USD per 1M cache-hit input tokens. |
| `providers.<name>.price_cache_write_per_mtok` | `0.0` / `6.25` / `0.0` | USD per 1M tokens written to the prompt cache (implicit caches charge nothing). |
| `providers.<name>.price_output_per_mtok` | `0.28` / `25.0` / `3.75` | USD per 1M output tokens, thinking tokens included where the provider bills them as output. |
| `voyage.enabled` | `true` | Embed articles and interests with Voyage AI. `false` ⇒ cached vectors only. |
| `voyage.base_url` | `https://api.voyageai.com/v1` | `POST {base_url}/embeddings`. |
| `voyage.model` | `voyage-4-lite` | Embedding model; changing it invalidates the cache. |
| `voyage.api_key` | — | **`DAILY_EPUB_VOYAGE__API_KEY`**. Absent ⇒ cached vectors only. |
| `voyage.output_dimension` | `512` | One of 256, 512, 1024, 2048. |
| `voyage.batch_size` | `32` | Texts per request. |
| `voyage.max_concurrent_requests` | `4` | Requests in flight. |
| `voyage.max_input_chars` | `60000` | Per-article cut, on a char boundary. |
| `voyage.max_daily_usd` | `0.50` | Runaway guard at $0.02/M tokens. |
| `curation.max_article_count` | `28` | Hard ceiling on issue size. `--max-articles N` lowers it to `min(28, N)` and drags the soft target down with it. Must be ≥ `target_article_count`. |
| `curation.always_include_feeds` | `[]` | Miniflux feed ids or URL substrings that can never be dropped. |
| `curation.blocked_domains` | `[]` | Hosts excluded outright. |
| `curation.paywall_domains` | `[]` | Extra paywalled hosts, merged with the built-in list (nytimes, wsj, ft, economist, …). |
| `curation.sections` | 8 sections | The **only** section names the model may use. `World Briefing` is reserved and never offered. |
| `curation.feedback.loved_value` | `1.0` | Weight for a Loved it verdict. |
| `curation.feedback.good_value` | `0.35` | Weight for a Good verdict. |
| `curation.feedback.not_for_me_value` | `-1.0` | Weight for a Not for me verdict. |
| `curation.feedback.verdicts_in_prompt` | `60` | Recent explicit verdicts included in the system prompt. |
| `curation.recent_rejection_days` | `7` | Churn window for recent low triage/deep assessments. |
| `curation.recent_rejection_floor` | `3.0` | Scores below this floor are excluded during the churn window (except auto-includes). |
| `curation.ranking.*` | see below | Every weight, quota, gate and threshold of the personalized ranker. |
| `editorial.summary_model` | `editor` | Which `[llm]` role writes the per-article summaries: `editor` (with per-article bulk fallback) or `bulk`. |
| `editorial.summary_input_tokens` | `3000` | Article text offered to the summary prompt. |
| `publish.epub_dir` | `/srv/bookorbit/libraries/daily-epub` | Both EPUB editions land here by atomic copy, and this is the directory the OPDS feed lists. The editions are distinguished by a `(X4)` tag in **both** the filename and `dc:title` — libraries and OPDS clients list books by title, so the filename alone would make them look identical. Point a BookOrbit watched folder at it if you want its UI too. **Renamed from `bookorbit_dir`**; the old key is a hard config error. |
| `publish.xtc_dir` | `/var/lib/daily-epub/xtc` | XTC artifacts. **Not** listed in the OPDS feed — CrossPoint cannot acquire them — but downloadable at `/files/xtc/<name>` for sideloading. |
| `bookorbit.enabled` | `false` | Enable the admin-only **Read in BookOrbit** integration when both OPDS credentials are set. |
| `bookorbit.public_url` | `https://bookorbit.hallada.net` | Browser-facing BookOrbit base URL. |
| `bookorbit.api_url` | `http://127.0.0.1:3498` | Server-facing BookOrbit base URL used for OPDS lookups. |
| `bookorbit.opds_user` | unset | Dedicated OPDS user created in BookOrbit's Settings → OPDS. |
| `bookorbit.opds_pass` | — | **`DAILY_EPUB_BOOKORBIT__OPDS_PASS`**, environment only. |
| `mail.enabled` | `false` | Enable outbound SMTP when the relay, sender, username, and password are configured. Mail settings require a server restart. |
| `mail.smtp_host` | `""` | SMTP relay hostname, such as `email-smtp.us-east-1.amazonaws.com`. |
| `mail.smtp_port` | `587` | SMTP relay port. Use 587 with STARTTLS or commonly 465 with implicit TLS. |
| `mail.smtp_starttls` | `true` | `true` uses STARTTLS; `false` uses implicit TLS. |
| `mail.smtp_user` | unset | SMTP username. It may be supplied as `DAILY_EPUB_MAIL__SMTP_USER`. |
| `mail.smtp_pass` | — | **`DAILY_EPUB_MAIL__SMTP_PASS`**, environment only. |
| `mail.from` | `""` | Sender mailbox, either a bare address or `Name <address>`. |
| `mail.notify_to` | unset | Recipient for access-request notifications. |
| `discovery.enabled` | `true` | Run the feed discovery stage during `generate`. |
| `discovery.max_lookups_per_run` | `30` | Hosts one run may look up in Miniflux. Each host is re-checked at most every 90 days. |
| `discovery.skip_hosts` | aggregators, code hosts, social networks | Hosts never looked up. A host matches itself or any subdomain of it. |
| `xtc.enabled` | `true` | Set `false` to skip the converter entirely. |
| `xtc.command` | `node` | Converter executable. |
| `xtc.args` | `["/opt/epub-to-xtc-converter/cli/index.js", "convert"]` | Prefix; the code appends `<input.epub> -o <output> -f <format>` (plus `-c <settings>`). |
| `xtc.format` | `xtch` | `xtc` (1-bit) or `xtch` (2-bit grayscale). `xtch` is ~96 KB per rendered page, `xtc` half that. |
| `xtc.settings` | unset | Settings JSON passed as `-c`. The flag is optional to the converter but the file is **required in practice**: without `font.path` the converter exits 2 before doing any work. Start from [`xtc-settings.example.json`](xtc-settings.example.json). |
| `server.bind` | `127.0.0.1:3499` | Listen address. |
| `server.public_url` | `https://daily.hallada.net` | Base URL the rating links inside the EPUB are built from. |
| `server.hmac_secret` | — | **`DAILY_EPUB_SERVER__HMAC_SECRET`** (or `DAILY_EPUB_SECRET`). Without it, generated links are rejected with 403. |
| `server.basic_auth_user` / `_pass` | unset | Optional Basic auth for `/opds/*` and `/files/*`; signed-in web users may download from `/files/*` without Basic auth. |
| `server.session_days` | `30` | Sliding lifetime for dashboard login sessions. |
| `server.login_attempts` | `10` | Shared login and access-request POSTs allowed per IP in one throttle window. |
| `server.login_window_minutes` | `15` | Length of the login throttle window. |
| `server.jobs_enabled` | `true` | Allow the dashboard to start the fixed systemd job catalogue. |
| `server.journal_lines` | `300` | Journal lines shown on a dashboard job page (10–5000). |

`[curation.ranking]` holds the ranker's tunables. The learned signals are
gated: `knn` (rated-neighbour preference) ramps from `knn_floor` (8) to
`knn_full` (25) rated articles with embeddings, `feed` (feed affinity) from
`feed_floor` (15) to `feed_full` (40) attributable ratings; below the floor the
signal is absent. Ratings decay with `rating_half_life_days` (60) over
`rating_lookback_days` (180); `neighbour_k` (5) neighbours per side and
`negative_coefficient` (0.75) shape the signal. `triage_max` (800),
`deep_keep` (120), `shortlist_keep` (60), `assessment_reuse_days` (3),
`semantic_min_words` (300), `exploration_slots` (5), `[curation.ranking.quotas]`
(`triage` 60 · `interest` 20 · `knn` 20), `[curation.ranking.weights.utility]`
(`quality` 0.40 · `fit` 0.20 · `knn` 0.15 · `interest` 0.10 · `feed` 0.05 ·
`triage` 0.05 · `social` 0.03 · `heuristic` 0.02, over the signals present for
each article of the deep set) and `[curation.ranking.diversity]`
(`cluster_threshold` 0.85, `per_cluster_cap` 2, `utility_protected` 10) drive
the LLM triage, deep assessment, utility ranking and diversification stages.
`[curation.ranking.weights.preliminary]` (`interest` 0.35 · `knn` 0.25 ·
`heuristic` 0.20 · `feed` 0.10 · `social` 0.10) blends the cheap signals; weights
are renormalized over the signals present for each article, so they need not sum
to 1. `embedding_retention_days` (120) and `telemetry_retention_days` (180) are
what `features prune` enforces. Validation: weights non-negative; `deep_keep ≥
shortlist_keep ≥ target_article_count`; `*_full > *_floor`; `0 ≤
cluster_threshold ≤ 1`; `per_cluster_cap ≥ 1`; Voyage batch size and concurrency ≥ 1.

---

## Deployment (systemd)

Units live in [`systemd/`](systemd/): `daily-epub.service` (the server),
`daily-epub-generate.service` (oneshot) and `daily-epub-generate.timer`
(05:30 America/New_York, `Persistent=true`, 5-minute jitter).

```sh
# binary
cargo build --release
sudo install -m0755 target/release/daily-epub /usr/local/bin/

# user + state
sudo useradd --system --home /var/lib/daily-epub --shell /usr/sbin/nologin daily-epub

# config + secrets
sudo install -d -m0750 -o daily-epub -g daily-epub /etc/daily-epub
sudo install -m0640 -o daily-epub -g daily-epub config.example.toml /etc/daily-epub/config.toml
sudo -e /etc/daily-epub/config.toml            # set publish dirs, xtc args, public_url
sudo tee /etc/daily-epub/env >/dev/null <<EOF
DAILY_EPUB_MINIFLUX__API_KEY=…
DAILY_EPUB_PROVIDERS__DEEPSEEK__API_KEY=…
DAILY_EPUB_PROVIDERS__ANTHROPIC__API_KEY=…
# DAILY_EPUB_PROVIDERS__GEMINI__API_KEY=…     # only if a role names "gemini"
DAILY_EPUB_VOYAGE__API_KEY=…
DAILY_EPUB_SERVER__HMAC_SECRET=$(openssl rand -hex 32)
DAILY_EPUB_MAIL__SMTP_USER=…
DAILY_EPUB_MAIL__SMTP_PASS=…
EOF
sudo chown daily-epub:daily-epub /etc/daily-epub/env && sudo chmod 0600 /etc/daily-epub/env
# One key per [providers.<name>] entry a role uses, named after the table
# (upper-cased). Any may be left unset: that role is then unavailable and the
# run degrades (editor → bulk, bulk → heuristic curation, Voyage → cached
# vectors). The pre-registry DAILY_EPUB_DEEPSEEK__API_KEY /
# DAILY_EPUB_ANTHROPIC__API_KEY names are a startup error, not a silent no-op.
sudo -u daily-epub bash -c 'set -a; . /etc/daily-epub/env; set +a;
  daily-epub --config /etc/daily-epub/config.toml config check'   # roles, keys present?, paths

# publish dirs must exist and be writable by the service user
sudo install -d -o daily-epub -g daily-epub /var/lib/daily-epub/xtc
sudo setfacl -m u:daily-epub:rwx /srv/bookorbit/libraries/daily-epub   # or chown

# units
sudo install -m0644 systemd/daily-epub.service systemd/daily-epub-generate.service \
                    systemd/daily-epub-generate.timer systemd/daily-epub-job@.service \
                    /etc/systemd/system/
sudo install -m0644 systemd/50-daily-epub.rules /etc/polkit-1/rules.d/
sudo systemctl daemon-reload
sudo systemctl enable --now daily-epub.service daily-epub-generate.timer
```

> **Keep `ReadWritePaths` in sync.** The units run under `ProtectSystem=strict`
> and list the publish directories explicitly; the server additionally lists
> `/etc/daily-epub` so Settings can replace `config.toml`:
> ```
> # generate and job units
> ReadWritePaths=/home/thallada/bookorbit/books/daily-epub /var/lib/daily-epub/xtc
> # server unit
> ReadWritePaths=/home/thallada/bookorbit/books/daily-epub /var/lib/daily-epub/xtc /etc/daily-epub
> ```
> If you change `publish.epub_dir` or `publish.xtc_dir` in the config, change
> these lines too and `systemctl daemon-reload`, or publishing fails with
> `Read-only file system`.

The generate unit deliberately omits `MemoryDenyWriteExecute` because it spawns
Node for the XTC converter, whose JIT needs W+X pages.

### Reverse proxy (`daily.hallada.net`)

The rating links baked into every article chapter point at
`server.public_url`, so `daily.hallada.net` must resolve and serve TLS from the
internet (e-readers tap these links). The OPDS catalog rides on the same host.

The login throttle's `SmartIpKeyExtractor` keys on the first entry of
`X-Forwarded-For`, so nginx must *set* that header from the connection's peer
address rather than appending to whatever the client sent — otherwise anyone
can mint a fresh throttle bucket per attempt by sending their own header. (The
original config appended with `$proxy_add_x_forwarded_for`; that was the
spoofable version.) The whole arrangement is only safe because the configured
bind address is loopback and nginx is the only process that can reach it.

Nothing sits in front of nginx. The site ran behind the Cloudflare proxy for one
day (2026-09-04 to 2026-09-05) and was taken back out: measured from Boston, the
proxied signed-in page took 70 ms after the TLS handshake against 27 ms direct,
and at this traffic the edge cache is cold for anonymous readers anyway. The zone
still lives on Cloudflare's nameservers, so re-proxying is a one-click toggle;
`docs/runbooks/cdn-rollout.md` keeps the zone settings, cache rules and the
nginx additions (the `set_real_ip_from` snippet) that the proxied setup needs.

```nginx
# One pool of idle connections to the app, so a request does not pay a fresh
# loopback TCP connect (and leave a TIME-WAIT socket) every time.
upstream daily_epub {
    server 127.0.0.1:3499;
    keepalive 8;
}

server {
    # HTTP/3 (nginx ≥ 1.25 built --with-http_v3_module; nginx.org packages
    # are). QUIC folds the TCP and TLS handshakes into one round trip. UDP 443
    # must be open in the firewall. `reuseport` goes on exactly one quic
    # listener per address; other server blocks on this host share it.
    listen 443 quic reuseport;
    listen [::]:443 quic reuseport;
    listen 443 ssl;
    listen [::]:443 ssl;
    http2 on;
    http3 on;
    server_name daily.hallada.net;
    # Tell an HTTP/2 client that HTTP/3 is available on the same port; the
    # browser switches on its next connection and remembers for a day.
    add_header Alt-Svc 'h3=":443"; ma=86400' always;

    ssl_certificate     /etc/letsencrypt/live/daily.hallada.net/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/daily.hallada.net/privkey.pem;
    ssl_trusted_certificate /etc/letsencrypt/live/daily.hallada.net/fullchain.pem;

    # TLS 1.3 0-RTT: a returning browser sends its first request inside the
    # handshake and saves a round trip. Safe only because the app answers 425
    # to any non-GET that arrives as early data (the Early-Data header below).
    # Applies to the TCP listener; over QUIC, nginx needs OpenSSL ≥ 3.5.1 for
    # early data, and the nginx.org build links the distro's OpenSSL.
    ssl_early_data on;

    include /etc/nginx/snippets/security-headers.conf;

    # The global `gzip on` only covers text/html. The stylesheet is ~12 KB of
    # plain CSS again — the Newsreader faces are separate .woff2 URLs, not
    # base64 inside it (see the doc comment on `web::APP_CSS`) — so this list is
    # about the JSON, XML and SVG responses. woff2 is already compressed and is
    # deliberately absent.
    gzip_types text/css application/javascript application/json
                        application/speculationrules+json image/svg+xml
                        application/atom+xml application/xml text/plain;
    gzip_min_length 1024;
    gzip_vary on;
    gzip_proxied any;

    # Keep a browser's connection open longer than the 75 s default so the
    # first click after a pause reuses it instead of paying a new TLS handshake.
    keepalive_timeout 300s;

    # XTCH files can be tens of MB; don't buffer them through nginx memory.
    proxy_max_temp_file_size 0;

    location / {
        proxy_pass http://daily_epub;
        # Keep-alive to the upstream needs HTTP/1.1 and an empty Connection
        # header (the default would forward "close").
        proxy_http_version 1.1;
        proxy_set_header Connection "";
        proxy_set_header Host $host;
        # SET, not append. The login throttle keys on the first X-Forwarded-For
        # entry, so a client-supplied header must never survive into the app.
        # $remote_addr is the connection's peer, which is the visitor itself
        # now that no proxy sits in front of nginx.
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto $scheme;
        # "1" while the request arrived as 0-RTT data (RFC 8470); the app
        # rejects non-safe methods sent that way.
        proxy_set_header Early-Data $ssl_early_data;
    }
}

server {
    listen 80;
    listen [::]:80;
    server_name daily.hallada.net;
    return 301 https://$host$request_uri;
}
```

**Only while the record is proxied through Cloudflare:** the connection's peer
is then Cloudflare, so `real_ip` has to rewrite `$remote_addr` from
`CF-Connecting-IP` before the `X-Forwarded-For` line runs. Add
`include /etc/nginx/snippets/cloudflare-real-ip.conf;` to the server block,
above `location /`. Leave it out when the record is DNS-only: it would be inert
for ordinary visitors, but anything connecting *from* a Cloudflare address (a
Worker, say) could then name its own address and dodge the login throttle.
`/etc/nginx/snippets/cloudflare-real-ip.conf` is one `set_real_ip_from` line per
published Cloudflare range plus the header to read. Cloudflare adds ranges from
time to time, so **regenerate it from the source rather than copying this list**
(<https://www.cloudflare.com/ips-v4>, <https://www.cloudflare.com/ips-v6>):

```sh
{ curl -s https://www.cloudflare.com/ips-v4; echo; \
  curl -s https://www.cloudflare.com/ips-v6; echo; } \
| awk 'NF {print "set_real_ip_from " $0 ";"} END {print "real_ip_header CF-Connecting-IP;"}' \
| sudo tee /etc/nginx/snippets/cloudflare-real-ip.conf
```

As of this writing that produces:

```nginx
set_real_ip_from 173.245.48.0/20;
set_real_ip_from 103.21.244.0/22;
set_real_ip_from 103.22.200.0/22;
set_real_ip_from 103.31.4.0/22;
set_real_ip_from 141.101.64.0/18;
set_real_ip_from 108.162.192.0/18;
set_real_ip_from 190.93.240.0/20;
set_real_ip_from 188.114.96.0/20;
set_real_ip_from 197.234.240.0/22;
set_real_ip_from 198.41.128.0/17;
set_real_ip_from 162.158.0.0/15;
set_real_ip_from 104.16.0.0/13;
set_real_ip_from 104.24.0.0/14;
set_real_ip_from 172.64.0.0/13;
set_real_ip_from 131.0.72.0/22;
set_real_ip_from 2400:cb00::/32;
set_real_ip_from 2606:4700::/32;
set_real_ip_from 2803:f800::/32;
set_real_ip_from 2405:b500::/32;
set_real_ip_from 2405:8100::/32;
set_real_ip_from 2a06:98c0::/29;
set_real_ip_from 2c0f:f248::/32;
real_ip_header CF-Connecting-IP;
```

Enable the site (`ln -s` into `sites-enabled`, `nginx -t`, `systemctl reload
nginx`), then `curl https://daily.hallada.net/healthz` should return `ok`.
Every response carries `Server-Timing: app;dur=<ms>`, the time the app spent
on the request; the browser's DevTools network panel shows it next to the
network timings, and `curl -sI` prints it, so origin work and the network can
be told apart without touching the logs. No
auth is needed at the proxy layer: rating links are self-authenticating (HMAC
tokens) and the OPDS/file routes use the app-level Basic auth from
`server.basic_auth_user`/`_pass` if you set them.

#### HTTP caching

The origin says what may be cached and for how long. No shared cache sits in
front of it today (the Cloudflare proxy was retired on 2026-09-05), but every
response still carries an explicit policy so the proxy can come back with a DNS
toggle and nothing else. The whole matrix:

| route | `Cache-Control` |
|---|---|
| `/`, `/issues`, `/issues/{date}`, `/feed.xml`, `/issues.json` (anonymous) | `public, max-age=300` |
| the same pages with a `daily_session=` cookie | `private, no-store` |
| `/robots.txt` | `public, max-age=86400` |
| `/static/*?v=<hash>` | `public, max-age=31536000, immutable` |
| `/static/*` with no `?v=` | `public, max-age=3600` |
| `/files/epub/*`, `/files/xtc/*`, `/opds*` (every status) | `private, no-store` |
| `/dashboard*`, `/login`, `/account`, errors, anything else | `no-store` |

Five minutes is the whole freshness story: an issue changes once a day, a
reader's tab or a CDN edge revalidates within five minutes of the new one
landing, and signed-in requests never touch a shared cache at all. A longer
edge age with an API purge after each publish was considered and dropped as
not worth its moving parts. The `no-store` on the last row is a default applied
by the `security_headers` middleware to any response that set no policy of its
own, so a route added later cannot silently inherit the CDN's default TTL.

### Installing the XTC converter

```sh
sudo git clone https://github.com/bigbag/epub-to-xtc-converter /opt/epub-to-xtc-converter
sudo npm --prefix /opt/epub-to-xtc-converter/cli install --omit=dev
sudo install -m0644 -o daily-epub -g daily-epub \
     xtc-settings.example.json /etc/daily-epub/xtc-settings.json
sudo -e /etc/daily-epub/xtc-settings.json     # point font.path at a font you have
```

`font.path` must be an existing TTF/OTF: the converter validates its settings
before opening the EPUB and exits 2 with `Font path is required` otherwise. There
is no built-in default, and the converter's own `cli/settings.json` points at a
GNOME font that most servers do not have. Check with `fc-list | grep -i serif`.

Then verify by hand before trusting the timer — **as `daily-epub`**, the account
the timer actually uses:

```sh
sudo -u daily-epub node /opt/epub-to-xtc-converter/cli/index.js convert \
     "/var/lib/daily-epub/out/The Daily EPUB - $(date +%F) (X4).epub" \
     -o /tmp/xtc-check.xtch -f xtch -c /etc/daily-epub/xtc-settings.json
```

> **Do not run this as yourself.** `/etc/daily-epub` is `0750 daily-epub:daily-epub`,
> so any other account — including your own — cannot even traverse into it. The
> converter loads its settings behind `fs.existsSync()`, which returns `false` on
> `EACCES` just as it does for a missing file, so an unreadable config is silently
> discarded and the built-in defaults (`font.path: null`) are used instead. You
> get `Font path is required` for a file that exists and is perfectly valid.
> Running as `daily-epub` both avoids the trap and proves the *service* can read
> everything it needs.

### The OPDS catalog

`daily-epub serve` publishes its own OPDS 1.2 acquisition feed at
**`https://daily.hallada.net/opds/daily.xml`** (the bare
`https://daily.hallada.net/opds`, with or without a trailing slash, serves the
same feed — one less thing to type on a seven-button keyboard). Point any OPDS
client — the X4's CrossPoint browser, KOReader, Calibre — at it and you get the
last 14 issues, newest first, **both editions of each**, with the standard
edition listed first and the files served from `/files/epub/`.

The feed is not a file. It is rendered per request by scanning
`publish.epub_dir` for `The Daily EPUB - YYYY-MM-DD*.epub`, so it cannot go
stale behind a failed publish and there is no generated index for the retention
sweep to step around. Entries are titled exactly like each EPUB's `dc:title`, so
the two editions of one issue are distinguishable in the list.

This is deliberately independent of BookOrbit: it needs only the directory, so
BookOrbit is optional, and it puts the day's issue one screen from the X4's home
instead of several clicks down a library tree.

For desktop reading, the issue page can show admins a **Read in BookOrbit**
button that opens the Standard edition in BookOrbit's web reader. Create an OPDS
user in BookOrbit under Settings → OPDS, put its name in `config.toml`, put
`DAILY_EPUB_BOOKORBIT__OPDS_PASS` in `/etc/daily-epub/env`, set
`bookorbit.enabled = true`, and restart `daily-epub.service`; no systemd change
is needed because the unit already allows loopback HTTP. Book and file ids are
looked up lazily on the first click and cached on the `issues` row; if BookOrbit
re-indexes a book, `/issues/<date>/read?refresh=1` clears the cache and resolves
the ids again.

### Why XTC is not in the feed

XTC files are still generated and still land in `publish.xtc_dir` — they are just
not advertised over OPDS, because **CrossPoint's OPDS browser can only acquire
EPUBs**. Two independent reasons, both in the firmware:

- its OPDS parser marks an entry as a book only when an acquisition link's `type`
  is exactly `application/epub+zip` (a `strcmp`), and drops the entry otherwise —
  which surfaces as *"No entries found"*;
- `OpdsBookBrowserActivity` hardcodes `.epub` as the saved filename regardless of
  the URL or `Content-Disposition`, and the reader dispatches on extension, so a
  downloaded `.xtch` would land under a name it then refuses to open.

XTC remains a first-class format *on the device* — the file browser lists
`.xtc`/`.xtch` and there is a dedicated XTC reader — so the artifacts stay
downloadable at `/files/xtc/<name>` (same Basic auth) for sideloading by SD card,
WebDAV or the device's web upload UI.

**Budget the disk.** XTCH is a pre-rendered 2-bit page bitmap — 480×800 px is
~96 KB per page regardless of what is on it — so an issue is large and its size
tracks the page count, which `font.size` drives:

| `font.size` | pages | `.xtch` | `.xtc` (1-bit) |
|---|---|---|---|
| 34 (converter default) | 1,088 | 104 MB | ~52 MB |
| 30 (`xtc-settings.example.json`) | 820 | 79 MB | ~39 MB |

Measured on the 20-article issue of 2026-08-15; conversion took ~13 s either way.
That is why `xtc_dir` is swept by **count** rather than age: at the default
`xtc_retention_count = 5` it holds ~0.4–0.5 GB, while the EPUBs beside it age out
on the much longer `retention_days`.

---

## Operational verification

Condensed from spec §5. Run it in this order the first time.

```sh
# 1. Config and schema
daily-epub --config /etc/daily-epub/config.toml config check   # roles → providers, keys present?
daily-epub --config /etc/daily-epub/config.toml db migrate

# 2. Ingest only, no keys spent: does Miniflux answer, and with how much?
DAILY_EPUB_OUT_DIR=./out daily-epub generate --dry-run --skip-llm --max-articles 6 --out ./out
#    → prints the window, per-feed entry counts, the lineup and $0.0000

# 3. Inspect the artifacts
ls -la ./out                       # two .epub files
epubcheck "./out/The Daily EPUB - $(date +%F).epub"   # expect zero errors
#    open the standard edition in Calibre / KOReader: cover, The Brief,
#    In This Issue, sections, discussions, Behind the paper, colophon; TOC depth 2

# 4. Now with the bulk and editor providers, still not publishing
daily-epub generate --dry-run --out ./out --max-articles 6
#    → check the lineup is sane (at most 6 picks, each with a "why" line) and the
#      printed per-provider cost is well under $1

# 5. Full live run
sudo systemctl start daily-epub-generate
journalctl -u daily-epub-generate -n 100 --no-pager
ls -la /srv/bookorbit/libraries/daily-epub /var/lib/daily-epub/xtc
#    → the issue appears in BookOrbit's UI under the Daily EPUB library only

# 6. Delivery
#    KOReader (Kindle/Palma): browse BookOrbit's OPDS, download, read.
#    Xteink X4 / CrossPoint: OPDS → https://daily.hallada.net/opds/daily.xml
curl -s https://daily.hallada.net/healthz
curl -s https://daily.hallada.net/opds/daily.xml | head
curl -s https://daily.hallada.net/issues.json | jq '.[0]'

# 7. Feedback loop: tap Loved it / Good / Not for me in KOReader, then
sqlite3 /var/lib/daily-epub/daily-epub.db 'select * from rating_events order by event_at desc;'

# 8. Watch cost and quality for a week
daily-epub stats --days 7
daily-epub explain --date $(date +%F) --near-misses
```

Tune `curation.ranking.deep_keep`, `target_article_count` and `curation.always_include_feeds`
from what you see in step 8, and read the paper's *Behind the paper* chapter
each morning: it is the same numbers, on the device.

### Troubleshooting

| Symptom | Likely cause |
|---|---|
| `ingesting entries from miniflux` error chain | wrong `miniflux.base_url`/API key, or Miniflux is down. Fatal by design. |
| Rating links return 403 | the issue was generated with a different (or missing) `server.hmac_secret` than the running server has. |
| `Read-only file system` while publishing | `ReadWritePaths` in the unit does not cover the configured publish dirs. |
| Run status `degraded` | a best-effort stage failed; the warnings are in the report (`/issues.json`, `runs.error`, the journal). |
| No XTC file | `xtc.enabled = false`, Node missing, wrong `xtc.args` path, or `xtc.settings` unset/pointing at a font that does not exist. Non-fatal — the X4 can read the X4 EPUB from BookOrbit instead. The report warning quotes the converter's own error. |
| `Font path is required` for a settings file that *does* set `font.path` | The process cannot read the file, and the converter cannot tell that apart from the file not existing. Almost always running the converter as yourself instead of `daily-epub` (see above), or a font path that has moved. `sudo -u daily-epub cat /etc/daily-epub/xtc-settings.json` and `sudo -u daily-epub test -r <font> && echo ok` settle it. |
| The X4's OPDS browser says "No entries found" | It fetched and parsed the feed but accepted no entry. Every acquisition link must be typed exactly `application/epub+zip`; anything else is dropped silently. `curl -s -u user:pass https://daily.hallada.net/opds/daily.xml \| grep -c "<entry>"` — zero means nothing has been published yet. |
| The X4's OPDS browser says "Failed to fetch feed" | The request never completed: wrong URL, TLS, or credentials. "Failed to parse feed" means malformed XML. The three messages are distinct — read which one you got. |
| No World Briefing | Retrieval begins with the previous calendar day (the latest completed page) and falls back up to `world::MAX_LOOKBACK_DAYS` days. A warning means those pages were empty or Wikipedia was unreachable. |
| An X4 cover still shows an old black band after regeneration | CrossInk caches its generated home-screen thumbnail under the EPUB path. Delete that book's cache before retesting the same filename, reopen the EPUB, then return to the Minimal home screen. |

---

## Development

```sh
cargo test                 # unit + integration, fully offline
cargo clippy --all-targets
cargo fmt
```

The crate is a library plus a thin binary, so tests drive the pipeline directly.
`tests/e2e_pipeline.rs` is the capstone: synthetic entries → dedupe → offline
extraction → signals → triage → admission → selection (both the `--skip-llm` route and a
`MockBackend` DeepSeek route) → editorial → both EPUB editions → publish → OPDS
and database rows, with no network access anywhere. `tests/m4_epub.rs` covers
the rendered chapters, including *Behind the paper* in both editions; the
`stats` and lock tests live next to their modules (`curate/telemetry.rs`,
`lock.rs`).

### Layout

`src/pipeline.rs` wires the stages; `src/types.rs` is the contract between them;
`src/auth.rs` owns the rating token formula, shared by the EPUB writer and the
server. The stages themselves:

```text
miniflux.rs   ingest            curate/       triage, assessment and selection
dedupe.rs     clustering          prefilter, llm, triage, admit, assess, rank,
                                  editor, editorial, embedding, signals, telemetry
extract.rs    body text           profile/    the reader's taste profile
images/       article images    comments.rs   discussion chapters
  normalize     usable <img>    world.rs      the world briefing
  refs          what's there    epub/         the two editions
  fetch         download          chapters, cover, build, x4
  encode        re-encode       publish.rs    BookOrbit + XTC
  embed         into the page   server.rs     ratings, OPDS
html.rs       markup helpers    db.rs         SQLite
lock.rs       one writer at a time (flock on <database_path>.lock)
```

Two modules are worth knowing about before you go looking for their contents.
`src/html.rs` holds the generic markup helpers — tag scanning, escaping, entity
decoding, XHTML fixups — that extraction, images, comments and the world briefing
all need; put anything that works on markup without caring what the markup is
*about* there. `src/images/` owns every stage of an article's images, which is
otherwise the kind of concern that smears itself across extraction and EPUB
building; see its module docs for the order the stages run in.

### Auditing images against real articles

Image handling fails in ways no synthetic fixture predicts, because every
publisher invents its own lazy-loading scheme. `examples/image_audit.rs` replays
the extraction and image pipeline over the articles of issues already published
and counts what actually reaches the page:

```sh
cargo run --release --example image_audit -- \
    --cache /tmp/pagecache ~/bookorbit/books/daily-epub/*.epub
```

It prints per-article `refs / embedded / shown / placeholders`, a tally of loss
reasons, and totals. Pages are cached on first run, so a change can be measured
against byte-identical input; `--dump <title substring>` lists the URLs one
article resolved to, and `--epub-out DIR` writes a readable EPUB of the audited
articles so the images can be looked at on a device rather than counted in a
table. It needs the network and is not part of `cargo test`.

---

## Known limitations

From spec §7, plus what implementation turned up:

- **X/Twitter social proof is omitted** — no free API. The `social.source` enum
  reserves `x` for the day that changes.
- **Lobsters linkage only works when the entry arrived via a lobste.rs feed** or
  carries a `lobste.rs/s/<id>` comments URL: there is no public URL-search API.
- **Paywalled articles degrade to an excerpt plus a link.** They are penalized in
  the pre-filter but not banned — strong social proof can still surface them.
- **The Xteink X4 cannot follow rating links** (no browser). Rating happens from
  KOReader devices; the X4 edition still carries the links harmlessly.
- **Reddit is rate-limited to ~1 request/second** and is skipped for the rest of
  the run after a 429. Social data is best-effort by design.
- **"Came via Scour" needs the feed URL.** It is detected from the live Miniflux
  feed map during a run; re-deriving it later from the `entries` table alone
  falls back to matching the feed title.
- **Images are downloaded once per edition** (the two editions need different
  resolutions and colour profiles), so an image-heavy issue makes two passes.
- **An image that cannot be embedded is dropped, not announced,** unless its alt
  text is a real description — decorative rules, spacers and dead links would
  otherwise litter the page with `[image: …]` lines. Verify image handling with
  `examples/image_audit.rs` after touching extraction.
- **`dc:date` rides inside a `dcterms:date` metadata fragment** because
  `epub-builder` neither exposes `dc:date` nor accepts a non-`chrono` date. The
  OPF output is correct; the mechanism is a workaround.
- **`async-openai` is not used.** The published crate exposes neither `Client` nor
  `types::chat` in a usable feature combination and would add a second HTTP
  stack, so `curate/llm.rs` speaks the OpenAI-compatible wire protocol over the
  shared `reqwest` client instead, behind a `ChatBackend` trait. The dependency
  was removed.
- **Two wire protocols are implemented**, `openai` (chat completions) and
  `anthropic` (Messages API), each a `ChatBackend` impl. Providers are config
  entries over those two kinds, each with its own `UsageMeter`, price table and
  ceiling; a protocol that is neither means another impl. Voyage AI embeddings
  sit behind the analogous `EmbeddingBackend` trait in `curate/embedding.rs`.
- **Triage and union admission replace the heuristic gate.** Every eligible
  article gets interest, rated-neighbour, feed-affinity, social and heuristic
  signals, then DeepSeek reads its opening (up to `triage_max`). The deep set is
  the union of triage, interest, neighbour, exploration, blend and auto-include
  retrievers. `explain` shows the assessment and `admitted_by`. Learned signals
  stay absent until their gates open (8 and 15 ratings respectively).
- **Deep assessment and diversity are live.** DeepSeek reads a representative
  beginning/middle/end sample, separates editorial quality from reader fit, and
  records descriptive facets. Utility is normalized over the deep set; embedding
  leader clusters cap near-duplicates before the 60-item editor shortlist.
- **DeepSeek's content filter rejects whole batches.** A `400 Content Exists
  Risk` refuses the entire request when any one article in it trips the input
  filter, without saying which. Triage and deep assessment therefore bisect a
  rejected batch (halves, then quarters, down to single articles) so the other
  articles keep their assessment. A single article the bulk provider still
  refuses is retried once on the editor provider with the same prompt when that
  is a different provider; if that also fails, an `article_assessments` row with
  `kind = 'provider_rejected'` and a NULL score is written so the article is not
  sent again for `assessment_reuse_days`, and it is ranked on its other signals.
  See them with `explain` (`triage: rejected by provider — deepseek: …`), the
  `triage:` / `assess:` log lines (`… 3 rejected (2 recovered on gemini)`), the
  ` · N rejected` suffix on the `curation:` line, or
  `sqlite3 /var/lib/daily-epub/daily-epub.db "select stage, count(*) from article_assessments where kind = 'provider_rejected' group by stage"`.
- **One shared reader profile, one issue per day.** Multiple login accounts and
  `user`/`admin` roles are supported, but they share one personalization model;
  there is no per-user issue or weekly/retrospective edition (spec §6).
