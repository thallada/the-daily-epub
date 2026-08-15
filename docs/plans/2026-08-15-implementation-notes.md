# Implementation notes (shared brief for all implementation agents)

Authoritative spec: `docs/plans/2026-08-15-the-daily-epub.md`. Read it fully before writing code.
This file records implementation-time decisions and verified external facts. Follow both.

## Verified external facts (2026-08-15)

- **DeepSeek model id is confirmed**: `deepseek-v4-flash` (version DeepSeek-V4-Flash-0731).
  Pricing per 1M tokens: $0.0028 cache-hit input, $0.14 cache-miss input, $0.28 output.
  OpenAI-compatible API at `https://api.deepseek.com/v1`, supports `response_format: {"type":"json_object"}`.
- **epub-to-xtc-converter** (github.com/bigbag/epub-to-xtc-converter) has **no global npm bin**.
  It is invoked as: `node <repo>/cli/index.js convert book.epub -o book.xtch -f xtch -c settings.json`
  (`-f xtc` = 1-bit, `-f xtch` = 2-bit grayscale; `init` subcommand generates default settings).
  Therefore config must be fully general: `xtc.command = "node"`,
  `xtc.args = ["/path/to/epub-to-xtc-converter/cli/index.js", "convert"]` and the code appends
  `<input.epub> -o <output.xtch> -f <format>` (plus `-c <settings>`). Missing/failed converter is
  non-fatal (log + continue).
  - **Corrected 2026-08-15 (post-M8, verified by running it).** `-c` is *not* optional in
    practice: the converter validates settings before opening the EPUB and exits **2** with
    `Configuration errors: - Font path is required. Set font.path in your config file.` There is
    no built-in font default, and the shipped `cli/settings.json` points at
    `/usr/share/fonts/Adwaita/AdwaitaSans-Regular.ttf`, which most servers do not have. So
    `xtc.settings` is effectively required whenever `xtc.enabled`. Deps also need
    `npm install` **inside `cli/`** (commander, jszip, minimatch, sharp).
  - **That same error also means "I could not read your config."** `loadSettings` in
    `cli/settings.js` guards with `fs.existsSync(configPath)`, which returns `false` on
    `EACCES` exactly as it does for a missing file, then silently falls back to
    `DEFAULT_SETTINGS` (`font.path: null`). Since `/etc/daily-epub` is `0750
    daily-epub:daily-epub`, running the converter by hand as any other account reproduces
    the "Font path is required" error against a valid file. Verify as `sudo -u daily-epub`.
  - **Output size.** XTCH is a pre-rendered page bitmap: 480×800 at 2bpp = ~96 KB/page. A
    20-article issue rendered at `font.size = 34` came to 1,088 pages ≈ **104 MB**, in ~13 s.
    `retention_days = 21` therefore implies ~2 GB in `publish.xtc_dir`.
- **CrossPoint's OPDS browser can only acquire EPUBs.** Verified against the
  `yokki-vans/InkPointX` sources (an open fork of CrossPoint/CrossInk). Two independent
  blockers, either one fatal for serving XTC over OPDS:
  1. `lib/OpdsParser/OpdsParser.cpp` sets an entry's `href` only for an acquisition link
     whose `type` is **exactly** `application/epub+zip` (`strcmp`), or for a navigation
     link (`application/atom+xml`). `endElement` then drops any entry with an empty
     `href`. An `application/octet-stream` acquisition link therefore yields an empty
     list and the UI reports `STR_NO_ENTRIES` — **"No entries found"**.
  2. `OpdsBookBrowserActivity::downloadBook` builds the destination filename as
     `sanitizeFilename(author + " - " + title) + ".epub"` — hardcoded, ignoring the URL
     and `Content-Disposition` — and `ReaderActivity` dispatches on extension only
     (`FsHelpers::hasXtcExtension` → `.xtc`/`.xtch`, no magic-byte sniffing). So even a
     mistyped XTC link downloads into a file the device will not open.
  The three OPDS failure strings are distinct and worth reading precisely:
  `STR_FETCH_FEED_FAILED` ("Failed to fetch feed"), `STR_PARSE_FEED_FAILED`
  ("Failed to parse feed") and `STR_NO_ENTRIES` ("No entries found") — only the last is
  reachable after a *successful* fetch **and** parse.
  Consequence: the built-in feed serves EPUBs (both editions), XTC is published but not
  advertised, and `publish.xtc_dir` is swept by count. See the amendment in spec §3.11.
- **X4 firmware rendering limits** (from the `epub-to-xtc-converter` optimizer's header, which
  cites papyrix-reader): 464×788 usable viewport, max image decode 2048×3072, **baseline JPEG
  only**, no GIF/SVG/WebP, **max 1500 CSS rules and simple selectors only** (`tag`, `.class`,
  `tag.class` — no descendant combinators), **max word length 200 chars**, images under 20 px
  treated as decorative. These bind the `(X4).epub` read natively off BookOrbit; they do *not*
  bind the `.xtch`, which CREngine pre-renders to page bitmaps (`convert` never calls
  `optimizeEpub` — the two subcommands are independent). `epub/x4.rs` and `style-x4.css` satisfy
  all of them; `x4::simplify_xhtml` soft-hyphenates past `MAX_WORD_CHARS` and
  `the_x4_stylesheet_uses_no_descendant_selectors` guards the selector rule.
- **Wikipedia Current Events portal pages are created empty a day ahead.**
  `Portal:Current_events/2026_August_15` was created 2026-08-14T03:30Z as a 192-byte stub and
  did not get its first news item until 2026-08-15T13:28Z. The 05:30 America/New_York timer
  fires at ~09:30Z, so the issue day's own page is **always** an unpopulated stub — its only
  `<li>` elements are the `current-events-navbar` edit/history/watch links, which the extractor
  drops, so `extract_events` correctly returns `None`. `world::fetch_with_fallback` therefore
  walks back up to `MAX_LOOKBACK_DAYS` and the section is datelined with the day it actually
  covers, not the masthead date.

## Cross-cutting implementation decisions

1. **sqlx usage**: use *runtime* queries (`sqlx::query(...).bind(...)`) and manual row mapping
   (or `sqlx::FromRow` derive with `query_as`). Do **not** use the compile-time checked
   `query!`/`query_as!` macros (they require DATABASE_URL/offline data at build time).
   Migrations via `sqlx::migrate!("./migrations")` embedded at compile time.
2. **Time**: `jiff` everywhere; day boundaries and `--date` interpretation in the configured
   timezone (`America/New_York` default). Store timestamps in SQLite as RFC3339 UTC strings.
3. **Errors**: modules return `thiserror` error types or `anyhow::Result`; `main.rs` uses `anyhow`.
   Pipeline stages are best-effort where the spec says so (social, XTC, world briefing, images).
4. **HTTP**: one shared `reqwest::Client` (rustls, gzip, no cookies, 10s timeouts, UA
   `the-daily-epub/1.0 (personal rss digest; contact tyler@hallada.net)`), passed by clone.
5. **LLM**: `async-openai` with custom base URL. All LLM calls go through `curate/llm.rs`
   `LlmClient` which tracks token usage into a shared `UsageMeter` (input/cached/output tokens,
   cost usd) and enforces `max_daily_usd`.
6. **Testing**: unit tests inline per module; integration tests in `tests/` over fixture JSON in
   `tests/fixtures/`. Never hit the network in tests. LLM stage mockable via `--skip-llm`
   (prefilter order used for selection, feed excerpts as summaries).
7. **Style**: rustfmt defaults, `cargo clippy` clean-ish, no `unwrap()` outside tests, tracing
   spans per pipeline stage.
8. **File ownership**: waves of agents work in parallel on disjoint files. Do not edit files
   outside your assigned set (module wiring in `main.rs`/`mod.rs` is done by the scaffold and
   the integration wave). If you need a helper from another module that doesn't exist yet, add
   a `// TODO(integration): ...` note and code against the stub signature.
9. **Dedupe module**: normalize/dedupe (§3.2) lives in `src/dedupe.rs` (canonical URL fn +
   clustering), called from the generate pipeline between ingest and extraction.
10. **World briefing** (§3.8) lives in `src/world.rs`.
11. **Askama templates** in `src/epub/templates/` (`*.xhtml` askama templates + `style.css`,
    `style-x4.css`). Askama 0.12+ configured via `askama.toml` if needed.
12. **Determinism**: chapter ids `art-{entry_id}`, stable filenames, issue regeneration for the
    same date replaces prior rows/files (idempotent upsert everywhere).
