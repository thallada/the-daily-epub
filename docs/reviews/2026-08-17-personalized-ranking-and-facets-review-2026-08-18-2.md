# Review — Personalized Ranking, Embeddings, Facets, and Feedback

**Plan:** `docs/plans/2026-08-17-personalized-ranking-and-facets.md`
**Reviewer:** independent blind review (no other review consulted)
**Date:** 2026-08-18
**Method:** plan read end to end, then checked against `src/pipeline.rs`, `src/curate/{mod,prefilter,score,select,llm}.rs`, `src/curate/profile/mod.rs`, `src/types.rs`, `src/db.rs`, `src/config.rs`, `src/main.rs`, `migrations/0001_init.sql`, `data/scour-interests.opml`. Voyage AI claims in §3 were independently verified against the live docs.

---

## Verdict

**Not ready to execute as written — but close, and the architecture is right.** The diagnosis in §1 is accurate and well evidenced against the code: `prefilter.rs` really does make the first irreversible cut on word count, social proof, and a max-over-feeds prior (`PrefilterContext::prior_for`, `prefilter.rs:118-129`), and `select.rs:556-579` really does pad the lineup back up to `target - 5` in direct contradiction of the profile's stated editorial philosophy. The union-of-retrievers direction, the quality/reader-fit split, `candidate_rankings` as a durable feature snapshot, and "no forced filler" are all the correct calls, and I would not relitigate them. The Voyage facts in §3 are accurate (I verified `voyage-4-lite` exists at 32K context, dims 256/512/1024/2048, 1,000 inputs and 1M tokens per request, `$0.02`/Mtok with 200M free) — a genuinely unusual level of rigor for a plan.

What blocks execution is a small number of concrete numerical-design defects, not the architecture. Three of them cause the new system to *look* like it is working while producing garbage: percentile normalization with distinct tie-ranks injects article-ID order as ranking signal whenever a signal is sparse or absent (which is the day-one state); the sparse-evidence confidence damping in §11.2 is mathematically cancelled by the very normalization step that consumes it; and the facet vocabulary is roughly 84 values estimated from roughly 63 ratings, so `facet_preference` will be a near-constant for months while holding 22% of the pre-Stage-A blend. Separately, §16's split of `LlmScore.score` silently kills the churn-suppression rule, and the plan doubles the number of sequential DeepSeek round-trips in a job that has a publish deadline without ever mentioning concurrency. Fix Critical and High below — most are a paragraph of spec each — and this is ready.

---

## Critical

### C1. Percentile normalization with ID tie-breaking turns sparse signals into article-ID bias

> §14.1: "convert the daily candidate values to deterministic percentile ranks in `[0,1]` … Tie breaking must be stable by article ID."

Stable-by-ID tie breaking assigns **distinct** percentile ranks to **equal** raw values. That is correct for determinism of output order and wrong for normalization. Consider the two signals most likely to be degenerate:

- **`embedding_preference` on any day with no ratings in the lookback window** (i.e. day one of rollout, and any 90-day gap): §11.2 gives `positive_similarity_or_0 - 0.75 * negative_similarity_or_0` = `0.0` for every candidate. Percentile-ranking 400 tied zeros with an ID tiebreak produces a perfect ascending-article-ID ramp from 0.0 to 1.0. §14.2 then weights that ramp at **0.28** of the pre-Stage-A score and §17 at **0.15** of utility.
- **`social_score`**: `composite_social_score` returns exactly `0.0` for every article with no `social` rows — the majority on any given day. Same ramp, 0.05 / 0.04 weight.

Article IDs are `AUTOINCREMENT` (`migrations/0001_init.sql:26`) and assigned in `persist_articles` iteration order, so low IDs are systematically older articles and articles from feeds that happened to sort earlier. The failure is silent: rankings look plausible, `candidate_rankings` looks populated, and the reader sees a subtly wrong paper.

**Fix (specify explicitly in §14.1):** equal raw values must receive **equal** normalized values — use mid-rank / average-rank percentiles (`(#below + (#equal + 1)/2) / n`). Article ID may break ties only in the *final output ordering*, never inside the normalizer. Add a unit test: "a signal that is constant across all candidates normalizes to 0.5 for every candidate," alongside the existing §27.6 "percentile normalization stable with ties" case, which as worded would pass with the buggy behavior.

### C2. Splitting `LlmScore.score` silently disables the churn-suppression rule

§16.1 replaces `score` with `quality_score` + `reader_fit_score`. §13.1 says the recently-rejected rule is preserved:

> §13.1: "recently rejected churn rule (LLM < 3 within the configured lookback), except always-includes."

That rule is implemented as `db.recently_low_scored_ids(STALE_LOW_SCORE, since)` (`prefilter.rs:104`), which reads `scores.llm_score` (`db.rs:325-339`), which is written only by `db.upsert_score` from `llm.score` (`db.rs:402`). If Stage A stops producing a field named `score`, nothing writes `scores.llm_score`, `recently_low_scored_ids` returns empty forever, and the rule dies with no error and no test failure. The consequence is not cosmetic: yesterday's rejects re-enter the recall pool every day, they consume facet-extraction and Stage A tokens, and `PENALTY_TITLE_PATTERNS`-class churn recirculates indefinitely.

The plan gestures at this — §16.1 says "Prefer a new type if changing `LlmScore` would make existing persisted `scores.llm_score` ambiguous" — but never states what writes the column going forward. Worth noting that `LlmScore` is **not** persisted as JSON anywhere (I checked `report.rs`, `server.rs`, `publish.rs`; `issue_articles` stores only section/position/summary), so the only real compatibility surface is this one column.

**Fix:** state in §16.1 and §26 that `scores.llm_score` continues to be written with `quality_score`, and add `llm_reader_fit_score` as a new column in `0002`. Add the regression test: "an article with `quality_score < 3` yesterday does not appear in today's recall pool."

### C3. The sparse-evidence confidence damping in §11.2 is a no-op

> §11.2: `confidence = total_decayed_rating_weight / (total_decayed_rating_weight + 6.0)`; `embedding_preference = embedding_preference_raw * confidence`

`confidence` is a **single scalar for the whole run** — it depends only on the rating history, not on the candidate. Multiplying every candidate's raw score by the same positive constant is a strictly monotone transform. It is therefore erased by:

- §14.1's percentile normalization (rank-invariant), which is what consumes `embedding_preference` in the §14.2 and §17 blends; and
- §13.3's "top `recall_embedding_preference_top` by learned embedding preference" (a pure top-K, also rank-invariant).

So the damping has no effect anywhere it is used. With three upvotes total, the embedding preference signal still contributes its full 0.28 / 0.15 weight, ranking on noise. This is the exact failure the paragraph was written to prevent.

**Fix:** damping must act on the **blend weight**, not the score. Replace with: compute `w_embed_effective = w_embed * confidence`, redistribute the freed weight proportionally across the remaining present signals, and record both in `explanation_json`. Alternatively blend the normalized signal toward the pool mean: `norm' = 0.5 + confidence * (norm - 0.5)`. Either works; the current formulation cannot. Same audit is needed for any other place the plan multiplies a whole-run constant into a per-candidate score.

---

## High

### H1. Signals with no evidence must be dropped from the blend and the weights renormalized

The plan does exactly the right thing *inside* facet preference — §11.3: "Normalize by the sum of weights actually present" — and then does not do it for the **outer** blends in §14.2 and §17. On a cold-start day:

- `embedding_preference` has no evidence (C3) → 0.28
- `facet_preference` has no evidence (H2) → 0.22, mapped through `(x+1)/2` to a constant 0.5

That is **50% of the pre-Stage-A score** that is either constant or ID-noise, silently compressing the dynamic range of the heuristic, interest, and social signals to half their intended influence. §17's utility has the same problem at 25%.

**Fix:** make the blend a weighted mean over *present* signals with per-signal presence tests (`has_positive_centroid`, `facet_dimensions_with_support > 0`, `social_rows > 0`), renormalizing to the weights actually present, and persist the effective weight vector into `explanation_json` so `explain` and `evaluate` can see it. This is ~15 lines and it is the difference between the system degrading gracefully and degrading invisibly.

### H2. The facet vocabulary is far too large to estimate from the available ratings

§10.1 defines 11 dimensions over roughly **84 controlled values** (14 topic groups + 18 formats + 4 depths + 5 technicality + 4 audience + 10 tones + 8 stances + 9 evidence modes + 4 temporal + 4 locality + 4 commerciality). §11.3 estimates a Beta rate per value from decayed ratings. The plan's own example telemetry in §24 says:

> `personalization: 63 recent ratings, 55 embedding-backed, 51 facet-backed`

63 ratings across 84 values, further split by up/down and diluted by multi-value averaging, means the median facet value will have **zero or one** observation. With `support = (u+d)/(u+d+4)`, a single observation gives `support = 0.2` and `effect = (0.67-0.5)*2*0.2 ≈ 0.067` — indistinguishable from noise. `facet_preference` will hover near zero for months while consuming 0.22 of the pre-Stage-A blend and one DeepSeek call per 12 recall-pool articles.

The plan is aware of the cardinality/evidence tradeoff — §10.1: "Keep controlled enums small enough that ratings accumulate statistical support" — and then does not follow its own rule.

**Fix, pick one:**
- **(Recommended)** Ship V1 with **four** dimensions and ~5 values each: `format` (collapse 18 → `reported | analysis_essay | tutorial_technical | postmortem_case_study | announcement_roundup`), `depth`, `evidence_mode` (collapse 9 → `first_hand | original_reporting | data_or_experiment | synthesis | speculative`), `commerciality`. That is ~20 parameters against 63 ratings — estimable. Keep the full vocabulary as `schema_version = 2` once there are ~300 ratings. The full enums are still worth extracting into `facets_json` for explanation and for the §12 profile prompt; just don't *score* on the sparse ones.
- Or gate the whole facet-preference contribution behind a minimum-evidence threshold and let H1's renormalization carry the weight elsewhere until then.

Note this also affects §12: "Enrich the rebuild prompt with saved facet data" is valuable at *any* cardinality, because the LLM is doing pattern recognition, not parameter estimation. That part should ship regardless.

### H3. The high-recall union has no quality floor on three of its four retrievers, and dense retrieval has a strong short-document bias

§13.3 admits candidates by "top 80 by semantic-interest score", "top 80 by learned embedding preference", "top 30 by feed affinity". Only exploration gets a floor (§15: "still above a minimal heuristic-quality floor so the system does not explore obvious junk").

Cosine similarity against a short query concentrates on short documents. A 60-word "Rust 1.94.0 released" changelog stub whose `embedding_document` (§8.1) is `Title: … / Source: … / <60 words>` will score *higher* against the interest query "Rust" than a 3,000-word essay that discusses Rust among other things — because the essay's vector is diluted across many topics. The same holds for the positive centroid. So the two semantic retrievers will systematically over-admit exactly the class §16.2 tells Stage A to punish ("announcements/roundups/vendor marketing are generally low quality"), and which `PENALTY_TITLE_PATTERNS` already exists to catch.

§13.4's "top 20 from each major retriever" protection bounds the damage to ~40 pool slots, but those slots cost facet-extraction tokens and displace real candidates.

**Fix:** apply the cheap existing hygiene to the semantic paths — require `word_count >= ~250` and `!looks_like_roundup(title)` for admission *via the semantic-interest or embedding-preference retrievers only* (an article can still enter via heuristic or auto-include). None of the plan's own motivating examples are affected: §32's "quiet 900-word post" clears 250 comfortably. Add the inverse regression test alongside §27.5's: "a 60-word release-note stub with very high interest similarity does **not** enter the recall pool."

### H4. The plan roughly doubles sequential LLM round-trips and never mentions concurrency

`score_all` batches serially (`score.rs:344`, a plain `for` over `chunks`). The new pipeline adds facet extraction over the ~240-article recall pool at `facet_batch_size` 12–16 — **15 to 20 additional sequential DeepSeek calls** — and simultaneously grows every Stage A prompt from a 200-word excerpt (`score.rs:21`) to a ~450-word beginning/middle/end sample (§10.2), which lengthens each call. Voyage adds ~13 more sequential calls at `batch_size = 32` over ~400 articles.

Token *cost* is not the issue (~240 × 600 tokens ≈ 145K input for facets, well inside `max_daily_usd = 2.0`). **Wall clock is.** This job runs on a 05:30 America/New_York timer and has to produce an EPUB before breakfast; adding 30+ sequential API round-trips to a stage that is already the slowest is a real delivery risk, and the plan's §24 stage-timing list implicitly acknowledges the concern without addressing it.

**Fix:** specify bounded concurrency (`futures::stream::iter(batches).buffer_unordered(4)`; `futures` is already a dependency) for facet extraction, Stage A, and Voyage batching, with the `UsageMeter::check_budget` gate evaluated before each spawn rather than between batches. Add the budget-trip semantics under concurrency to §23 — currently "Once tripped, remaining calls for that provider are skipped for the run" is written assuming a serial loop.

### H5. `candidate_rankings` reruns will interleave two runs' state unless the write is a full replace

> §5.4: "Persist rows incrementally as stages complete. A rerun for the same date should replace/update the day's rows deterministically."

"Replace/update" is ambiguous, and the existing house idiom is the opposite of what is needed here: `db.upsert_score` uses `COALESCE(excluded.x, scores.x)` (`db.rs:394-397`), which deliberately *preserves* prior values. If `candidate_rankings` copies it, a rerun that trips the budget at the recall stage will leave yesterday's `llm_quality_score`, `stage_a_candidate = 1`, and `selected = 1` attached to rows the current run never scored. This table is the ground truth for §21's entire evaluation program and for acceptance criterion 12; silently corrupt evaluation data is worse than no evaluation data.

**Fix:** mandate `DELETE FROM candidate_rankings WHERE run_date = ?` at the start of the recall stage, inside the same transaction as the first batch of inserts — matching `replace_issue_articles` (`db.rs:475-497`), which is the correct existing precedent. Add to §27.8: "rerunning a date that previously reached Stage B, but which now trips the budget at recall, leaves no stale Stage A/B flags."

### H6. `explain` and acceptance criterion 10 cannot be satisfied by the proposed schema

> §30, criterion 10: "`candidate_rankings` records why every eligible daily article did or did not survive each funnel stage."

But §5.4 persists "every **post-hygiene** candidate," and the most common exclusions happen *before* that: already-published, blocked domain, recently-rejected churn (`prefilter.rs:271-287`). Those articles get no row at all, so the single most frequent answer to "why did this article not show up?" is unanswerable from the table. The boolean flag set also cannot distinguish "did not make the recall union" from "made the union but was cut by the cap in §13.4."

**Fix:** add `excluded_reason TEXT` (nullable; `published | blocked | churn | not_recalled | recall_cap | stage_a_cut | mmr_cut | not_selected`) and write a row for every article the run considered, hygiene-excluded ones included, with only that column and the identifying keys populated. Cost is ~400 thin rows/day. Then criterion 10 is actually testable.

---

## Medium

### M1. Backfill has no cost estimate or guard, and embedding storage grows unbounded

§20.1 step 3: "Embed recent articles (e.g. last 90 days) for historical replay/exploration if desired." Nothing prunes `articles` — `publish::prune` only removes EPUB/XTC *files* (`publish.rs:491-547`), and `retention_days = 21` is a file policy. At ~400 articles/day, a system that has been running 90 days holds ~36,000 article rows; embedding all of them is ~47M tokens — a quarter of the lifetime 200M free allocation spent by one command with no confirmation, across ~1,125 requests.

Storage compounds: 1024 × f32 = 4,096 bytes plus row overhead, ~1.7 MB/day, **~600 MB/year** of SQLite BLOB on a VPS, with no retention policy anywhere in the plan.

**Fix:** (a) require `features backfill` to print an estimated token count and USD cost and require `--yes` above a threshold; default `--days` to 30 and default to `--rated-only`. (b) Add a retention rule to §5.1: drop embeddings for articles that are neither rated nor published and are older than N days. (c) Reconsider `output_dimension = 512` as the **default** rather than 1024 — Voyage's Matryoshka training makes 512 near-lossless for retrieval, it halves storage and dot-product cost, and the plan already requires the dimension to be configurable (§3). For a single-reader system on a small host, 512 is the better default and 1024 is the thing you evaluate into.

### M2. `Source: <feed title>` in the embedding document contradicts §8.1's own rule and degrades MMR

§8.1 is emphatic — "Do not include social score, ratings, feed prior, LLM rationale, or other ranking metadata … The vector should represent the article itself" — and then includes `Source: <feed title>` in the document format. Feed title *is* provenance metadata. Two consequences:

1. The positive centroid partly encodes "feeds the reader upvotes," double-counting with the separate `feed_affinity` signal (0.08 pre-Stage-A, 0.05 utility) that the plan went to some trouble to de-bias in §5.5.
2. **MMR degrades**: two unrelated posts from the same blog become artificially similar, so §18's diversification will suppress the second post from a favored feed as "redundant" when it is not. This is the one calculation where topical purity actually matters.

The effect is small for a 2,000-word article and material for a 200-word one — compounding with H3.

**Fix:** drop `Source:` (and probably `Author:`) from `embedding_document` v1. Keep the `EMBEDDING_DOCUMENT_VERSION` constant so this is an easy A/B later.

### M3. Max-similarity over 230 standing interests will not discriminate, because most interests are broad single words

I parsed `data/scour-interests.opml`: 230 interests, dominated by short generic terms — `Nature`, `History`, `Space`, `Engineering`, `Science`, alongside specific ones like `Gaussian Splatting`, `Writerdeck`, `tmux`. §9.2 takes `0.70 * top1 + 0.30 * mean(top3)` of raw cosine similarity across all of them.

Broad terms have high *average* similarity to everything. So `top1_similarity` will almost always be one of the generic interests, at a value that varies little between articles, and the score mostly measures "how generic is this article" rather than "does this match a stated interest." The genuinely valuable signal — "this article is *unusually* close to Gaussian Splatting" — is exactly what max-of-raw-cosine destroys.

**Fix:** z-score each interest's similarity **across the day's candidate pool** before taking top-1/top-3: `z_i(a) = (sim_i(a) - mean_a sim_i(a)) / std_a sim_i(a)`. This is free — you have already computed the full 230 × 400 matrix — and it converts "close to a broad term" into "unusually close to *this* term," which is what you want for both the score and the top-3 explanation shown to Stage B. Persist raw similarity too, as §9.2 already requires.

Relatedly: `input_type = "query"` already causes Voyage to prepend "Represent the query for retrieving supporting documents" (verified in the live API reference), so the `"Articles about: "` prefix is a second, redundant instruction that is identical across all 230 interests — it pulls all interest vectors toward each other and further compresses the `top1 − top3` gap. Worth testing the bare interest name as v2 of the interest text format.

### M4. Exploration is unbounded at exactly the moment it does the most damage

§15's V1 definition admits candidates "from a feed with low rating evidence **or** semantically outside the dense region of recent positive ratings." On day one there are no ratings, so *every* feed has low evidence and there is no positive region — the predicate is universally true, and 20 slots in the recall pool plus a guaranteed shortlist reservation (§18.2 rule 4) plus Stage B exposure go to articles chosen by `hash(run_date, article_id)`. That is a lot of deliberate noise injected during the phase where you are trying to measure whether the new ranker beats the old one.

"Semantically outside the dense region" is also the one place the plan drops below implementation grade — no definition, no threshold.

**Fix:** make `recall_exploration` scale to zero when `total_decayed_rating_weight` is below a threshold (~15), and define "outside the dense region" concretely as `positive_similarity < 25th percentile of the day's candidate distribution`. Default the reservation to 8, not 20, until Phase D.

### M5. Phase A shadow mode cannot shadow what the plan implies it shadows

> §28 Phase A: "New ranker computes in shadow mode and persists what it _would_ have done. Compare old vs new selections for several days."

The new utility score (§17) is 40% LLM quality + 15% reader fit, and those fields do not exist until Phase C enables the new Stage A. So the Phase A shadow can only compute the non-LLM 45% of utility, and "compare old vs new selections" is not achievable — the shadow shortlist would be ranked on less than half its intended signal.

That is fine, and the honest framing is more useful anyway: **Phase A should shadow the recall and pre-Stage-A stages only**, which is precisely §21.2's metric 6 ("count historical upvoted articles that would have been lost at each proposed stage") — the plan's own "one of the most important metrics." That comparison is fully computable in Phase A and is the single best evidence for whether the recall redesign is justified.

**Fix:** rewrite Phase A's exit criterion as "recall-boundary diagnostics show the union recovers upvoted articles the current top-120 would have dropped," and move selection comparison to Phase C.

### M6. Voyage's daily ceiling will not survive a rerun, unlike DeepSeek's

§23 promises "DeepSeek and Voyage meters trip independently," but §7.4 says "Database columns for Voyage tokens/cost are optional in the first migration if the JSON report is sufficient." DeepSeek's ceiling is day-scoped because `pipeline.rs:379-386` preloads `db.spend_for_date(date)` from the `runs` table. Without an equivalent column, `voyage.max_daily_usd` is per-*invocation*, and `generate --date X` reruns are a normal, documented workflow (idempotency is a stated invariant).

Impact is genuinely low — the embedding cache makes reruns nearly free — but the asymmetry is a trap for whoever debugs a budget trip later.

**Fix:** add `voyage_input_tokens` / `voyage_cost_usd` columns to `runs` in `0002` and preload them the same way. It is four lines and removes a whole class of confusion.

### M7. Replay is reproducible for scalars but not for vectors, and the plan overstates it

> §21.1: "For exact future replay, `candidate_rankings` becomes authoritative."

`article_embeddings`'s primary key is `(article_id, model, dimension)` with `input_hash` as a *non-key* column, so a re-extraction overwrites the vector in place. `upsert_article` overwrites `content_html` on every re-ingest of the same `canonical_url` (`db.rs:272-279`), which happens routinely because the 26h lookback window overlaps consecutive days. So the vectors used for MMR and for §21.2's metric 5 (shortlist diversity) are not recoverable for a past date.

Additionally, percentile normalization is **day-relative**: re-tuning weights on historical data requires recomputing percentiles from the full day's candidate set, which requires a `candidate_rankings` row for every eligible article. §5.4 provides that (and H6 would complete it), so the scalar path works — but only if the evaluator recomputes percentiles from stored *raw* values rather than trusting stored normalized ones.

**Fix:** state explicitly in §21.1 that (a) `evaluate` recomputes normalization from raw columns and never trusts persisted normalized values across code changes, and (b) vector-dependent metrics (diversity, MMR replay) are approximate for historical dates. Do not add embedding versioning to fix this — the storage cost is not worth it; just stop claiming exactness.

### M8. `--max-articles` is not currently a hard ceiling, and the plan assumes it is

> §19.2: "`--max-articles N` should **remain** a hard ceiling/override, not a target that forces filling."

It is not one today. `pipeline.rs:188` assigns `--max-articles` to `target`, and `select.rs:187-193` derives `(target - 5, target + 5)`. So `--max-articles 10` today permits **15** picks and forces a floor of 5. An agent reading "remain" will assume the behavior already exists and not fix it.

Also unaddressed: how `--max-articles` composes with the new `max_article_count = 25`. Presumably `effective_max = min(max_article_count, --max-articles)` with no floor at all.

**Fix:** reword to "`--max-articles N` must **become** a hard ceiling" and specify the composition rule.

---

## Low

- **L1 — `assemble()` loses its sort key.** §26 says to replace `ScoredArticle::combined_score()` with the `rank.rs` utility, but `assemble` uses `combined_score()` in three places (`select.rs:545`, `:609`, and via `sort_by_combined` at `:563`) for oversize trim and intra-section ordering. §19.2 says "keep the max-size trim" without saying what it sorts by. Specify: trim and order by `utility_score`, falling back to `prefilter_score` when utility is absent.
- **L2 — `select_without_llm` ordering not updated.** §23 says the DeepSeek-unavailable path should "use the enhanced deterministic ranking … rather than reverting all the way to old prefilter order," but §26's `select.rs` bullets don't mention `select_without_llm`, which calls `sort_by_prefilter` directly (`select.rs:660`). Cross-reference the two sections.
- **L3 — MMR seed rule looks like a slip.** §18.2 rule 1: "Seed with the highest-utility **non-auto** candidate." If an auto-include is the day's best article, it should seed. Also unstated: whether the rule-5 force-preserved top-20 count as `already_selected` for the max-similarity term (they must, or MMR will re-select near-duplicates of them).
- **L4 — "exploration/novelty bonus" has no definition.** §14.2 gives it 0.05 of the pre-Stage-A blend, but §15 defines exploration as boolean set membership. Either make it a flat additive bonus for `exploration_candidate` articles, or define the continuous novelty measure (e.g. `1 - max similarity to the positive centroid`).
- **L5 — `article_facets` omits `model` from its primary key** while `article_embeddings` includes `model`. Switching DeepSeek models silently reuses facets extracted by the previous one. Either add `model` to the key or state that facets are deliberately model-agnostic and that `prompt_version` is the invalidation lever.
- **L6 — `--skip-llm` / `--skip-embeddings` are inconsistent.** §23 recommends that `--skip-llm` also disable Voyage generation "and add `--skip-embeddings` later only if a real operator need appears" — but §20 already specifies `features backfill --embeddings-only`, which *is* that need. Cleaner: `--skip-llm` gates DeepSeek only, `--skip-embeddings` gates Voyage, both shipped in the same commit. One extra boolean.
- **L7 — `prefilter_keep` validation must move.** `config.rs:348` enforces `prefilter_keep >= target_article_count`. If §25 deprecates or aliases it to `stage_a_keep`, that check needs relocating, and the new constraints (`recall_pool_keep >= stage_a_keep >= shortlist_keep`, `0 <= diversity_lambda <= 1`) need adding. §26's config bullet lists some of this; add the ordering constraints explicitly.
- **L8 — Correlated "independent" signals.** §16.3 correctly forbids showing numeric preference scores to the reader-fit rubric, but reader-fit *is* shown the taste profile, whose learned-adjustments section is now (§12) enriched with facet data derived from the same ratings that produce `facet_preference`. §17's claim that "30% of the score is direct rating-derived preference" understates the true rating-derived share (~40%) and, more importantly, those components share error. Not a blocker — just note it as a correlation to watch in §21.2 rather than asserting independence.
- **L9 — Dry-run behavior with `candidate_rankings` unstated.** Articles are persisted even under `--dry-run` (`pipeline.rs:320-332`), so ranking rows will be written on dry runs. Probably desirable for Phase A shadow work; say so.

---

## Nits

- **N1 — Stale file references.** §"read the current curation implementation" lists `src/curate/profile.rs`; it is `src/curate/profile/mod.rs` plus `themes.rs`. §6's module layout repeats the flat `profile.rs` and omits `editorial.rs`, which exists. An agent following §6 literally might collapse the profile module. For a plan that is explicitly "implementation-grade," these should be exact.
- **N2 — "configured lookback" for the churn rule doesn't exist.** §13.1 says "within the configured lookback"; `STALE_LOOKBACK_DAYS` is a `const` (`prefilter.rs:59`), not config. Either make it config as part of this work or drop the word.
- **N3 — Acceptance criterion 11 is nearly vacuous.** "`features backfill` can populate at least all historical rated articles **without network calls in tests**" — every test is offline per notes §6. The meaningful criterion is "backfill is resumable and idempotent: re-running it makes zero API calls when the cache is warm."
- **N4 — Root config section is not typo-protected.** Nested config structs use `#[serde(deny_unknown_fields)]`, but `Config` itself deliberately does not (`config.rs:41-43`, so that bare `DAILY_EPUB_SECRET` passes through). A `[voyages]` typo will therefore be silently ignored and defaults used. Worth a line in §7.1 telling the operator to verify via the startup log rather than assuming.
- **N5 — `voyage.max_daily_usd = 0.25` is a runaway guard, not a cost ceiling.** At `$0.02`/Mtok that trips at 12.5M tokens/day, ~25× expected volume, and the meter cannot know whether the 200M free allocation is exhausted. Fine as designed — just describe it as a runaway guard so nobody tunes it as if it were a bill.
- **N6 — `PENALTY_TITLE_PATTERNS` becomes redundant once facets exist.** `ArticleFormat::{roundup, release_notes, announcement}` subsumes the 22-pattern title list (`prefilter.rs:27-50`) with far better recall. §26 says to preserve current heuristic values for evaluation, which is right for V1; add a note that retiring the title-pattern penalty is a Phase E cleanup candidate.
- **N7 — Verified-facts section deserves a re-verification date.** §3's Voyage facts are correct as of today (I confirmed model, context, dimensions, dtypes, 1,000-input/1M-token request limits, `$0.02`/Mtok, and the 200M free allocation against the live docs). Add a note that this block should be re-checked whenever `model` changes, in the same spirit as the 2026-08-15 notes.

---

## Alternatives

### A1. Skip facets in V1; fit a regularized linear probe on the embeddings instead *(strongest alternative)*

The plan's own §11.2 already computes a positive and a negative centroid. The difference of two class centroids is the closed-form solution of a specific naive classifier — it weights every embedding dimension equally. A **ridge-regularized logistic regression** on the same labels is the same idea done properly: it learns *which* dimensions discriminate, it is convex, it needs no new dependency (a few hundred lines of gradient descent over `Vec<f32>`, or closed-form ridge on 256-d), and it handles the correlated-positives problem in §11.2 that the centroid difference cannot.

Crucially, it addresses H2's evidence problem head-on: with 63 labels you cannot estimate 84 facet parameters, but you *can* fit a heavily-regularized 256-dimensional linear model, because regularization is exactly the tool for the low-n regime — and it costs **zero DeepSeek tokens**.

"Likes first-hand postmortems, dislikes vendor announcements" is substantially linearly separable in a good embedding space; that is what embeddings are for. Facets buy explainability and the §12 profile-prompt enrichment, both real, but neither requires facets to be in the *scoring* path.

**Prefer this when:** rating volume is under ~200 and you want the immediate-feedback property (§30 criterion 4) working on day one. **Prefer the plan's approach when:** rating volume is high enough to estimate facet stats, or when the explainability of "you downvote `commerciality = product_marketing`" is worth more than ranking accuracy — which for a single-reader system it genuinely might be.

**Concrete middle path:** ship facets for extraction, storage, `explain`, and the §12 profile prompt (all cheap and all valuable), but have the *numeric* scoring path use a linear probe on embeddings until facet evidence crosses a threshold. That gets both properties and lets §21 compare them empirically.

### A2. Threshold clustering instead of MMR for diversification

§18's MMR introduces `lambda = 0.82`, a magic number whose meaning is not interpretable in isolation, and it composes awkwardly with rule 5 ("preserve the top ~20 by raw utility regardless of MMR" — at which point you are no longer running MMR, you are running force-include-then-MMR).

**Alternative:** single-linkage cluster the shortlist candidates at cosine > ~0.85, then take the top-N by utility with a cap of 2 per cluster. One interpretable parameter (the similarity threshold, which you can eyeball against real article pairs), trivially composable with auto-includes and the top-20 preservation rule, and dramatically easier to render in `explain` ("suppressed: 3rd article in the cluster led by #4821").

**Prefer MMR when** you want a smooth relevance/diversity tradeoff across the whole ranking. **Prefer clustering when** the actual problem is the one §32 describes — "six articles about the same AI news cycle" — which is a discrete cluster-cap problem, not a continuous one. I would ship clustering first and reach for MMR only if it proves too blunt.

### A3. Two vectors per article: title+lead for interest matching, full body for preference and MMR

Addresses H3 and M2 at the root rather than by filtering. The short-document bias in interest matching is not really a bug — it reflects that interest matching *should* be title-like. The problem is using one vector for two jobs with opposite length preferences.

Embed `Title + first ~100 words` with `input_type = "document"` for standing-interest matching, and the full capped body for the rating centroids and MMR. Cost is one extra Voyage call per batch (~13/day, negligible against a 200M free allocation), storage doubles (mitigated by A4), and both jobs get a vector shaped for them. The `article_embeddings` PK already accommodates this if you add a `kind` column or fold it into `model`.

### A4. Default to `output_dimension = 512`

Voyage's Matryoshka training makes 512-d near-lossless for retrieval on general text. It halves BLOB storage (~300 MB/year instead of ~600), halves every dot product (230 interests × 400 articles × 2 = 184K dot products/day either way — not a bottleneck, but MMR is O(n²)), and the plan already mandates configurability. For a single-reader system where the DB shares a VPS with the EPUB output directory, 512 is the better *default*; 1024 is the thing you evaluate into if §21 shows a measurable difference. This also makes A3's second vector free in storage terms.

---

## Open Questions

These block confident approval; each is answerable quickly and each changes what gets built.

1. **What writes `scores.llm_score` after the Stage A split?** (C2) Without an answer the churn rule dies silently. If the answer is "nothing, migrate `recently_low_scored_ids` to `candidate_rankings`," the `0002` migration and `prefilter.rs` change scope.
2. **How many ratings exist in the production database today?** If the count is under ~100, H2 stops being a tuning concern and becomes blocking: the entire facet-scoring path would be dead weight for the first several months, and A1 becomes the right V1.
3. **How many rows are in the production `articles` table?** Determines backfill token spend (M1) and whether the ~600 MB/year embedding storage projection is tolerable on the host. Answerable with one `SELECT COUNT(*)`.
4. **What is the current wall-clock runtime of `generate`, and what is the hard publish deadline?** (H4) If the run currently takes 4 minutes against a 30-minute window, serial batching is fine and H4 downgrades to a nit. If it takes 20 minutes, concurrency is mandatory before any of this ships.
5. **Does `[curation.personalization] enabled = false` (§28) disable embedding *generation*, or only the new ranking path?** If it disables generation, Phase A collects no data and the shadow phase is impossible. If it does not, the flag name is misleading and the config docs need to say so.
6. **Is the reader willing to accept a visibly noisier paper during Phase A–B?** M4's exploration reservation plus H3's short-document admissions will both be most visible exactly when the system has the least evidence. If the answer is no, exploration should start at zero and ramp with rating volume.
