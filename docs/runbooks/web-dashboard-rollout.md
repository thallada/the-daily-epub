# Runbook — web dashboard rollout

**Written:** 2026-09-03 for the production host. Run these commands from the checked-out
`web-dashboard` tree as an operator with `sudo`. The existing curation-v2 migration should
already be complete.

## 1. Build and install

The deployed binary does not require Node. For UI development, edit
`src/web/tailwind.css`, the templates, or `src/web/static/app.js`, then run
`npm run css` and commit the regenerated `src/web/static/app.css` alongside the
source. The `/static/*.css|js` URLs carry a hash of the embedded assets
(`web::ASSET_VERSION`), so a new build busts browser caches on its own — no
hard refresh and no version bump needed. CI and release preparation should run `npm run css:check` to detect
stylesheet drift.

```sh
npm ci
npm run css:check
cargo test
cargo build --release
sudo install -m0755 target/release/daily-epub /usr/local/bin/daily-epub
daily-epub --help
```

Confirm the help includes both `users` and `job`. Keep the previous binary available until the
smoke tests below pass.

## 2. Install the job unit and polkit rule

```sh
sudo install -m0644 systemd/daily-epub-job@.service /etc/systemd/system/
sudo install -m0644 systemd/50-daily-epub.rules /etc/polkit-1/rules.d/
sudo install -m0644 systemd/daily-epub.service /etc/systemd/system/
sudo systemctl daemon-reload
```

The server unit must contain both:

```ini
SupplementaryGroups=systemd-journal
ReadWritePaths=/home/thallada/bookorbit/books/daily-epub /var/lib/daily-epub/xtc /etc/daily-epub
```

Keep the two publish paths aligned with `[publish]`. The polkit rule permits user `daily-epub`
to start only `daily-epub-job@[a-z0-9-]+.service`; it does not grant stop, restart, or arbitrary
unit management.

## 3. Verify writable paths and ownership

Settings saves create a temporary file and rename it into `/etc/daily-epub`, so the directory
and config file must both belong to the service account:

```sh
sudo chown daily-epub:daily-epub /etc/daily-epub /etc/daily-epub/config.toml
sudo chmod 0750 /etc/daily-epub
sudo chmod 0640 /etc/daily-epub/config.toml
sudo install -d -m0750 -o daily-epub -g daily-epub /var/lib/daily-epub/data
sudo test -f /var/lib/daily-epub/data/profile.md
sudo -u daily-epub test -w /var/lib/daily-epub/data/profile.md
```

Set `profile_path = "/var/lib/daily-epub/data/profile.md"`; `StateDirectory=daily-epub` makes
that location writable. Also re-check the configured publish directories as `daily-epub`.

## 4. Review the new server settings

All keys have defaults, so additions are optional unless production needs overrides:

```toml
[server]
bind = "127.0.0.1:3499"
public_url = "https://daily.hallada.net"
session_days = 30
login_attempts = 10
login_window_minutes = 15
jobs_enabled = true
journal_lines = 300
```

Keep the bind address on loopback: the login throttle trusts `X-Forwarded-For`, so only the
reverse proxy should be able to reach the listener. Keep secrets in `/etc/daily-epub/env`, not
TOML. If `server.basic_auth_user` and `server.basic_auth_pass` are omitted, the existing
`/opds/*` and `/files/*` routes remain public; this is intentional so OPDS acquisition links
continue to work. When Basic auth is configured, signed-in users can still download files with
their session.

## 5. Restart, migrate, and verify public pages

```sh
sudo systemctl restart daily-epub.service
sudo systemctl --no-pager --full status daily-epub.service
curl -fsS https://daily.hallada.net/ | grep -F 'The Daily EPUB'
curl -fsS https://daily.hallada.net/feed.xml >/tmp/daily-epub-feed.xml
python3 -c 'import xml.etree.ElementTree as E; E.parse("/tmp/daily-epub-feed.xml")'
```

Migration `0004_web.sql` applies automatically at start. Anonymous issue pages and the Atom
feed must show the source-link index without summaries, article bodies, comments, the Brief, or
World Briefing text.

## 6. Bootstrap the administrator

```sh
sudo -u daily-epub /usr/local/bin/daily-epub \
  --config /etc/daily-epub/config.toml users add tyler --admin
sudo -u daily-epub /usr/local/bin/daily-epub \
  --config /etc/daily-epub/config.toml users list
```

The first command prompts twice without echo. Log in at `https://daily.hallada.net/login`, open
`/dashboard`, and confirm `/dashboard/users` shows the admin and an open session. A non-admin
account should receive the site's 403 page for every dashboard route.

## 7. Smoke test a job

Start `features-prune` from `/dashboard/jobs`, open its job detail, and watch the status and log
tail. On the host, confirm the same unit:

```sh
systemctl status daily-epub-job@features-prune.service
journalctl -u daily-epub-job@features-prune.service -n 100 --no-pager
```

If the page reports a permission failure, inspect `journalctl -u polkit` and verify the installed
rule and exact unit name. Do not broaden the rule to arbitrary units.

## 8. Smoke test an in-place settings write

In `/dashboard/settings`, change `curation.ranking.utility_protected` by one and save. Confirm
the next view shows the new value, comments and ordering remain in the file, and the attributed
change appears at `/dashboard/settings/history`:

```sh
sudo -u daily-epub grep -n 'utility_protected' /etc/daily-epub/config.toml
```

Change the value back through the page and confirm the second history row. Environment-overridden
fields should be locked, and no API key, HMAC secret, or Basic-auth password should be displayed.

## 9. Choose the history window

Candidate and assessment history is pruned after
`curation.ranking.telemetry_retention_days` (180 by default). Raise it now if the dashboard
should retain a longer article/run history, then reload `/dashboard/settings` and verify the
effective value. This changes future pruning only; it cannot restore rows already deleted.
