# The Daily EPUB

A personalized daily newspaper, delivered as an EPUB.

Every morning a systemd timer wakes one Rust binary. It pulls the last ~26 hours
from a self-hosted [Miniflux](https://miniflux.app), deduplicates and extracts
the articles, enriches them with HackerNews/Lobsters/Reddit social proof, filters
300–500 candidates down to ~120 with cheap heuristics, and asks DeepSeek to score,
select and introduce 15–25 of them. It assembles two EPUB editions (a standard one
and one tuned for the Xteink X4 e-ink reader), converts the X4 edition to XTC, and
publishes the lot over its own OPDS catalog — which doubles as a
[BookOrbit](https://github.com/thallada/bookorbit) watched folder if you run one.
Each article chapter ends with 👍/👎 links that feed back into tomorrow's curation.

Steady-state cost is roughly **$0.05–0.30/day** in DeepSeek tokens, hard-capped by
`max_daily_usd`.

- Full design: [`docs/plans/2026-08-15-the-daily-epub.md`](docs/plans/2026-08-15-the-daily-epub.md)
- Implementation decisions: [`docs/plans/2026-08-15-implementation-notes.md`](docs/plans/2026-08-15-implementation-notes.md)

---

## Pipeline

```
Miniflux ingest ─▶ dedupe ─▶ extraction ─▶ persist ─▶ social enrichment
  ─▶ pre-filter ─▶ LLM scoring ─▶ selection ─▶ comments ─▶ editorial
  ─▶ world briefing ─▶ EPUB (standard + X4) ─▶ XTC ─▶ publish ─▶ report
```

Every stage writes to SQLite, so a run is idempotent per date: re-running
`generate --date 2026-08-15` replaces that issue rather than duplicating it.

**Failure policy.** Miniflux ingest, SQLite writes, EPUB assembly and publishing
are fatal — without them there is no issue, and the `runs` row records why.
Social lookups, comment fetching, the world briefing, images and the XTC
conversion are best-effort: they log, add a warning (run status `degraded`) and
the run continues. Every DeepSeek stage *degrades*: a missing key, a dead API or
a tripped budget turns the run into the `--skip-llm` shape (prefilter order
selects, feed excerpts stand in for summaries) instead of losing the day's issue.

---

## Prerequisites

| Requirement | Why | Notes |
|---|---|---|
| Rust (2024 edition toolchain) | building | `cargo build --release` |
| **Miniflux** with an API key | the only content source | Settings → API Keys. The client is read-only and never mutates read state. |
| **DeepSeek API key** | curation + editorial | <https://platform.deepseek.com>. Optional: `--skip-llm` runs the whole pipeline without it. |
| A 32+ byte random secret | signs the 👍/👎 rating links | `openssl rand -hex 32` |
| **BookOrbit** library + watched folder | *optional* — a richer library UI on top of the same folder | Delivery does not need it: `daily-epub serve` has its own OPDS catalog over `publish.epub_dir`. If you do run it, create a dedicated "The Daily EPUB" library, enable *Watch folders*, and point `publish.epub_dir` at it. |
| **Node.js 18+** and a clone of [`epub-to-xtc-converter`](https://github.com/bigbag/epub-to-xtc-converter) | XTC/XTCH output for the Xteink X4 | Optional (`xtc.enabled = false` turns it off). Needs `npm install` **inside `cli/`**, and a settings JSON naming a real TTF/OTF — see below. It has **no global npm bin** — it is invoked as `node <repo>/cli/index.js convert …`, which is why `xtc.command`/`xtc.args` are fully general. |
| A reverse proxy for `daily.hallada.net` → `127.0.0.1:3499` | rating links must be reachable from e-readers on the internet | TLS via your existing setup. |

`data/scour-interests.opml` (the ~220 Scour interests the taste profile is seeded
from) must be readable at the path in `interests_opml`.

---

## Install & build

```sh
git clone https://github.com/thallada/the-daily-epub && cd the-daily-epub
cargo build --release
cargo test                      # everything is offline; no keys needed
sudo install -m0755 target/release/daily-epub /usr/local/bin/
```

### Commands

```
daily-epub generate [--date YYYY-MM-DD] [--dry-run] [--out DIR] [--max-articles N] [--skip-llm]
daily-epub serve                # rating endpoints + OPDS catalog + downloads
daily-epub profile rebuild      # regenerate the taste profile from ratings (weekly inside generate)
daily-epub backfill-social      # re-poll social scores for recent articles
daily-epub db migrate           # run migrations (also automatic on every start)
```

`--dry-run` does everything except deliver: it still ingests, persists entries and
articles, curates and **builds both EPUBs into `--out`**, but it does not copy to
BookOrbit, does not run the retention sweep, does not write the `issues` row and
does not advance the ingest watermark. It prints the lineup and the cost report.

---

## Configuration

Start from [`config.example.toml`](config.example.toml). Load order, later wins:

1. built-in defaults
2. the TOML file (`--config PATH`, else `./config.toml` when present)
3. `DAILY_EPUB_*` environment variables

Nested keys use a **double underscore**: `[miniflux] api_key` becomes
`DAILY_EPUB_MINIFLUX__API_KEY`. Top-level keys are just uppercased:
`DAILY_EPUB_LOOKBACK_HOURS=30`. As a convenience, plain **`DAILY_EPUB_SECRET`**
is accepted as an alias for `server.hmac_secret` (the explicit key wins if both
are set).

Secrets belong in the environment file, never in the TOML.

### Reference

| Key | Default | Meaning |
|---|---|---|
| `timezone` | `America/New_York` | Day boundaries and `--date` interpretation. |
| `lookback_hours` | `26` | Size of the ingest window ending at the issue day's end (clamped to now). |
| `target_article_count` | `20` | Lineup size the selector aims for. `--max-articles` overrides it. |
| `prefilter_keep` | `120` | Candidates surviving the heuristic pre-filter. Must be ≥ `target_article_count`. |
| `retention_days` | `21` | EPUBs older than this are deleted from `publish.epub_dir`. SQLite history is kept forever. |
| `xtc_retention_count` | `5` | How many XTC issues to keep in `publish.xtc_dir`. Counted, not dated: each `.xtch` is ~80–100 MB, so the binding constraint is disk, not age. |
| `max_daily_usd` | `2.0` | Hard ceiling on DeepSeek spend **per day**, not per run — a re-run inherits what earlier runs for that date already spent. Tripping it skips remaining LLM work and degrades to excerpts. |
| `world_briefing` | `true` | Include the Wikipedia Current Events section. |
| `database_path` | `/var/lib/daily-epub/daily-epub.db` | SQLite file; parent dirs are created. |
| `out_dir` | `/var/lib/daily-epub/out` | Where `generate` writes artifacts before publishing. |
| `interests_opml` | `data/scour-interests.opml` | Scour interest export used to seed the taste profile. |
| `miniflux.base_url` | `http://127.0.0.1:8082` | Miniflux root (no `/v1`). |
| `miniflux.api_key` | — | **`DAILY_EPUB_MINIFLUX__API_KEY`**. Required. |
| `miniflux.page_limit` | `250` | Entries per page; Miniflux caps this at 250. |
| `deepseek.base_url` | `https://api.deepseek.com/v1` | OpenAI-compatible endpoint. |
| `deepseek.model` | `deepseek-v4-flash` | Verified 2026-08-15 (DeepSeek-V4-Flash-0731). |
| `deepseek.api_key` | — | **`DAILY_EPUB_DEEPSEEK__API_KEY`**. Absent ⇒ the run curates heuristically. |
| `deepseek.score_batch_size` | `12` | Articles per stage-A scoring request. |
| `deepseek.score_temperature` | `0.3` | Scoring/selection temperature. |
| `deepseek.editorial_temperature` | `0.8` | Summaries, intros, front page. |
| `deepseek.price_input_per_mtok` | `0.14` | USD per 1M cache-miss input tokens (cost guardrail arithmetic). |
| `deepseek.price_cached_input_per_mtok` | `0.0028` | USD per 1M prefix-cache-hit input tokens. |
| `deepseek.price_output_per_mtok` | `0.28` | USD per 1M output tokens. |
| `curation.always_include_feeds` | `[]` | Miniflux feed ids or URL substrings that can never be dropped. |
| `curation.blocked_domains` | `[]` | Hosts excluded outright. |
| `curation.paywall_domains` | `[]` | Extra paywalled hosts, merged with the built-in list (nytimes, wsj, ft, economist, …). |
| `curation.sections` | 8 sections | The **only** section names the model may use. `World Briefing` is reserved and never offered. |
| `publish.epub_dir` | `/srv/bookorbit/libraries/daily-epub` | Both EPUB editions land here by atomic copy, and this is the directory the OPDS feed lists. The editions are distinguished by a `(X4)` tag in **both** the filename and `dc:title` — libraries and OPDS clients list books by title, so the filename alone would make them look identical. Point a BookOrbit watched folder at it if you want its UI too. **Renamed from `bookorbit_dir`**; the old key is a hard config error. |
| `publish.xtc_dir` | `/var/lib/daily-epub/xtc` | XTC artifacts. **Not** listed in the OPDS feed — CrossPoint cannot acquire them — but downloadable at `/files/xtc/<name>` for sideloading. |
| `xtc.enabled` | `true` | Set `false` to skip the converter entirely. |
| `xtc.command` | `node` | Converter executable. |
| `xtc.args` | `["/opt/epub-to-xtc-converter/cli/index.js", "convert"]` | Prefix; the code appends `<input.epub> -o <output> -f <format>` (plus `-c <settings>`). |
| `xtc.format` | `xtch` | `xtc` (1-bit) or `xtch` (2-bit grayscale). `xtch` is ~96 KB per rendered page, `xtc` half that. |
| `xtc.settings` | unset | Settings JSON passed as `-c`. The flag is optional to the converter but the file is **required in practice**: without `font.path` the converter exits 2 before doing any work. Start from [`xtc-settings.example.json`](xtc-settings.example.json). |
| `server.bind` | `127.0.0.1:3499` | Listen address. |
| `server.public_url` | `https://daily.hallada.net` | Base URL the rating links inside the EPUB are built from. |
| `server.hmac_secret` | — | **`DAILY_EPUB_SERVER__HMAC_SECRET`** (or `DAILY_EPUB_SECRET`). Without it, generated links are rejected with 403. |
| `server.basic_auth_user` / `_pass` | unset | Optional Basic auth for `/opds/*` and `/files/*`. |

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
DAILY_EPUB_DEEPSEEK__API_KEY=…
DAILY_EPUB_SERVER__HMAC_SECRET=$(openssl rand -hex 32)
EOF
sudo chown daily-epub:daily-epub /etc/daily-epub/env && sudo chmod 0600 /etc/daily-epub/env

# publish dirs must exist and be writable by the service user
sudo install -d -o daily-epub -g daily-epub /var/lib/daily-epub/xtc
sudo setfacl -m u:daily-epub:rwx /srv/bookorbit/libraries/daily-epub   # or chown

# units
sudo install -m0644 systemd/daily-epub.service systemd/daily-epub-generate.service \
                    systemd/daily-epub-generate.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now daily-epub.service daily-epub-generate.timer
```

> **Keep `ReadWritePaths` in sync.** Both units run under `ProtectSystem=strict`
> and list the publish directories explicitly:
> ```
> ReadWritePaths=/home/thallada/bookorbit/books/daily-epub /var/lib/daily-epub/xtc
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
With an existing certificate, a minimal nginx site is:

```nginx
server {
    listen 443 ssl;
    listen [::]:443 ssl;
    server_name daily.hallada.net;

    ssl_certificate     /etc/letsencrypt/live/daily.hallada.net/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/daily.hallada.net/privkey.pem;

    # XTCH files can be tens of MB; don't buffer them through nginx memory.
    proxy_max_temp_file_size 0;

    location / {
        proxy_pass http://127.0.0.1:3499;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}

server {
    listen 80;
    listen [::]:80;
    server_name daily.hallada.net;
    return 301 https://$host$request_uri;
}
```

Enable it (`ln -s` into `sites-enabled`, `nginx -t`, `systemctl reload nginx`),
then `curl https://daily.hallada.net/healthz` should return `ok`. No auth is
needed at the proxy layer: rating links are self-authenticating (HMAC tokens)
and the OPDS/file routes use the app-level Basic auth from
`server.basic_auth_user`/`_pass` if you set them.

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
daily-epub --config /etc/daily-epub/config.toml db migrate

# 2. Ingest only, no keys spent: does Miniflux answer, and with how much?
DAILY_EPUB_OUT_DIR=./out daily-epub generate --dry-run --skip-llm --max-articles 6 --out ./out
#    → prints the window, per-feed entry counts, the lineup and $0.0000

# 3. Inspect the artifacts
ls -la ./out                       # two .epub files
epubcheck "./out/The Daily EPUB - $(date +%F).epub"   # expect zero errors
#    open the standard edition in Calibre / KOReader: cover, From the Editor,
#    In This Issue, sections, discussions, colophon; TOC depth 2

# 4. Now with DeepSeek, still not publishing
daily-epub generate --dry-run --out ./out --max-articles 6
#    → check the lineup is sane and the printed cost is well under $0.50

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

# 7. Feedback loop: tap 👍 in KOReader, then
sqlite3 /var/lib/daily-epub/daily-epub.db 'select * from ratings;'
sqlite3 /var/lib/daily-epub/daily-epub.db 'select * from feed_priors;'

# 8. Watch cost and quality for a week
sqlite3 /var/lib/daily-epub/daily-epub.db \
  'select date, status, entries_fetched, candidates, selected, cost_usd from runs order by id desc limit 7;'
```

Tune `prefilter_keep`, `target_article_count` and `curation.always_include_feeds`
from what you see in step 8.

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
extraction → prefilter → selection (both the `--skip-llm` route and a
`MockBackend` DeepSeek route) → editorial → both EPUB editions → publish → OPDS
and database rows, with no network access anywhere.

Layout: `src/pipeline.rs` wires the stages; `src/{dedupe,extract,social,curate,
comments,world,epub,publish,server}` implement them; `src/auth.rs` owns the rating
token formula used by both the EPUB writer and the server; `src/types.rs` is the
contract between stages.

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
- **`dc:date` rides inside a `dcterms:date` metadata fragment** because
  `epub-builder` neither exposes `dc:date` nor accepts a non-`chrono` date. The
  OPF output is correct; the mechanism is a workaround.
- **`async-openai` is not used.** The published crate exposes neither `Client` nor
  `types::chat` in a usable feature combination and would add a second HTTP
  stack, so `curate/llm.rs` speaks the OpenAI-compatible wire protocol over the
  shared `reqwest` client instead, behind a `ChatBackend` trait. The dependency
  was removed.
- **Only DeepSeek is wired.** Another provider means another `ChatBackend` impl.
- **No embedding-based personal ranker yet** (spec §3.9 future work); the schema
  is ready for it once ~200 ratings exist.
- **One reader, one issue per day.** There is no multi-user support and no
  weekly/retrospective edition (spec §6).
