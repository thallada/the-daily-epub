
---

# YOUR STEP: 2 — Claude editor and editorial (plan §21 step 2)

Step 1 has landed (three-way votes, `rating_events`, `data/profile.md`, the rebuilt system
prompt). Read `git log -3` and the current `src/curate/llm.rs`, `select.rs`, `editorial.rs`,
`pipeline.rs`, `config.rs`, `report.rs`, `db.rs`, `src/epub/chapters.rs` + templates before coding.

Scope (plan §4.1, §4.2, §5, §7.6, §13, §14, §15.1 chapter/in-this-issue `why`, §18 types, §19 config):

1. **`AnthropicBackend`** in `src/curate/llm.rs` implementing `ChatBackend` exactly per §4.2:
   `POST {base_url}/v1/messages`; headers `x-api-key`, `anthropic-version: 2023-06-01`,
   `content-type: application/json`, `anthropic-beta: server-side-fallback-2026-07-01`;
   body `{model, max_tokens: 16000, system: [{type:"text", text, cache_control:{type:"ephemeral"}}],
   messages:[{role:"user", content}], output_config:{effort}, fallbacks:"default"}`.
   **Never** send `temperature`, `top_p`, `top_k`, `thinking`, or an assistant prefill. Ask for JSON
   in the prompt text and parse tolerantly (reuse `strip_code_fence` + the existing tolerant parsing).
   Response: concatenate `content[]` blocks with `type == "text"`. Usage: `input_tokens`,
   `cache_creation_input_tokens`, `cache_read_input_tokens`, `output_tokens`; cost = input×price_input
   + cache_creation×price_cache_write + cache_read×price_cache_read + output×price_output (per M).
   `stop_reason == "refusal"` (HTTP 200) surfaces as a distinct `LlmError` variant that triggers the
   fallback; 429/5xx/network are `Transient` (retried by the existing `RetryPolicy`); 400 is never
   retried. Request timeout 300 s. API key only from `DAILY_EPUB_ANTHROPIC__API_KEY`.
   `ChatRequest` gains `effort: Option<String>`; the Anthropic backend ignores `temperature`/`json`
   and maps `system` to the cached system block. `TokenUsage` needs a cache-write counter (keep the
   DeepSeek path's cost identical to today). Generalize `UsageMeter` to take a per-provider price
   table instead of `&DeepseekConfig`.
2. **`Llms { bulk: Option<LlmClient>, editor: Option<LlmClient> }`** with `editor_or_bulk()` (the
   editor when configured and its meter is not tripped, else bulk). Both clients share the exact same
   system prompt string (§8.4). `LlmError::MissingApiKey` and friends must name the provider.
3. **Config** (§4.2, §19): `[anthropic]` with `enabled`, `base_url`, `model = "claude-opus-5"`,
   `api_key` (env only), `effort = "high"`, `price_input_per_mtok = 5.0`,
   `price_cache_write_per_mtok = 6.25`, `price_cache_read_per_mtok = 0.5`, `price_output_per_mtok = 25.0`,
   `max_daily_usd = 3.0`, `max_concurrent_requests = 4`; `[deepseek].max_concurrent_requests = 4`;
   `[curation].max_article_count = 28`; `[editorial]` with `summary_model = "editor"` (`editor|bulk`)
   and `summary_input_tokens = 3000`. Validate `max_article_count >= target_article_count`, effort ∈
   {low, medium, high, xhigh, max}, batch/concurrency ≥ 1. Startup logs the resolved models and whether
   each provider is enabled (§19). Update `config.example.toml` and the README (prerequisites, cost
   line, the note that server-side fallback is enabled, dashboard spend limits as the real backstop).
4. **Budget and concurrency** (§5): one `UsageMeter` per provider, each with its own `max_daily_usd`.
   The budget day is the **UTC date of the run's `started_at`**, preloaded by summing
   `runs.provider_costs_json` for earlier runs that UTC day; this replaces `db::spend_for_date`
   (remove it). Write `runs.provider_costs_json` (`{"deepseek": {...usage, cost_usd}, "anthropic": {...}}`)
   and `runs.config_json` (the resolved `[curation]`, `[editorial]`, model names and prompt versions,
   as JSON) in `finish_run`. Keep `runs.cost_usd` as the total across providers. Existing Stage A
   scoring batches run through `futures::stream::iter(...).buffer_unordered(max_concurrent_requests)`
   with the budget check before each request is spawned; a tripped meter skips remaining calls,
   lets in-flight finish, records the number of unscored candidates in the report, and continues.
5. **The editor** (§13): `EDITOR_INSTRUCTIONS` replaces `SELECT_INSTRUCTIONS` verbatim from the plan
   with `{soft_target}`/`{hard_max}` substituted. Runs on `editor_or_bulk()`; on refusal/error fall
   back to the same prompt on the bulk client; if that fails too, `select_without_llm`. Per-item
   rendering follows §13 with what exists today (Stage A score/category/rationale instead of
   quality/fit/facets — those arrive in step 5; render `flags: always-include | excerpt only`;
   `opening:` first 60 words). Do not put the numeric blend in the prompt. `assemble()` keeps section
   validation, unique lead, auto-include reinsertion, duplicate-id defence, malformed-response
   fallback and the `hard_max` trim (by today's ranking key, utility arrives in step 5).
   **Delete the "too few: top up" branch.** `--max-articles N` is a ceiling:
   `hard_max = min(config.curation.max_article_count, N)`, `soft_target = min(target_article_count, hard_max)`.
   `select_without_llm` respects `soft_target` as its size. Each pick's `why` (≤14 words) lands on
   `Pick.why: Option<String>` and in `issue_articles.why` (`replace_issue_articles`).
6. **Editorial** (§14): summaries run on the editor client per `editorial.summary_model` with the
   input budget from config (3,000 tokens), concurrency 4 via `buffer_unordered`, fallback per article:
   bulk client, then the excerpt. `BRIEF_INSTRUCTIONS` replaces `FRONT_PAGE_INSTRUCTIONS` verbatim
   from §14.2 (JSON `{"brief": "..."}`); input is the lineup with sections, each pick's title, feed,
   `why`, summary and the Stage A score. `Editorial.section_intros` is removed (or always empty) and
   `section.xhtml` renders only the section name; `front_page.xhtml` renders the brief under the
   masthead; fallback stays `fallback_front_page_html`. The weekly profile rebuild runs on
   `editor_or_bulk()` (pipeline and the `profile rebuild` CLI).
7. **Paper** (§15.1): `chapter.xhtml` gets a small italic `Why it's here: <why>` line under the meta
   line; the In-this-issue page shows the `why` line under each summary. `Colophon` gains
   `provider_costs` and `models`; the colophon template prints per-provider cost lines and the models
   (editor and summaries model, bulk model). `StageCounts`/`RunReport` carry per-provider usage;
   `print_report` in `main.rs` prints per-provider cost.
8. Remove `Vote`-era leftovers you notice only if they are in your files; do not touch embeddings,
   signals, telemetry or triage (steps 3–5).

Tests to add/adapt (§20 "Anthropic backend", "Editor", parts of "Pipeline"): request body has the
system block with `cache_control`, no `temperature`, `output_config.effort`, `fallbacks`, the beta
header; usage fields parsed into cost with cache read/write prices; `stop_reason: refusal` surfaces as
the fallback-triggering error; 429 retried, 400 not (use a mock backend or a local `axum`/`tokio`
listener — never the real network); a nine-pick response is published as nine; `hard_max` trims;
`--max-articles` is a ceiling; auto-includes reinserted; `why` lines land on picks and in
`issue_articles.why`; refusal/error on the Anthropic mock falls back to the DeepSeek mock with the
same prompt; the brief is parsed and rendered, section intros gone; `provider_costs_json` and
`config_json` are written; budget day preload sums by UTC date.
