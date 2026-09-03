# Implementation notes (shared brief for all implementation agents)

Authoritative spec: `docs/plans/2026-08-15-the-daily-epub.md`. Read it fully before writing code.
For curation (§3.5, §3.6 and §3.9 of that spec) the authority is now
`docs/plans/2026-09-02-personalized-curation-v2.md`; see "Curation v2" below.
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

## Verified external facts (2026-09-02, curation v2)

- **Anthropic Messages API** (verified 2026-09-02 against the bundled Claude API reference):
  `POST https://api.anthropic.com/v1/messages` with headers `x-api-key`,
  `anthropic-version: 2023-06-01`, `content-type: application/json` and
  `anthropic-beta: server-side-fallback-2026-07-01`. Model id `claude-opus-5`; pricing
  **$5.00 / M input, $25.00 / M output**, cache reads 0.1× input ($0.50/M), cache writes
  1.25× ($6.25/M); the minimum cacheable prefix is 512 tokens. **No sampling parameters**
  (`temperature`, `top_p`, `top_k` are a 400) and no `thinking` block — adaptive thinking is on
  by default and depth is set with `output_config: {"effort": "high"}` (`low | medium | high |
  xhigh | max`). The system prompt goes in `system: [{type: "text", text, cache_control:
  {type: "ephemeral"}}]`; no assistant prefill, JSON is asked for in the prompt and parsed
  tolerantly. `"fallbacks": "default"` (with the beta header) routes a request the safety
  classifiers would refuse to a fallback model server-side; a response can still end with
  `stop_reason: "refusal"` on HTTP 200, which the code treats as an error that degrades the
  call to DeepSeek. Usage fields: `input_tokens` (uncached remainder),
  `cache_creation_input_tokens`, `cache_read_input_tokens`, `output_tokens`. Timeout 300 s;
  retry 429/5xx/network, never 400. Key only from `DAILY_EPUB_PROVIDERS__ANTHROPIC__API_KEY`
  (the provider registry below; the pre-registry `DAILY_EPUB_ANTHROPIC__API_KEY` is a startup
  error).
- **Gemini 3.8 Flash over the OpenAI-compatible endpoint** (beta, verified 2026-09-02):
  `POST https://generativelanguage.googleapis.com/v1beta/openai/chat/completions` with
  `Authorization: Bearer <key>`, the standard `messages` / `temperature` /
  `response_format: {"type": "json_object"}` body. Model id `gemini-3.8-flash`. Reasoning depth
  is the OpenAI `reasoning_effort` field, which Google maps onto Gemini 3.x's `thinking_level`
  (`minimal | low | medium | high`; `none` is not accepted by 3.x models). Usage: implicit
  cache hits are reported in `prompt_tokens_details.cached_tokens` (the same field DeepSeek
  now fills), and `completion_tokens` already includes the thinking tokens that
  `completion_tokens_details.reasoning_tokens` breaks out — so output is priced from
  `completion_tokens` alone, never the sum. Prices per 1M tokens (promotional through
  2026-12-31): **$0.75 input, $0.075 cache read, $3.75 output** (thinking included); from
  2027-01-01 **$1.50 / $0.15 / $7.50**. No cache-write charge. Key only from
  `DAILY_EPUB_PROVIDERS__GEMINI__API_KEY`. Shipped as `[providers.gemini]`, unreferenced until
  a role names it.
- **Voyage AI embeddings** (verified 2026-09-02): `POST https://api.voyageai.com/v1/embeddings`
  with `Authorization: Bearer <key>`; body `{input: [...], model: "voyage-4-lite", input_type:
  "document" | "query", truncation: true, output_dimension: 512, output_dtype: "float"}`. Up
  to 1,000 inputs and 1M tokens per request, 32k tokens per input. Vectors are
  unit-normalized, so dot product = cosine. **$0.02 / M tokens** after a 200M-token free
  allocation. Key only from `DAILY_EPUB_VOYAGE__API_KEY`.

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
5. **LLM**: a hand-rolled `reqwest` client, not `async-openai` (the published crate exposes
   neither `Client` nor `CreateChatCompletionRequest` at the pinned version). Every LLM call
   goes through `curate/llm.rs`: `LlmClient { provider, system_prompt, model, effort,
   max_concurrent_requests, meter, backend, retry }` over the `ChatBackend` trait, with two
   wire protocols — `OpenAiCompatibleBackend` (`{base_url}/chat/completions`, bearer key,
   `response_format: json_object`, `reasoning_effort` when the provider has an `effort`) and
   `AnthropicBackend` (Messages API, facts above). **Providers are config, not code**: the
   `[providers.<name>]` registry (`kind = openai | anthropic`, `base_url`, `model`, `effort`,
   `max_daily_usd`, `max_concurrent_requests`, `price_*`) is a `BTreeMap<String,
   ProviderConfig>`, and `[llm] bulk = "<name>"` / `editor = "<name>"` assign the two roles by
   name (`editor = ""` means everything runs on bulk; both roles on one provider share one
   client and one ceiling). `LlmClient::for_provider(name, &cfg, ..)` dispatches on `kind`;
   `Llms::from_config(&config, prompt, &meters)` builds the roles; `editor_or_bulk()` degrades
   to bulk when the editor client is missing or its meter is tripped. One `UsageMeter` per
   *referenced* provider (`llm::provider_meters`), keyed by provider name — the same key used
   for `runs.provider_costs_json`, the UTC-day spend preload and the log lines — plus Voyage's
   own. `LlmClient.provider` is the config name, never the kind. The system prompt is sent
   first and byte-identical within a run so every provider's prefix cache hits. Keys come only
   from `DAILY_EPUB_PROVIDERS__<NAME>__API_KEY` (figment lower-cases the path, so provider
   names are `[a-z0-9_]+`); `daily-epub config check` prints the resolved roles without
   opening the database.
6. **Testing**: unit tests inline per module; integration tests in `tests/` over fixture JSON in
   `tests/fixtures/`. Never hit the network in tests: `MockBackend` (`ChatBackend`) and the
   embedding mock (`EmbeddingBackend`) stand in for all three providers. `--skip-llm` makes
   zero LLM calls (admission by cheap signals, `select_without_llm` by utility, excerpt
   summaries); `--skip-embeddings` makes zero Voyage calls.
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

## Curation v2 (2026-09-02)

The personalized ranker is specified in `docs/plans/2026-09-02-personalized-curation-v2.md`
(§0 settled decisions, §3 target pipeline, §19 configuration, §21 the seven landed steps);
`docs/plans/2026-09-02-curation-v2-progress.md` records per-step deviations. Facts an
implementer needs that are easy to get wrong:

- **Tables** (`migrations/0002_curation_v2.sql`, `0003_drop_scores.sql`; never edit
  `0001_init.sql`): `rating_events` (append-only; the current verdict is the latest
  `explicit` event), `article_embeddings` and `interest_embeddings` (f32 little-endian BLOBs,
  `input_hash` = sha256 of the embedded text), `article_assessments` (`stage IN ('triage',
  'deep')`, reused while `model` and `prompt_version` match and `assessed_at` is within
  `assessment_reuse_days`; `--rescore` ignores the cache), `candidate_runs` (one row per
  considered article per run, upserted with every column set on each stage transition),
  `runs.config_json` / `runs.provider_costs_json`, `issue_articles.why`. `ratings`,
  `feed_priors` and `scores` are dropped; `kv` keeps `ingest_watermark`, `taste_profile`,
  `taste_profile_learned`, `profile_version`.
- **Feedback**: `Vote` is `loved | good | down` (`NotForMe`); the HMAC message stays
  `{issue_date}/{article_id}/{vote}`. `Vote::parse("up")` → `Loved` and `auth::verify_token`
  still accepts tokens signed over the literal `up` segment because published issues carry
  those links. Keep both.
- **Budget day**: each provider's `UsageMeter` is preloaded with the spend of earlier runs on
  the **UTC date of the run's `started_at`**, summed from `runs.provider_costs_json`
  (`db::spend_for_date` by nominal issue date is gone). A tripped meter skips that provider's
  remaining calls; the paper always publishes.
- **Lock**: `src/lock.rs` takes `libc::flock(LOCK_EX | LOCK_NB)` on `<database_path>.lock`
  for `generate`, `profile rebuild`, `features backfill` and `backfill-social`; a second
  writer exits with "<command> is already running". `serve`, `explain`, `stats`, `ratings`,
  `features prune` and `db migrate` never take it.
- **Retention**: `telemetry::prune` removes `article_embeddings` of unrated, unpublished
  articles older than `embedding_retention_days` (120) and `candidate_runs` rows plus
  `article_assessments` older than `telemetry_retention_days` (180). `features prune` runs it
  on demand; `generate` runs it once after publishing, best effort.
- **Keys**: `DAILY_EPUB_PROVIDERS__<NAME>__API_KEY` and `DAILY_EPUB_VOYAGE__API_KEY` map onto
  `ProviderConfig.api_key` / `VoyageConfig.api_key` through figment; the fields exist only
  for that mapping and are never documented in TOML, logged, or stored
  (`Config::providers_redacted()` is what reaches `runs.config_json`).
- **Provider registry** (2026-09-02, after step 7): the `[deepseek]` and `[anthropic]` tables
  and the top-level `max_daily_usd` are gone. `[llm]` holds the role names and the role-level
  knobs (`triage_batch_size`, `deep_batch_size`, `score_temperature`,
  `editorial_temperature`); `[providers.deepseek]`, `[providers.anthropic]` and
  `[providers.gemini]` ship in `config.example.toml` and are `Config::default()` key for key.
  Stale shapes fail at load, naming the new key: a `[deepseek]`/`[anthropic]` header, a
  top-level `max_daily_usd`, any of the four role keys outside `[llm]`, or a
  `DAILY_EPUB_DEEPSEEK__*` / `DAILY_EPUB_ANTHROPIC__*` environment variable. Batching
  concurrency (`triage`, `assess`) is the bulk provider's `max_concurrent_requests`; the
  summaries fan out at the summary provider's. `Models { bulk, editor, summaries }` in the
  colophon and Behind the paper stay model ids taken from the built clients.
- **Provider rejections** (2026-09-02): `curate::batch` bisects a triage or deep batch that
  fails with a non-transient error (`Api`, `Refusal`, `EmptyResponse`, or a response that
  parses to zero items) down to single articles; a rejected single is retried once on the
  editor client when it is another provider. An article both refuse gets an
  `article_assessments` row with `kind = 'provider_rejected'`, `score`/`fit` NULL,
  `rationale = "<provider>: <message ≤ 200 chars>"`, the bulk `model` and the stage's
  `prompt_version`; the cache loaders skip it (leaving the assessment absent) while it is within
  `assessment_reuse_days`, `--rescore` ignores it, and `admit::hygiene` never treats it as a low
  score. Because a recovered article's row carries the editor's model, reuse accepts rows whose
  `model` is either configured model (`triage::reusable_models`).

## Web dashboard (2026-09-03)

Verified on the production host and against the implemented dependency graph on 2026-09-03:

- **Host authorization and units:** polkit 124 supports JavaScript rules. The installed rule must
  grant user `daily-epub` only `org.freedesktop.systemd1.manage-units`, verb `start`, for units
  matching `^daily-epub-job@[a-z0-9-]+\.service$`. `daily-epub.service` now needs
  `SupplementaryGroups=systemd-journal` to read job logs and `/etc/daily-epub` in
  `ReadWritePaths` to atomically replace `config.toml`. The job template deliberately omits
  `MemoryDenyWriteExecute` because generate jobs can launch Node's JIT; the server retains it.
- **Settings writes:** direct `toml_edit 0.25` performs typed, comment/order-preserving updates.
  A candidate file is loaded through `Config::load` before its permissions are copied and it is
  renamed over the original. The process therefore needs write access to the containing
  directory, not only the file. Every changed dotted key is written to `config_changes`.
- **Authentication stack:** `axum-login` is pinned to git revision
  `151c72d7a1b4646830f86b4332e6bd6e34d719a7`, whose graph contains `tower-sessions 0.15`.
  The local `SqliteSessionStore` uses this crate's existing sqlx 0.9 pool because the published
  tower SQLx store is incompatible. `password-auth 1.0` supplies Argon2id PHC hashes;
  `tower_governor 0.8` throttles login POSTs. Its smart IP extractor trusts forwarded headers,
  so production must bind to loopback and accept traffic only from the configured reverse proxy.
- **Test environment:** router tests use `tower::ServiceExt::oneshot`, temp SQLite databases and
  `MockRunner`; they do not bind or invoke systemd. This sandbox forbids loopback listeners, so
  the four `curate::llm::tests::anthropic_*` tests, the three OpenAI tests using the same fake
  listener, `extract::tests::relative_urls_resolve_against_the_url_we_landed_on`, the five
  listener-based `server::tests::*`, and `tests/m7_server.rs` are filtered only for sandbox runs.
  The complete suite is expected to run outside the sandbox.

Implementation-time decisions recorded while landing dashboard steps 1–7:

- `/files/*` remains public when Basic auth is not configured, preserving existing OPDS
  acquisition behavior. With Basic auth configured, either valid Basic credentials or any valid
  web session authorizes a download. This intentionally resolves the plan's conflicting request
  to redirect unauthenticated downloads in favor of its compatibility acceptance criterion.
- Final reports are attached to an issue after `finish_run`, when publish timing and status are
  complete. Issue snapshots omit article bodies and rehydrate them from `articles`; old rows use
  the reduced fallback renderer. The CLI password prompt uses `rpassword` after it became
  available to the orchestrator.
- Flash handlers extract the exact tower session installed by the auth layer through
  `Extension<Session>`. Dashboard `down` forms persist the established `not_for_me` label.
  Ratings-page feed credit includes rating decay because that is what `signals::feed_rates`
  actually uses; the stored prompt verdict count is inferred from the prompt text because
  `TasteProfile.verdicts` is not persisted.
- Dynamic list SQL is assembled only from fixed fragments and allow-listed sort/filter names,
  wrapped in sqlx 0.9's `AssertSqlSafe`; all user values remain bound parameters. Funnel bars
  count rows that reached each stage because a row stores the stage where it stopped. SVG/meter
  attributes replace inline styles under the CSP. Article pages omit the nominal extract method
  because database reconstruction currently hard-codes it and would display misleading data.
- Shipped providers cannot be removed: deleting one would cause `Config::default()` to restore
  it. They remain editable and may be unreferenced; custom providers are removable. An absent
  setting already equal to its submitted default stays absent. Settings that are captured while
  building the server, session, or throttle layers carry restart notices.
- A job start reloads a hand-edited config before inserting and starting the unit, retaining the
  last-good config if reload fails. The offline lifecycle test uses `features-prune`; generate
  run-id linkage is covered separately. CLI duration statistics retain finished dry runs for
  byte-identical output, while dashboard run series and overview sparklines exclude them.
