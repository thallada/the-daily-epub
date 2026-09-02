
---

# YOUR STEP: 5 — Deep assessment, utility, diversity (plan §21 step 5)

Steps 1–4 have landed; triage and union admission now gate the deep set and the old prefilter is
reduced to hygiene helpers. Read `git log -8`, then all of `src/curate/`, `src/pipeline.rs`,
`src/types.rs`, `src/config.rs`, `src/report.rs`, `src/main.rs`, `src/db.rs`.

Scope (plan §12 in full, §13 rendering with facets/neighbours, §7.3 deep rows, §18 module retirements):

1. **`src/curate/assess.rs`** replaces `score.rs` (§12.1): `DEEP_INSTRUCTIONS` and
   `DEEP_PROMPT_VERSION = 1` verbatim from the plan (the section palette is substituted from
   `curation.sections`). Per article: title, feed, author, length, excerpt-only flag, the triage
   `why`, the interest and neighbour hint lines from §10, and a **representative sample** of
   ~2,000 tokens: body under ~1,500 words sent whole; otherwise first 600 words, 500 around the
   midpoint, last 400, with visible `[BEGINNING]`/`[MIDDLE]`/`[END]` markers, split on word
   boundaries. Batches of `deepseek.deep_batch_size` (8, replaces `score_batch_size`) through
   `buffer_unordered`, bulk client, budget check per batch. Auto-includes are assessed too.
   Tolerant parsing: `quality`/`fit` clamped 0–10, `category` validated against the palette (invalid →
   `None`, the editor decides), `paywalled_guess`, and `facets` where every unknown enum token degrades
   to `None` and `specific_topics` is capped at 3. **Every enum token in the prompt must round-trip
   through the parser (test).** Persist to `article_assessments (stage='deep', score=quality, fit,
   kind=facets.format, facets_json, rationale, category, paywalled_guess, ...)`; reuse per §7.3 with
   `--rescore` bypass. Facets are shown to the editor, the profile rebuild and `explain`; **not** a
   numeric signal.
2. **`src/curate/rank.rs`** (§12.2–§12.5): normalization (LLM scores ÷ 10; everything else mid-rank
   percentile over the deep set — reuse/move the step-3 normalizer), the **utility** blend over
   present signals with `[curation.ranking.weights.utility]` renormalized and learned signals
   multiplied by their gate ramp first, stored 0–100; the preliminary blend stays for the eligible set.
   **Diversified shortlist** (§12.5): leader clustering by embedding cosine with `cluster_threshold`
   (0.85) and `per_cluster_cap` (2): sort by utility desc then article id asc; assign each article to
   the first existing cluster whose *leader* has cosine ≥ threshold, else make it a new leader;
   articles without embeddings are singletons; admit in order while the cluster's admitted count is
   below the cap until `shortlist_keep` (60); the top `utility_protected` (10) by utility and all
   auto-includes are admitted regardless and still count toward their cluster; exploration picks that
   reached the deep set get up to 3 reserved slots; if short, relax to cap 3, then uncapped. Persist
   `cluster_id`, `cluster_rank`, `rank_utility`, `utility`, `excluded_reason ∈ cluster_suppressed |
   shortlist_cap`, stage `shortlisted`.
3. **`src/curate/editor.rs`** replaces `select.rs`: the §13 rendering in full —
   `quality 8.5 · fit 7.0 · triage 8.0 — <deep rationale>`, `facets: format · depth · evidence ·
   technicality · topic_group`, `matches:`, `closest rated:`, `flags: exploration | always-include |
   excerpt only`, `opening:` first 60 words. The editor sees the 60-item shortlist. `assemble()`'s
   `hard_max` trim and `select_without_llm` now order by **utility**, falling back to the preliminary
   blend. Delete `ScoredArticle::combined_score()` and `ScoredArticle` itself; `Candidate` is the only
   flow type. Move the tests from `score.rs`/`select.rs` into `assess.rs`/`editor.rs` and delete the
   old files (update `curate/mod.rs`, `Curator`).
4. **Editorial inputs**: the Brief and the summaries use quality/fit where the plan says so (§14.2
   input lists quality/fit). The weekly profile rebuild's rated lines now get real facets.
5. **Pipeline/report**: stage timings `assess` and `rank`; `StageCounts` gains `assessed`,
   `shortlisted`, `clusters`; the §15.4 curation line
   `curation: 412 considered → 398 eligible → 398 triaged → 120 assessed → 60 shortlisted → 17 selected`.
   `explain` prints the deep assessment (quality, fit, category, rationale, facets), utility and rank,
   cluster id and what suppressed it; `--near-misses` orders by utility. `runs.config_json` gains
   `DEEP_PROMPT_VERSION`. `--skip-llm`/DeepSeek down ⇒ no deep assessment, utility over present
   signals (triage/interest dominate, §12.3), editor still runs on what it has (§17).
6. `config.example.toml`/README: `deep_batch_size` replaces `score_batch_size`; a stale
   `score_batch_size` key fails loudly naming `deep_batch_size`.

Tests (§20 "Triage and deep parsing" deep half, "Normalization", "Clustering", "Editor" utility
parts): realistic deep fixture; malformed items do not sink a batch; unknown facet tokens → `None`;
every enum token round-trips; representative sample has the three markers and respects word
boundaries, short bodies are sent whole; cached deep rows reused / bypassed with `--rescore`; constant
signal → 0.5; ties equal; absent values do not shift others; effective weights sum to 1; a candidate
missing a signal is scored on the rest; near-duplicates share a cluster and the third is suppressed;
protected top-N survive and count; the bridge case (A~C, B~C, A≁B, utility A>B>C) yields two clusters;
articles without embeddings are never suppressed; `hard_max` trims by utility; `select_without_llm`
orders by utility; the mocked full pipeline publishes with DeepSeek down and writes `shortlisted`
rows with cluster ids.
