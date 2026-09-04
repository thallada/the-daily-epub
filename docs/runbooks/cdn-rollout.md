# Runbook — putting daily.hallada.net behind Cloudflare

**Written:** 2026-09-04 for the production host. Steps 1–3 happen in the Cloudflare
and registrar dashboards; steps 4–5 are on the server as an operator with `sudo`.
The origin-side changes (the `Cache-Control` matrix below) ship in the same
release; deploy the new binary before enabling the proxy.

Read the whole thing before starting step 1: a nameserver change is the one step
that cannot be undone in seconds.

## 0. What the origin already does

The app decides what is cacheable; Cloudflare is configured only to obey it.

| route | `Cache-Control` |
|---|---|
| `/`, `/issues`, `/issues/{date}`, `/feed.xml`, `/issues.json` (anonymous) | `public, max-age=300` |
| the same pages with a `daily_session=` cookie | `private, no-store` |
| `/robots.txt` | `public, max-age=86400` |
| `/static/*?v=<hash>` | `public, max-age=31536000, immutable` |
| `/static/*` with no `?v=` | `public, max-age=3600` |
| `/files/epub/*`, `/files/xtc/*`, `/opds*` (every status, 401 and 404 included) | `private, no-store` |
| `/dashboard*`, `/login`, `/account`, error pages, anything else | `no-store` |

Two facts follow from that table and matter for every choice below:

- A public page is fresh for **five minutes** everywhere: browser and edge alike
  revalidate within five minutes of a new issue landing, and nothing has to tell
  the edge that the day changed. (A one-day edge age with an API purge after
  every publish was tried and dropped: at this traffic an edge rarely keeps a
  page long enough for the longer age to matter, and the purge was one more
  token, config section and failure mode.)
- Anything gated by a cookie or Basic auth already says `private, no-store`, so
  even a misconfigured cache-everything rule cannot make a download public. The
  cookie bypass rule in step 3 is defence in depth, not the only defence.

## 1. Move DNS to Cloudflare

The Free plan has no partial (CNAME) setup, so this is a full nameserver move:
Cloudflare becomes authoritative for the entire `hallada.net` zone, not just this
host. Everything else in the zone must survive the move unchanged.

1. Export the current zone from Route 53 as BIND:

   ```sh
   ZONE=$(aws route53 list-hosted-zones-by-name --dns-name hallada.net. \
        --query 'HostedZones[0].Id' --output text | cut -d/ -f3)
   aws route53 list-resource-record-sets --hosted-zone-id "$ZONE" > /tmp/hallada-net.json
   ```

   The AWS CLI has no BIND exporter; the console's **Export zone file** button on
   the hosted zone does produce one. Keep both files — the JSON is the rollback
   reference.

2. In Cloudflare, **Add a site** → `hallada.net` → Free plan → **Import DNS
   records** and upload the BIND file. Then read the imported list against the
   export line by line. Pay attention to `MX`, `TXT` (SPF/DKIM/DMARC), and any
   `CNAME` used for verification: those must all stay **DNS only** (grey cloud).

3. Set **only** the `daily` record to **Proxied** (orange cloud). Nothing else in
   the zone should be proxied — proxying an MX target or a mail-related host
   breaks it.

4. Change the nameservers at the registrar to the two Cloudflare assigns.
   Propagation is usually minutes and can be hours. Cloudflare emails when the
   zone goes active. **Keep `daily` DNS-only until the zone is Active *and*
   SSL/TLS → Edge Certificates shows the Universal certificate as Active.** A
   proxied host with no edge certificate yet fails every TLS handshake
   (`SSL alert number 40`) while plain HTTP still redirects, which is exactly
   what happened on the first attempt: readers whose resolvers had picked up
   the new nameservers got a broken site. Set Full (strict) (step 2) before
   flipping the record to Proxied.

5. Before the switch, drop the TTL on the `daily` record (60 s) so a rollback is
   quick. Verify afterwards:

   ```sh
   dig +short NS hallada.net
   dig +short daily.hallada.net           # Cloudflare anycast addresses once proxied
   dig +short MX hallada.net              # unchanged
   ```

## 2. Zone settings

Under the zone's **SSL/TLS**, **Speed** and **Scrape Shield** sections:

- **SSL/TLS → Overview → Full (strict)**. The origin has a real Let's Encrypt
  certificate, so there is no reason to accept anything weaker. *Flexible* would
  make Cloudflare talk plain HTTP to the origin and would break the redirect in
  the nginx `:80` server block into a loop.
- **Caching → Configuration → Browser Cache TTL → Respect Existing Headers.**
  This one is easy to miss and quietly wrong: the default (**4 hours**) *raises*
  the `max-age` Cloudflare sends to browsers, so the deliberate 5-minute browser
  age on the public pages would become 4 hours and a reader's tab would show a
  stale paper for most of a morning.
- **Speed → Optimization → Rocket Loader: OFF.**
- **Scrape Shield → Email Address Obfuscation: OFF.**
  Both inject a Cloudflare-hosted script into the HTML. The site's CSP is
  `script-src 'self'`, so the browser blocks the injected script and the page
  either breaks or logs CSP violations on every load. There is nothing to gain:
  the site has no email addresses in its markup and its own JS is two small
  files.
- **Do not enable Web Analytics with automatic injection** — same reason, it is
  an injected third-party script. If analytics are wanted later, they have to be
  first-party and listed in the CSP.
- **Always Use HTTPS: ON** is fine and lets the origin's `:80` block stay as a
  backstop.

## 3. Cache Rules

**Caching → Cache Rules.** Order matters: rule 1 must sit above rule 2.

**Rule 1 — "Bypass cache for signed-in readers"**

- When incoming requests match: `http.cookie contains "daily_session="`
- Then: **Bypass cache**

The origin already sends `private, no-store` to a cookie-bearing request, but
this makes the bypass a property of the request rather than of the response, so
nothing is ever *looked up* in a shared cache for a signed-in reader.

**Rule 2 — "Cache by origin headers"** *(optional)*

Without this rule Cloudflare caches only its default static extensions (the
CSS, JS, fonts and SVG here) and passes HTML straight through, which already
captures most of the benefit: TLS terminates at the edge and the origin is
reached over a warm connection. Add the rule if you want the HTML itself
served from the edge; it is only safe together with rule 1.

- When incoming requests match: `http.host eq "daily.hallada.net"`
- Then: **Eligible for cache**
- **Edge TTL:** *Use cache-control header if present, bypass cache if not*
- **Browser TTL:** *Respect origin*

That Edge TTL mode is the whole point of the origin work: a response with
`max-age` is cached for exactly that long, and a response with `no-store` (or
one that somehow arrives with no policy at all) is not cached. It is the reason
the `security_headers` middleware defaults unknown routes to `no-store` — with
this mode, "no header" means "do not cache" rather than "cache for 2 hours".

Leave **Cache Key** at its default. Do not add "Ignore query string": the
`?v=<hash>` on `/static/*` is what makes the immutable one-year `max-age` safe.

## 4. Lock the origin to Cloudflare

Once traffic arrives through the edge, direct hits on the origin's port 443
should stop. Either is enough; the first is simpler.

- **Firewall:** allow 443 only from the published Cloudflare ranges.

  ```sh
  { curl -s https://www.cloudflare.com/ips-v4; echo; \
    curl -s https://www.cloudflare.com/ips-v6; echo; } \
  | awk 'NF {print $0}' \
  | xargs -I{} sudo ufw allow proto tcp from {} to any port 443 comment 'cloudflare'
  sudo ufw delete allow 443/tcp        # remove the open rule last
  ```

  Keep a way in that does not depend on this (SSH from your own address) before
  removing the open rule.

- **Authenticated Origin Pulls** (SSL/TLS → Origin Server) instead: install
  Cloudflare's client CA on the origin and add `ssl_client_certificate` +
  `ssl_verify_client on` to the nginx server block. Stronger, but one more
  certificate to keep track of.

Also apply the nginx changes from the README's *Reverse proxy* section now:
the `include /etc/nginx/snippets/cloudflare-real-ip.conf;` (generated from
Cloudflare's published ranges, `real_ip_header CF-Connecting-IP;` at the end) and
`proxy_set_header X-Forwarded-For $remote_addr;` in place of
`$proxy_add_x_forwarded_for`. Without the first, every request looks like it came
from Cloudflare and the login throttle becomes global; without the second, a
client could forge the throttle key. Then:

```sh
sudo nginx -t && sudo systemctl reload nginx
```

Check a login attempt from a phone on cellular and one from the LAN land in
different throttle buckets, and that `journalctl -u daily-epub` shows real client
addresses rather than Cloudflare's.

## 5. Verify

Anonymous public page — with rule 2, expect `HIT` on the second request (the
first fills the edge); without it, `DYNAMIC`. Either way the origin's own
`Cache-Control` must come through unchanged:

```sh
curl -sI https://daily.hallada.net/ | grep -iE 'cf-cache-status|cache-control|age'
curl -sI https://daily.hallada.net/ | grep -i cf-cache-status     # HIT
```

Signed in — the cookie rule must take it out of the cache entirely:

```sh
curl -sI -H 'Cookie: daily_session=whatever' https://daily.hallada.net/ \
  | grep -iE 'cf-cache-status|cache-control'
# cf-cache-status: BYPASS (DYNAMIC is also acceptable)
# cache-control: private, no-store
```

Static assets — versioned URLs should settle on `HIT`:

```sh
curl -s https://daily.hallada.net/ | grep -o '/static/app.css?v=[0-9a-f]*'
curl -sI 'https://daily.hallada.net/static/app.css?v=<hash>' \
  | grep -iE 'cf-cache-status|cache-control'
# cf-cache-status: HIT
# cache-control: public, max-age=31536000, immutable
```

Downloads and OPDS — never cached, at any status:

```sh
curl -sI https://daily.hallada.net/opds | grep -iE 'cf-cache-status|cache-control'
curl -sI https://daily.hallada.net/files/epub/does-not-exist.epub \
  | grep -iE 'cf-cache-status|cache-control'
# cache-control: private, no-store   (cf-cache-status: BYPASS or DYNAMIC)
```

The dashboard and the login page:

```sh
curl -sI https://daily.hallada.net/login | grep -iE 'cf-cache-status|cache-control'
# cache-control: no-store
```

Finally, the morning after: within five minutes of the `generate` timer firing,
`curl -s https://daily.hallada.net/ | grep -o 'issues/[0-9-]*' | head -1` shows
the new date from a network that is not signed in.

## Rollback

- **Cache misbehaving:** turn on **Development Mode** (Caching → Configuration)
  for a three-hour edge bypass while you look, or **Purge Everything**. Neither
  needs a deploy.
- **Something worse:** set the `daily` DNS record back to **DNS only** (grey
  cloud). Traffic goes straight to the origin again within the record's TTL, and
  nothing about the origin's behaviour depends on Cloudflare being there — the
  `Cache-Control` headers are correct without it.
- **Full retreat:** point the registrar's nameservers back at Route 53. The
  hosted zone still exists unless it was deleted; do not delete it until the
  Cloudflare setup has run for a few weeks.
