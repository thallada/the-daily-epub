# Review: Personalized Ranking, Embeddings, Facets, and Feedback

The plan has a strong overall direction—union-based recall, separate descriptive facets, explicit quality/fit scores, pre-editor diversification, and removal of forced filler are all sensible—but it is not ready to execute as written. The principal blockers are that the proposed persistence model cannot provide the promised per-run/exact replay guarantees, facet cache invalidation contradicts its schema, historical runs can leak future feedback, the single positive/negative centroid is too lossy for the stated multi-interest personalization goal, and the rollout/budget and target/ceiling semantics are internally inconsistent. Resolve the High findings before implementation; the remaining items can be handled during staged delivery.

## Critical

No critical findings.

## High

### 1. The facet cache key cannot honor the stated invalidation rules

Evidence: §5.3 defines `PRIMARY KEY (article_id, schema_version)` while also storing `model`, `prompt_version`, and `input_hash`; it says “`prompt_version` changes when instructions change” and “Reuse a facet row only if schema version and content hash match.” §10.4 then describes the cache as `(article_id, schema_version, input_hash)`.

With the proposed primary key, a prompt or model change either reuses stale output or overwrites the previous observation. Worse, §10.3 proposes placing the mutable reader taste profile in the system prompt for a supposedly descriptive extraction task, but neither the profile version nor its text hash participates in invalidation. Facets would therefore be reader/profile-dependent while appearing globally reusable and stable.

Recommendation:

- Remove the taste profile entirely from facet extraction. Use a stable, reader-independent descriptive system prompt and treat article text as untrusted quoted data.
- Make the cache identity explicit, for example `(article_id, schema_version, model, prompt_version, input_hash)`, or use a surrogate `facet_observation_id` plus a uniqueness constraint over those fields.
- Define `input_hash` over the exact effective facet input: excerpt-format version, title/author/source fields actually sent, and the representative text. Do not call it merely a “content hash.”
- Decide whether old observations are retained for replay or superseded; do not silently overwrite them if exact replay remains a goal.

### 2. `candidate_rankings` is neither per-run nor sufficient for exact replay

Evidence: goal 7 promises “replayable from persisted per-run features,” §5.4 calls the table essential, and §21.1 says it becomes authoritative “For exact future replay.” Yet its primary key is `(run_date, article_id)`, despite the existing `runs` table allowing multiple invocations per date. The plan explicitly says reruns replace the date’s rows.

This loses shadow-versus-live results, failed/partial attempts, changed configurations, and the exact feature versions used. It also omits the ranking configuration/profile/model/prompt versions and per-retriever membership/cut reason. Mutable `article_embeddings` and `article_facets` rows can be overwritten, so a later MMR replay cannot reconstruct the original pairwise similarities. `explanation_json` is not a substitute unless its required schema and versioning are specified.

Recommendation:

- Key snapshots by `run_id` (foreign key to `runs`) and `article_id`; keep `run_date` as an indexed denormalization.
- Add a run-level immutable ranking manifest containing personalization mode (`shadow`/`live`), all normalized weights and thresholds, model/dimension versions, facet schema/prompt version, embedding/excerpt format versions, profile version/hash, feature availability, and deterministic algorithm version/seed.
- Persist retriever membership and explicit terminal reason (`blocked`, `published_before_cutoff`, `not_in_union`, `recall_cap`, `stage_a_cut`, `mmr_cut`, etc.). The current booleans cannot explain why an article failed to enter a stage.
- Either retain versioned embedding/facet observations referenced by the run or persist enough pairwise/MMR inputs to reproduce the shortlist. Narrow the claim from “exact replay” to “score diagnostics” if that storage is not desired.
- Write a run snapshot transactionally or mark its lifecycle (`running`, `complete`, `failed`) so evaluation does not treat a partial run as authoritative.

### 3. Historical generation/evaluation has undefined “as-of” semantics and can leak future data

Evidence: §11 says to build preference state “at the beginning of every generate run” using recent ratings, §20 adds historical backfill/evaluation, and §21 proposes historical replay. The plan does not say that ratings must be bounded by the simulated run time. It also says recall should reuse current prefilter history logic (§13.1/§26), but the current repository’s `previously_published_ids()` returns articles from every issue, including issues after a historical target date, and the current profile code anchors its rating window to `Timestamp::now()`.

A replay of August 1 performed on August 18 could train on August 2–18 votes, use the latest prose profile, and exclude an article because it was published on August 10. That produces optimistic evaluation and non-reproducible historical runs.

Recommendation:

- Define one `as_of` timestamp for every run/evaluation and require all ratings, issue history, feed priors, profile state, and candidate publication/history queries to use it.
- For an issue dated `D`, exclude only articles published in issues before the chosen cutoff; never use future issue rows.
- Store or reconstruct profile versions by effective interval. If historical prose profiles are unavailable, explicitly disable that feature in replay and label the limitation.
- Separate “regenerate an old issue using knowledge available today” from “historical replay as of that day” as distinct CLI modes.
- Add leakage tests in which future ratings/issues exist but do not affect an as-of replay.

### 4. One global positive and negative centroid collapses the reader’s multi-modal taste

Evidence: §9 describes roughly 220 standing interests, while §11.2 reduces all recent upvotes to one unit-normalized positive centroid and all downvotes to one negative centroid. §32 expects a highly favored niche to be rescued early.

A single average vector is a poor representation of a reader who likes unrelated clusters such as Rust, local Boston reporting, books, and e-ink. Niche vectors may be only weakly similar to the global mean. The negative centroid also conflates topic rejection with format/quality rejection; the plan acknowledges this caveat but still gives the combined embedding preference 15–28% of ranking weight. This can work against the main recall goal before facets or Stage A can rescue the article.

Recommendation: evaluate a signed, time-decayed top-k neighbor signal as the V1 baseline (`top/mean similarity to recent upvotes` minus a configurable downvote term), optionally grouped by topic cluster. At this scale it is simpler than centroid maintenance and preserves multiple modes. If centroids remain, keep several clusters or combine centroid and nearest-neighbor signals, and make a synthetic multi-interest recall test an acceptance criterion—not just a one-topic centroid test.

### 5. Shadow mode can consume or trip the same DeepSeek budget needed by the production selector

Evidence: §28 Phase A says facets and new snapshots run in shadow while “Existing production selection remains authoritative.” §10.3 adds facet extraction for about 240 articles, and §23 says DeepSeek’s meter trips independently once its ceiling is reached. The current pipeline uses a single per-day DeepSeek meter for scoring, selection, profile, and editorial work.

If shadow facet calls run before the existing production stages, they can exhaust the shared daily ceiling and force the supposedly authoritative path into fallback. This is a behavior change, not passive shadowing. Persisting Voyage cost only in report JSON also cannot reliably preload/enforce a provider-specific daily ceiling across reruns or dry runs.

Recommendation:

- Run authoritative production calls first, then shadow work from a separately configured shadow budget, or reserve explicit provider/stage budget slices before shadow calls.
- Persist provider usage by `run_id` in queryable columns or a `run_provider_usage` table and preload same-date spend, as the current DeepSeek path does. “JSON is sufficient” is not compatible with a durable daily guardrail.
- State whether failed requests, retries, dry runs, and concurrent runs count toward the budget, and make reservation/accounting atomic enough to prevent two runs overspending simultaneously.

### 6. Soft target, hard ceiling, auto-includes, and `--max-articles` have contradictory precedence

Evidence: §19.2 says target approximately 20, never exceed `max_article_count` 25 “plus unavoidable auto-includes,” then says `--max-articles N` is a “hard ceiling/override.” §18.2 also allows auto-includes and other protected sets to exceed nominal shortlist size. In current code, `--max-articles` replaces `target_article_count`, so merely reusing the existing plumbing would tell Stage B to aim for the ceiling rather than keep the normal soft target.

Recommendation:

- Carry `soft_target` and `hard_max` as separate values through the pipeline and Stage B prompt.
- Define one precedence rule for auto-includes. Either they can exceed the hard max (then call it a normal-content ceiling and report the exception) or the CLI ceiling is truly hard (then reject conflicting configuration or specify which auto-includes win).
- Define the exact shortlist-cap precedence among raw-utility preservation, exploration reservation, auto-includes, and MMR; specify whether protected items seed MMR similarity calculations.
- Add tests for `--max-articles` below, equal to, and above the soft target, including excess auto-includes.

## Medium

### 1. Missing-signal normalization is underspecified and can turn degradation into a penalty

Evidence: §14 percentile-normalizes daily signals, while §23 says missing embedding signals become “neutral, not zero-quality penalties.” The plan does not define the empirical population, neutral value, behavior for all-equal/singleton inputs, or whether missing values participate in percentile ranking.

Recommendation: define a typed normalization contract. Exclude missing values from the empirical CDF, assign missing signals an explicit neutral value (normally 0.5), return 0.5 for degenerate/all-tied distributions, and store availability flags alongside raw and normalized values. Test mixed cached/missing embeddings so an outage does not systematically demote uncached articles.

### 2. The preference evidence model needs vote-time and exposure semantics tightened

Evidence: §11 decays ratings by `age_days` without saying whether age is based on `rated_at` or `issue_date`; §5.5 keeps `included` exposure metadata but §11.3’s support uses only explicit votes. A vote can arrive long after publication, and repeated flips update `rated_at` in the current schema.

Recommendation: use `rated_at` for behavioral recency but document that a flip resets recency, or preserve the initial and last-changed timestamps separately. Keep exposure out of the label, as planned, but persist whether/when an item was shown so evaluation can distinguish unshown, shown-unrated, and rated items. Anchor all calculations to the run’s `as_of` timestamp.

### 3. Feed-prior rebuilding needs an atomic, fully specified source policy

Evidence: §5.5 says split credit among direct `Feed` sources, fall back to `best_entry_id`, then use a “weighted mean” of future source priors without defining weights. §22 allows either recomputation or transactional updates. The current rebuild path performs independent upserts and does not clear obsolete rows.

Recommendation: define deduplication by `feed_id`, the exact candidate-affinity weights, and what happens when the best-entry fallback is itself a discovery feed. Recompute the complete v2 table in one transaction (temporary table plus replace, or delete/upsert under a transaction) so generation never reads a partial rebuild and stale zero-evidence rows disappear.

### 4. LLM inputs need an explicit prompt-injection and data-handling policy

Evidence: §§10.3, 16.4, and 19.1 send extracted third-party article text to DeepSeek, and §8 sends up to 60,000 characters to Voyage. The plan discusses API-key secrecy but not untrusted instructions embedded in article text, provider data handling, or operator opt-out for private/authenticated feeds.

Recommendation: delimit article content as data, explicitly instruct the model to ignore instructions found inside it, validate outputs only against offered IDs/enums, and cap/escape metadata consistently. Document that article text is sent to external providers and add a feed/domain-level “local only / do not send” policy if private feeds are possible. Confirm the providers’ retention/training terms before rollout.

### 5. The new facet stage is expensive but its necessity is not tested against cheaper alternatives

Evidence: §10 adds a DeepSeek call for roughly 240 articles every day before Stage A, while §27 tests parsing and caching but not whether facets are stable or improve ranking. Many proposed fields (depth, technicality, temporal orientation, commerciality) may be derivable cheaply or bundled into Stage A for only 120 candidates.

Recommendation: in shadow mode, measure inter-run facet stability and incremental ranking value. Consider extracting cheap deterministic facets before recall, asking Stage A for descriptive facets alongside quality/fit for its 120 candidates, or using the embedding provider only for topic retrieval and delaying facets until enough ratings justify them. Keep the separate 240-item call only if it measurably rescues candidates at the pre-Stage-A cut.

### 6. The migration and rollout plan needs explicit compatibility behavior for partially deployed code

Evidence: §5 says keep `scores` temporarily, §26 says split `LlmScore`, and §28 phases behavior over several deployments, but the plan does not define which binary versions can safely run against migration 0002 or how Stage A v1/v2 values coexist. `scores.llm_score` becomes semantically ambiguous during Phase B/C.

Recommendation: version the assessment (`assessment_version`, model, prompt version), keep v1 and v2 writes distinguishable, and specify read precedence during each phase. Add forward/backward deployment tests or explicitly require a stop-the-world binary migration for this single-operator service.

## Low

### 1. The provider facts are current, but pricing and compatibility should remain metadata

The choices in §3 match the current official Voyage documentation: `voyage-4-lite` supports the stated context length/dimensions and request limits, and the listed price is currently correct. The provider also states that Voyage 4-series embeddings are mutually compatible, which means the plan’s blanket “Never compare vectors with different model” rule is conservative rather than technically required. Conservative isolation is reasonable for reproducibility; record model IDs in the run manifest and only relax compatibility after an explicit evaluation. See [Voyage embeddings](https://docs.voyageai.com/docs/embeddings), [API reference](https://docs.voyageai.com/reference/embeddings-api), and [pricing](https://docs.voyageai.com/docs/pricing).

### 2. Character caps must be Unicode-safe and do not guarantee the claimed token margin

Evidence: §7.3 uses 60,000 normalized characters per article as an aggregate-token safety proxy, while §8.1 says to cap text but only §10.2 explicitly warns against unsafe UTF-8 splitting.

Recommendation: make every cap Unicode-safe, enforce both per-input and aggregate character budgets before batching, and handle server truncation/token-limit errors explicitly. Log truncation counts without article text.

### 3. MMR should clamp/validate similarities and define the utility scale

Evidence: §18 uses `normalized_utility` but §17 stores utility on a 0–100 scale. Floating-point embeddings loaded from storage are only length-checked, not checked for finite values or unit norm.

Recommendation: define whether MMR uses utility divided by 100 or a percentile, reject non-finite vector values, and normalize or verify vector norms within tolerance before dot products. Clamp small floating-point overshoots when using cosine-like scores.

## Nits

- §5.4 says “every post-hygiene candidate,” while acceptance criterion 10 says “every eligible daily article.” Define whether blocked, previously published, and recently rejected articles receive rows with terminal reasons. Logging them is necessary to explain hard exclusions.
- Use SQLite integer `0/1` plus `CHECK` constraints for ranking flags if strictness matters; SQLite does not enforce a separate Boolean storage class.
- `specific_topics: Vec<String> // 0..=4` and other cardinality comments require explicit validation after deserialization; Serde alone will not enforce them.
- The exploration hash should include an algorithm/version salt in the run manifest so an implementation change does not masquerade as reproducible behavior.
- Replace approximate terms such as “top ~20” and “small number of exploration candidates” with configuration fields and deterministic defaults before handing the plan to an implementation agent.

## Alternatives

### Alternative A: signed nearest-neighbor preference instead of one centroid

Keep recent rated document embeddings and compute a time-decayed top-k positive similarity and top-k negative similarity for each candidate. This preserves unrelated interest clusters, is explainable (“similar to these three upvotes”), and is trivial at the repository’s scale. Prefer this for V1 when ratings are sparse and multi-modal. Add clustered centroids later if the rating history becomes large enough for neighbor scans to matter.

### Alternative B: immutable feature observations plus run manifests

Store embeddings and facets as immutable observations keyed by model/prompt/input versions, and let each `candidate_ranking` reference the exact observation IDs plus a run manifest. Prefer this when reproducibility and offline tuning are real product requirements. If storage simplicity matters more, retain mutable caches but explicitly downgrade the promise to diagnostic snapshots rather than exact replay.

### Alternative C: fold facets into Stage A initially

Ask the existing 120-candidate Stage A call to return descriptive facets alongside quality and reader fit, then use those facets in final utility and future preference learning. Prefer this during early shadowing to avoid doubling DeepSeek candidate volume. A separate 240-candidate facet stage is preferable only if shadow evaluation shows facet preference materially improves the 240-to-120 cut.

### Alternative D: production-first shadow execution

Run the unchanged authoritative curation path first, reserve enough budget for all publication-critical calls, and execute new facet/ranking work afterward with its own cap. Prefer this during Phase A because it makes “shadow” genuinely non-interfering. Once the new path becomes authoritative, consolidate stage budgets under the provider-level ledger.

## Open Questions

1. Is a historical `generate --date D` supposed to reproduce knowledge available on day D, or intentionally re-curate D using today’s ratings/profile? The database queries and CLI need separate semantics for these two operations.
2. Is `--max-articles N` an absolute ceiling even when there are more than N auto-includes? Which requirement wins?
3. Should facet labels be globally descriptive and reader-independent? If yes, confirm that the taste profile will be removed from the facet system prompt.
4. Is exact replay a hard requirement? If so, is retaining immutable feature observations and run manifests acceptable, including the extra storage?
5. How much of the DeepSeek daily budget is reserved for publication-critical Stage A/B/editorial work versus shadow facets, profile rebuilds, and evaluation/backfill?
6. Are any Miniflux feeds private, authenticated, or otherwise unsuitable for sending article text to Voyage/DeepSeek?
7. What minimum evidence must the shadow evaluation meet before moving between rollout phases (number of runs/ratings and explicit pass/fail thresholds), rather than “several days” or “monitor”?
