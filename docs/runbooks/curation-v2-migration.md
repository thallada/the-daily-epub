# Runbook — upgrading the server from v1 to curation v2

**Written:** 2026-09-02, for the `curation-v2` branch (all seven plan steps plus the provider
registry). Every command below is meant to be run on the server, as `root` via `sudo`, unless it
says otherwise. Expect the whole thing to take about an hour, most of it waiting on the embedding
backfill and one dry run.

What changes for the operator, in one paragraph: the binary is replaced; the SQLite schema gains
tables and drops `ratings`, `feed_priors` and `scores` (the migration copies your ratings first);
`config.toml` loses a few keys and gains the `[llm]` / `[providers.*]` registry plus a
`profile_path`; the env file gains two API keys and renames the DeepSeek one; a hand-maintained
`profile.md` is installed next to the OPML; the systemd units are unchanged.

## 0. Before touching the server

On the dev box:

```sh
git checkout curation-v2
cargo test                      # expect green
cargo build --release           # or build on the server, as you do today
```

The crate uses `edition = "2024"` and let-chains, so the server's toolchain must be current
(`rustup update stable`). Read `data/profile.md` once; it is the reader profile you will be
editing by hand from now on, and the first thing to tune when the paper feels off.

## 1. Freeze the timer, back everything up

```sh
sudo systemctl stop daily-epub-generate.timer daily-epub-generate.service
sudo systemctl stop daily-epub.service          # the server also runs migrations on start

sudo install -d -m0750 -o daily-epub -g daily-epub /var/lib/daily-epub/backup
sudo -u daily-epub sqlite3 /var/lib/daily-epub/daily-epub.db \
  ".backup '/var/lib/daily-epub/backup/daily-epub-pre-v2-$(date +%F).db'"
sudo cp -a /etc/daily-epub/config.toml /etc/daily-epub/config.toml.v1
sudo cp -a /etc/daily-epub/env         /etc/daily-epub/env.v1
sudo cp -a /usr/local/bin/daily-epub   /usr/local/bin/daily-epub.v1
```

`.backup` is the safe way to copy a WAL-mode database; a plain `cp` while anything is open can
miss the WAL. The migration is one-way (`ratings` is dropped after being copied), so this backup
is the rollback path.

## 2. Install the new binary

```sh
sudo install -m0755 target/release/daily-epub /usr/local/bin/
daily-epub --version
daily-epub --help                 # confirm `config`, `stats`, `explain`, `ratings`, `features` exist
```

Do **not** start any service yet: every subcommand except `config check` opens the database and
applies pending migrations, and you want the config right first.

The units in `systemd/` did not change. If you want to be sure your installed copies match:

```sh
diff systemd/daily-epub-generate.service /etc/systemd/system/daily-epub-generate.service
diff systemd/daily-epub.service          /etc/systemd/system/daily-epub.service
```

## 3. Edit `config.toml` in place

Everything you do not mention keeps its documented default, so the edit is small. Open it with
`sudo -e /etc/daily-epub/config.toml` and apply this checklist.

**Remove** (each one now fails loudly at startup, naming its replacement):

| Old key | Why |
|---|---|
| `prefilter_keep = …` (top level) | replaced by `curation.ranking.deep_keep` (default 120) |
| `max_daily_usd = …` (top level) | now per provider: `providers.deepseek.max_daily_usd` |
| the whole `[deepseek]` table | becomes `[providers.deepseek]` + `[llm]` (see below) |
| any `[anthropic]` table (only if you added one from an interim build) | becomes `[providers.anthropic]` |

**Add** near the top, next to `interests_opml`:

```toml
profile_path = "/var/lib/daily-epub/data/profile.md"
```

Use an absolute path. The default is `data/profile.md` *relative to the working directory*, which
under the unit is `/var/lib/daily-epub`, so the default would resolve to the same place, but an
explicit path survives running one-off commands from another directory. Point
`interests_opml` at an absolute path too if it is still relative.

**Add** the LLM registry. Carry over the `base_url`, `model` and `price_*` values from your old
`[deepseek]` table if you had changed them; the values shown are the defaults.

```toml
[llm]
bulk = "deepseek"            # triage, deep assessment, and the fallback for every editor call
editor = "anthropic"         # lineup, summaries, the Brief, the weekly profile rebuild
triage_batch_size = 25
deep_batch_size = 8
score_temperature = 0.3
editorial_temperature = 0.8

[providers.deepseek]
kind = "openai"
base_url = "https://api.deepseek.com/v1"
model = "deepseek-v4-flash"
max_daily_usd = 2.0
max_concurrent_requests = 4
price_input_per_mtok = 0.14
price_cache_read_per_mtok = 0.0028
price_cache_write_per_mtok = 0.0
price_output_per_mtok = 0.28

[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"
model = "claude-opus-5"
effort = "high"
max_daily_usd = 3.0
max_concurrent_requests = 4
price_input_per_mtok = 5.0
price_cache_read_per_mtok = 0.5
price_cache_write_per_mtok = 6.25
price_output_per_mtok = 25.0
```

Optional, for the Gemini comparison (section 9):

```toml
[providers.gemini]
kind = "openai"              # Gemini's OpenAI-compatible endpoint
base_url = "https://generativelanguage.googleapis.com/v1beta/openai"
model = "gemini-3.8-flash"
effort = "high"
max_daily_usd = 3.0
max_concurrent_requests = 4
price_input_per_mtok = 0.75          # $1.50 from 2027-01-01
price_cache_read_per_mtok = 0.075    # $0.15 from 2027-01-01
price_cache_write_per_mtok = 0.0
price_output_per_mtok = 3.75         # includes thinking tokens; $7.50 from 2027-01-01
```

**Leave alone** `[miniflux]`, `[server]`, `[publish]`, `[xtc]`, `[world]`, `[curation]
sections`, `always_include_feeds`, `blocked_domains`. `[voyage]`, `[editorial]`,
`[curation.feedback]` and `[curation.ranking]` all have sensible defaults; copy a section from
`config.example.toml` only when you want to change a value in it.

## 4. Edit the env file

```sh
sudo -e /etc/daily-epub/env
```

| Variable | Action |
|---|---|
| `DAILY_EPUB_DEEPSEEK__API_KEY` | **rename** to `DAILY_EPUB_PROVIDERS__DEEPSEEK__API_KEY` (the old name is rejected at startup so it cannot silently disable the bulk model) |
| `DAILY_EPUB_PROVIDERS__ANTHROPIC__API_KEY` | add |
| `DAILY_EPUB_VOYAGE__API_KEY` | add |
| `DAILY_EPUB_PROVIDERS__GEMINI__API_KEY` | add only if you configured `[providers.gemini]` |
| `DAILY_EPUB_MINIFLUX__API_KEY`, `DAILY_EPUB_SERVER__HMAC_SECRET` | unchanged |

Keep it `0600 daily-epub:daily-epub`. Then set hard spend limits in the DeepSeek, Anthropic and
Voyage dashboards: the in-app `max_daily_usd` meters are runaway guards, not accounting.

## 5. Install the reader profile

```sh
sudo install -d -m0750 -o daily-epub -g daily-epub /var/lib/daily-epub/data
sudo install -m0640 -o daily-epub -g daily-epub data/profile.md /var/lib/daily-epub/data/profile.md
# if the OPML is not already there:
sudo install -m0640 -o daily-epub -g daily-epub data/scour-interests.opml /var/lib/daily-epub/data/
```

If the file is missing the run does not fail; it logs a warning and uses the OPML interests only,
which is a much worse prompt. `config check` in the next step tells you whether it was found.

## 6. Check the config as the service user

The units run as `daily-epub` with the env file loaded, so check the same way. This helper runs
one command in that identity, with the env file and the working directory the unit uses, and no
other hardening:

```sh
de() { sudo systemd-run --quiet --wait --pty --collect \
        --uid=daily-epub --gid=daily-epub \
        -p WorkingDirectory=/var/lib/daily-epub -p EnvironmentFile=/etc/daily-epub/env \
        /usr/local/bin/daily-epub --config /etc/daily-epub/config.toml "$@"; }

de config check
```

Expected: every line is a fact, no line starts with `!`. Fix anything marked `MISSING` (a key
name, a path) before continuing. A stale key in the TOML is reported as an error naming its
replacement; go back to section 3.

## 7. Migrate the schema

```sh
de db migrate
sudo -u daily-epub sqlite3 /var/lib/daily-epub/daily-epub.db '.tables'
#   expect rating_events, article_embeddings, interest_embeddings, article_assessments,
#   candidate_runs; no ratings, feed_priors or scores
sudo -u daily-epub sqlite3 /var/lib/daily-epub/daily-epub.db \
  "select label, count(*) from rating_events group by label;"
#   your old votes: up → loved, down → not_for_me, source = 'migration'
```

## 8. Warm the embedding cache, then a dry run

```sh
de features backfill --rated-only          # the learned set; prints a token estimate first
de features backfill --days 30             # recent articles, so day one is not all cache misses
de generate --dry-run --out /var/lib/daily-epub/out-check
```

The dry run makes real DeepSeek, Claude and Voyage calls but publishes nothing and writes no
`issues` row. Read the printed lineup and the four report lines (`curation:`, `admission:`,
`preference:`, `providers:`); the cost should be well under $1. Then read the EPUB it wrote
(Calibre or KOReader): The Brief, the `Why it's here` line under each headline, and the new
Behind-the-paper chapter before the colophon. Finally:

```sh
de explain --date "$(date +%F)" --near-misses
```

Remove `/var/lib/daily-epub/out-check` when done.

## 9. Go live

```sh
sudo systemctl daemon-reload
sudo systemctl start daily-epub.service
sudo systemctl enable --now daily-epub-generate.timer
sudo systemctl list-timers daily-epub-generate.timer
```

If you want today's paper regenerated by the new pipeline now rather than tomorrow at 05:30:
`sudo systemctl start daily-epub-generate`. A same-date rerun replaces today's issue and is a
new run id in telemetry.

Afterwards:

```sh
journalctl -u daily-epub-generate -n 60 --no-pager     # the four-line info block near the end
de stats --days 14
```

## 10. Comparing editors (Claude Opus 5 vs Gemini 3.8 Flash)

With `[providers.gemini]` and its key in place, an A/B needs no config edit: environment variables
override the TOML, so

```sh
sudo systemd-run --quiet --wait --pty --collect --uid=daily-epub --gid=daily-epub \
  -p WorkingDirectory=/var/lib/daily-epub -p EnvironmentFile=/etc/daily-epub/env \
  -E DAILY_EPUB_LLM__EDITOR=gemini \
  /usr/local/bin/daily-epub --config /etc/daily-epub/config.toml \
  generate --dry-run --date "$(date +%F)" --out /var/lib/daily-epub/out-gemini
```

produces the same date's paper with Gemini as the editor. Triage and deep assessments are cached
for three days, so the second run costs only the editor, summaries and the Brief. Compare the two
lineups, the `why` lines and the Brief side by side, and the `providers:` cost line. To switch
for good, set `editor = "gemini"` in `[llm]` (and `summary_model` stays `editor`, so summaries
move with it). The same trick works for the bulk role: `DAILY_EPUB_LLM__BULK=gemini`.

## Rollback

```sh
sudo systemctl stop daily-epub-generate.timer daily-epub.service
sudo install -m0755 /usr/local/bin/daily-epub.v1 /usr/local/bin/daily-epub
sudo cp -a /etc/daily-epub/config.toml.v1 /etc/daily-epub/config.toml
sudo cp -a /etc/daily-epub/env.v1         /etc/daily-epub/env
sudo -u daily-epub cp /var/lib/daily-epub/backup/daily-epub-pre-v2-<date>.db /var/lib/daily-epub/daily-epub.db
sudo rm -f /var/lib/daily-epub/daily-epub.db-wal /var/lib/daily-epub/daily-epub.db-shm
sudo systemctl start daily-epub.service daily-epub-generate.timer
```

The database restore is required, not optional: the v1 binary expects `ratings`, `feed_priors`
and `scores`, which v2's migrations drop.
