# Operator and developer shortcuts for The Daily EPUB.
#
#   just              list recipes
#   just de <args>    run any daily-epub subcommand on the server as the service user
#
# Server recipes run the binary the way the systemd units do: same user, working
# directory and EnvironmentFile (`/etc/daily-epub/env`, where the API keys live),
# via a transient `systemd-run` unit. Plain `sudo -u daily-epub` never loads that
# file, so `miniflux api key is not configured` from a hand-run command means the
# environment, not the config.

set positional-arguments

bin       := "/usr/local/bin/daily-epub"
config    := "/etc/daily-epub/config.toml"
env_file  := "/etc/daily-epub/env"
work_dir  := "/var/lib/daily-epub"
user      := "daily-epub"

# The systemd-run prefix every server recipe uses.
run := "sudo systemd-run --quiet --wait --pty --collect --uid=" + user + " --gid=" + user \
       + " -p WorkingDirectory=" + work_dir + " -p EnvironmentFile=" + env_file \
       + " " + bin + " --config " + config

default:
    @just --list --unsorted

# ---------------------------------------------------------------------------
# Server: any subcommand
# ---------------------------------------------------------------------------

# Run any daily-epub subcommand as the service user with the env file loaded.
de *args:
    {{run}} "$@"

# Validate the config as `generate` would; one fact per line.
config-check:
    {{run}} config check

# Run pending migrations (also automatic on every start).
db-migrate:
    {{run}} db migrate

# ---------------------------------------------------------------------------
# Server: issues
# ---------------------------------------------------------------------------

# Build and publish an issue by hand (same as the timer, but in the foreground).
generate *args:
    {{run}} generate "$@"

# Build everything, publish nothing, record no issue.
dry-run *args:
    {{run}} generate --dry-run "$@"

# Why an article was (or was not) in the paper, from persisted telemetry.
explain *args:
    {{run}} explain "$@"

# The weekly numbers: issues, ratings, retriever yield, cost, timing.
stats *args:
    {{run}} stats "$@"

# ---------------------------------------------------------------------------
# Server: feeds, ratings, profile, features
# ---------------------------------------------------------------------------

# Seed /dashboard/feeds from recent aggregator-only articles.
feeds-discover days="14" limit="50":
    {{run}} feeds discover --days {{days}} --limit {{limit}}

# List current explicit verdicts.
ratings-list *args:
    {{run}} ratings list "$@"

# Set a verdict: `just ratings-set <article-id> <loved|good|not_for_me> [--note ...]`.
ratings-set *args:
    {{run}} ratings set "$@"

# Clear a verdict: `just ratings-clear <article-id>`.
ratings-clear *args:
    {{run}} ratings clear "$@"

# Regenerate the taste profile from ratings history.
profile-rebuild:
    {{run}} profile rebuild

# Embed rated and published articles, then interests, into the cache.
features-backfill *args:
    {{run}} features backfill "$@"

# Drop stale embeddings, old telemetry and old assessments per the retention config.
features-prune:
    {{run}} features prune

# Re-poll social scores for recent entries.
backfill-social days="7":
    {{run}} backfill-social --days {{days}}

# ---------------------------------------------------------------------------
# Server: users and jobs
# ---------------------------------------------------------------------------

# List dashboard users and open-session counts.
users-list:
    {{run}} users list

# Add a user: `just users-add NAME [--admin]` (prompts for the password).
users-add *args:
    {{run}} users add "$@"

# Change a password and revoke sessions: `just users-passwd NAME`.
users-passwd *args:
    {{run}} users passwd "$@"

# Any other `users` subcommand: `just users role NAME admin`, `just users disable NAME`, ...
users *args:
    {{run}} users "$@"

# Run one catalogue job in-process (generate, dry-run, profile-rebuild, features-backfill, backfill-social, features-prune, import-ratings).
job name:
    {{run}} job run {{name}}

# Start a catalogue job as its systemd unit, the way the dashboard does.
job-unit name:
    sudo systemctl start daily-epub-job@{{name}}

# ---------------------------------------------------------------------------
# Server: services and logs
# ---------------------------------------------------------------------------

# Status of the web server, the generate timer and its last run.
status:
    systemctl status --no-pager daily-epub.service daily-epub-generate.timer daily-epub-generate.service || true
    systemctl list-timers --no-pager daily-epub-generate.timer

# Restart the web server (after installing a new binary or unit).
restart:
    sudo systemctl restart daily-epub.service

# Follow the web server log.
logs:
    journalctl -u daily-epub.service -f

# Last N lines of the most recent generate run.
logs-generate lines="120":
    journalctl -u daily-epub-generate.service -n {{lines}} --no-pager

# Follow one job unit's log: `just logs-job features-prune`.
logs-job name:
    journalctl -u daily-epub-job@{{name}} -f

# ---------------------------------------------------------------------------
# Deploy (run from a checkout on the server)
# ---------------------------------------------------------------------------

# Release build, install the binary and restart the web server.
deploy: build-release
    sudo install -m0755 target/release/daily-epub {{bin}}
    sudo systemctl restart daily-epub.service
    systemctl is-active daily-epub.service

# Install the systemd units and the polkit rule, then reload systemd.
install-units:
    sudo install -m0644 systemd/daily-epub.service systemd/daily-epub-generate.service \
        systemd/daily-epub-generate.timer systemd/daily-epub-job@.service /etc/systemd/system/
    sudo install -m0644 systemd/50-daily-epub.rules /etc/polkit-1/rules.d/
    sudo systemctl daemon-reload

# Edit the environment file (API keys and other secrets).
edit-env:
    sudo -e {{env_file}}

# Edit the config file as the service user would see it.
edit-config:
    sudo -e {{config}}

# ---------------------------------------------------------------------------
# Development (local checkout)
# ---------------------------------------------------------------------------

build:
    cargo build

build-release:
    cargo build --release

# Everything CI expects: format check, clippy, tests, and the committed CSS is current.
check: fmt-check clippy test css-check

test *args:
    cargo test "$@"

clippy:
    cargo clippy --all-targets

fmt:
    cargo fmt

fmt-check:
    cargo fmt --check

# Rebuild src/web/static/app.css from the Tailwind source (commit the result).
css:
    npm run css

# Fail if the committed app.css is out of date.
css-check:
    npm run css:check

# Rebuild app.css on every template change.
css-watch:
    npm run css:watch

# Seed a throwaway dev database under ./dev (admin/adminpassword123, reader/readerpassword123).
dev-seed:
    cargo run --example seed_dev_db -- ./dev

# Serve the dev database on http://127.0.0.1:3599.
dev-serve:
    DAILY_EPUB_SERVER__HMAC_SECRET=$(head -c 48 /dev/urandom | base64) \
        cargo run -- --config ./dev/config.toml serve

# Run any subcommand against the local dev config: `just dev config check`.
dev *args:
    cargo run -- --config ./dev/config.toml "$@"

# Replay extraction + images over published EPUBs: `just image-audit ~/books/*.epub`.
image-audit *args:
    cargo run --release --example image_audit -- --cache /tmp/pagecache "$@"
