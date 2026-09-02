# Re-review — Personalized Ranking, Embeddings, Facets, and Feedback (v3)

**Plan:** `docs/plans/2026-08-17-personalized-ranking-and-facets.md`  
**Date:** 2026-08-19  
**Scope:** revision 3, after the R3 amendments

## Verdict

Revision 3 resolves every R3 finding thoughtfully, and the main architecture is now coherent: the provider boundary is explicit, run eligibility uses the real lifecycle, profile text is versioned, facets are post-admission, and leader clustering is specified rather than mislabeled. I still would **not begin the full implementation unchanged**. Two remaining correctness defects sit below those amendments: the single global evidence weight can fully activate a learned signal backed by almost no compatible observations, and the stated `as_of`/backfill contract cannot be implemented with the proposed mutable feed-prior and date-keyed score stores. The generation lease also needs fencing and an active heartbeat before it can provide the mutual exclusion the plan claims. These are bounded amendments; the union-admission, utility, facet, and diversification decisions do not need to be revisited.

## Critical

### C1. One global evidence weight does not measure evidence for each learned signal

Evidence:

- §14 defines one `W` as the decayed weight of **all** ratings and uses it to gate `embedding_preference`, `facet_preference`, and `feed_affinity`.
- §13.4 says a rating is useful for embedding/facet preference only when its article has the corresponding features and that missing historical features “reduce evidence.”
- §25.1 guarantees protected articles receive no embedding or facets, but then says their ratings still update both `feed_priors_v2` **and the kNN preference state**.

Those statements cannot all hold. A newly protected article has no embedding, so its rating cannot enter kNN. More generally, 19 ratings without compatible embeddings plus one compatible rating produce `W = 20`, fully activating an embedding-preference signal learned from one example. The same issue applies independently to facets. Presence-aware blending does not fix it: once one compatible example makes the signal `Present`, the unrelated global `W` gives it full configured weight.

This recreates the sparse-evidence failure the ladder was designed to prevent, especially after provider opt-outs, partial backfills, model/dimension changes, or facet parse failures.

**Required amendment:** maintain evidence per learned signal:

```text
W_embedding = decayed weight of ratings with a compatible article embedding
W_facet     = decayed weight of ratings with usable scored facets
W_feed      = decayed weight successfully attributable to a feed
W_global    = decayed weight of all ratings (telemetry/exploration maturity only)
```

Compute each gate from its own `W_signal`; facet value support remains an additional inner gate. Persist all four values in the finalized manifest/explanation rather than one `rating_evidence_weight`. A protected rating may update feed affinity and `W_global`, but it cannot update kNN or facet preference unless a locally generated compatible feature exists. Add a regression test where 20 total ratings but only one embedding-backed rating leave the embedding gate near its floor rather than fully open.

### C2. The `as_of` and historical-backfill contracts remain unsatisfiable with the proposed stores

Three plan requirements conflict:

1. **Feed priors:** §6.1 requires feed priors recomputed from ratings bounded by `as_of`, while §7.7 defines one singleton `feed_priors_v2` table rebuilt by `DELETE; INSERT`. A historical replay would overwrite today’s materialized priors with a past snapshot. Meanwhile the rating server can rebuild the same table while generation holds its lease, because `serve` is not lease-protected. Atomic replacement prevents partial reads but not “wrong snapshot won the race” or future-data leakage relative to a run’s fixed `as_of`.
2. **Churn scores:** §6.1 says churn is as-of bounded, but §18.4 keeps it on `scores.llm_score`. That table is keyed only by `(article_id, run_date)` and has no `run_id` or creation timestamp. A recuration performed on August 19 for an August 1 issue writes an August 1 score; a replay as of August 5 cannot tell that the score was created in its future. The proposed `recently_low_scored_ids(threshold, since, until)` bounds the nominal date, not observation time.
3. **Backfilled features:** §6.1 says replay ignores embedding/facet rows created after `as_of`, while §27.1 says to backfill embeddings/facets now and then replay historical dates. Those newly backfilled rows are, by definition, created after the historical `as_of`, so the replay must ignore the very features the backfill was intended to supply.

Acceptance criterion 13’s leakage test cannot make all three behaviors correct without choosing stronger semantics.

**Required amendment:** separate two operations that v3 still calls `replay`:

- **Fidelity replay:** reconstruct only information actually available then. Ignore later-created features; derive feed affinity in memory from canonical ratings bounded by `as_of`; read churn assessments from run-scoped observations joined to `runs.started_at <= as_of`. Legacy `scores` rows without observation time must be excluded or explicitly treated as unverifiable.
- **Counterfactual evaluation:** ask how the new algorithm would rank a historical candidate set using features computed later. Permit marked post-hoc embeddings/facets, record `feature_time_policy = counterfactual`, and label results approximate because `content_html` is mutable.

Do not rebuild the global `feed_priors_v2` table for replay. Either compute a run-local `HashMap<FeedId, FeedPriorV2>` directly from bounded ratings, or persist a run-scoped snapshot. Keep the singleton table only as a current/live cache if the rating endpoint still needs it. Move the churn rule to `candidate_rankings.llm_quality_score` joined through eligible runs/manifests, or add `run_id`/actual `scored_at` provenance to `scores`; nominal `run_date` is insufficient.

## High

### H1. The expiring generation lease is not fenced and can expire during a live stage

Evidence:

- §7.4c gives the lease a 30-minute default TTL and refreshes it only “at each stage boundary.”
- §24.1 applies the same lease to `features backfill`, which may run for many batches, and acknowledges that Stage A wall clock is currently unmeasured.
- §31.11 tests racing acquisition and expired recovery, but not a live operation lasting longer than the TTL or an old holder acting after reclamation.

If one stage lasts longer than 30 minutes, a second process may reclaim the lease while the first is still running. Both then proceed. Worse, an RAII guard from the original process can later delete or refresh the replacement owner’s row unless every operation is conditional on an unforgeable ownership token. The claimed guarantee—“two concurrent `generate` invocations cannot both proceed”—does not hold.

**Required amendment:**

- assign each acquisition a random fencing token/generation;
- refresh and release only with `WHERE name = ? AND token = ?`;
- run a background heartbeat at a fraction of the TTL (for example TTL/3), not only at stage boundaries;
- if heartbeat/refresh loses ownership, abort before the next external call or persistent side effect;
- make a stale holder unable to publish even after another process has reclaimed the lease.

Add tests for a stage exceeding one TTL, reclamation followed by the old guard dropping, and a stale holder attempting to refresh/release/publish. An OS file lock is a simpler alternative on one host (§ Alternatives).

### H2. Phase B’s interleave rule depends on Phase C utility and does not guarantee exposure

Evidence:

- §32 says Phase B enables new admission but keeps the existing final selector.
- The Stage A quality/fit split and new utility do not become authoritative until Phase C.
- Phase B nevertheless reserves a slot for the “highest-utility” union-only candidate.
- The same sentence calls it an “issue slot” while preserving “Stage B’s right to refuse.”

At Phase B there is no v3 utility score to rank this cohort unless Phase C work has silently moved earlier. If Stage B may refuse, the slot is not an issue slot and the seven-run exit window can yield zero interleaved exposures, defeating the stated purpose of collecting real labels.

**Required amendment:** choose one exact Phase B behavior:

- rank candidates using an available Phase B score (preliminary blend or the legacy combined Stage A score), apply an explicit minimum quality threshold, and deterministically reinsert one qualified union-only candidate after Stage B; or
- call it an interleave nomination, let Stage B refuse, and require a minimum number of actual exposures before the phase can exit rather than “7 runs.”

If v3 utility is computed in Phase B shadow solely to choose the interleave, state which Stage A response fields exist then and move the necessary implementation work out of Phase C.

### H3. Migration `0002` cannot seed a hashed profile row as ordinary SQL without a bootstrap design

§7.4b and §31.13 require migration `0002` to seed `taste_profile_versions` from three existing `kv` values, including:

- parsing the JSON `profile_version` payload to obtain `version` and `built_at`;
- hashing the existing profile text with SHA-256 for non-null `profile_hash`;
- preserving learned text when present.

SQLite has no built-in SHA-256 function, and the repository’s migrations are plain SQL. Even if JSON extraction is available, the hash cannot be produced by the shown migration alone. A fake/empty hash would violate the manifest identity contract.

**Required amendment:** specify a Rust bootstrap immediately after schema migration:

1. open one transaction;
2. read and parse the current `kv` values;
3. compute the canonical hash in Rust;
4. insert the first history row idempotently;
5. commit before any profile load/rebuild.

Alternatively register an explicit SQLite hash function, but that is more machinery for a one-row migration. Test absent, malformed, and already-seeded `kv` states, not only the happy path.

## Medium

### M1. Adjudications are not tied to the run or algorithm that produced the sample

The §7.4d primary key is `(run_date, article_id)`, while all ranking snapshots correctly use `run_id`. A date can have live, shadow, dry-run, and rerun manifests with different candidate sets/configurations. `evaluate --adjudicate --date D` therefore cannot prove which run defined “union-only” and “control,” and a later rerun can change the explanation behind an existing verdict.

The text also says the table ensures an article is “never re-scored twice,” but including `run_date` permits the same overlapping article to be adjudicated again the next day.

**Recommendation:** introduce an adjudication batch keyed to `run_id`, algorithm version, and a persisted deterministic sample seed. Store randomized display order separately from hidden arm, and decide whether deduplication is per run, per article globally, or after a cooldown. The CLI should print the selected run ID before collecting labels.

### M2. A valid zero-candidate run has no “first ranking rows” with which to finalize its manifest

§7.4 finalizes the manifest in the same transaction as the first `candidate_rankings` inserts. Empty ingest windows and all-hygiene-excluded days are valid degraded/empty outcomes, but supply no first row. Such a run remains provisional forever and is excluded from every diagnostic even though its ranking configuration and zero-candidate outcome are meaningful.

**Recommendation:** finalize in one transaction that inserts zero or more initial rows; row existence must not be the trigger. Add a zero-eligible-candidate integration test.

### M3. `stage_completeness_json` is authoritative but nullable and structurally unversioned

Per-metric eligibility now depends on fields inside `stage_completeness_json`, yet the column is nullable, has no schema version, and no final-manifest invariant requires valid stage coverage. A malformed or old-shape JSON object can silently change metric denominators.

**Recommendation:** define a versioned typed structure, require it when `manifest_status = 'final'` in application logic, and treat parse/unknown-version failures as ineligible with an explicit diagnostic. Consider normal columns for the few coverage counts most often queried; JSON is reasonable only if all filtering occurs after typed decoding rather than ad hoc SQL JSON paths.

## Low

### L1. `explain` still says “latest complete run”

§26.3 says `explain` defaults to the “latest complete run for the date,” reintroducing the nonexistent lifecycle term fixed in §7.6. Say “latest eligible run under the typed predicate” and specify whether dry-run/shadow runs are excluded by default.

### L2. The proposed `QueryFragment` return type is not part of the current SQLx design

§7.6 sketches `fn evaluable_runs(kind: EvalKind) -> QueryFragment`, but this repository uses runtime `sqlx::query` and has no query-fragment abstraction. Keep the typed single-source requirement, but specify an implementable shape: a DB method that executes the full query, a `QueryBuilder<Sqlite>` helper, or a typed status/stage predicate applied after loading rows.

### L3. “Failed requests count actual tokens spent” is not always observable

§24 requires failed requests and retries to count actual provider tokens. For transport failures and some 5xx responses, no usage payload exists even if a provider ultimately bills work. Reserve-before-send is still correct, but reconciliation cannot always know “actual.”

Document the conservative rule: retain the estimate when actual usage is unavailable, reconcile only from trustworthy usage responses, and report estimated versus provider-reported usage separately.

## Nits

- Acceptance criteria number `18` appears twice; the final API-key/vector criterion should be 23.
- The R1 resolution table still points leakage tests to §31.9; they moved to §31.12.
- §24.1’s parenthetical calls `--dry-run` read-only and then immediately says it persists data. State simply that dry runs acquire the lease.
- §15.1 says a dedicated facet stage is triggered by “metric 6 restricted to facet-driven admissions,” but facet preference is now forbidden in admission until that stage exists. Frame the trigger as an offline counterfactual evaluation, not an existing admission metric.

## Alternatives

### Alternative A: Per-signal evidence gates

Keep the current linear ramp but instantiate it independently for embedding, facets, and feed affinity using only compatible observations. Use global `W` solely for exploration maturity and general telemetry. Prefer this because it preserves the plan’s simple mathematics while making feature outages, opt-outs, and model migrations honest.

### Alternative B: Run-local derived preference snapshots

Treat `ratings` as canonical and derive kNN examples, facet statistics, and feed priors into one immutable in-memory `PreferenceState` per run, all bounded by `as_of`. Persist only the resulting raw candidate signals and evidence counts in run snapshots. Keep `feed_priors_v2` as an optional current-serving cache, never as replay input. Prefer this over versioning every aggregate table at the project’s scale.

### Alternative C: Separate fidelity replay from counterfactual evaluation

Use `replay` only for “what information was available then?” and add an explicit `evaluate --counterfactual-features` mode for post-hoc embeddings/facets. Prefer this when offline algorithm comparison matters more than exact historical feature provenance. The manifest must record the feature-time policy so results cannot be mixed.

### Alternative D: OS-backed process lock

On one Linux host, an advisory file lock held by an open file descriptor naturally releases on process death and cannot be deleted by a stale RAII guard after another process acquires it. Prefer it if generation never needs to coordinate across hosts. Keep the SQLite fenced lease only if holder identity, waiting, and future multi-host operation justify the extra heartbeat/fencing machinery.

## Open Questions

1. Is `replay` meant to reproduce only features available at `as_of`, or to evaluate the new algorithm using features computed later? Both are useful, but they require different cache rules and labels.
2. Should protected/missing-feature ratings count toward exploration maturity while remaining excluded from embedding/facet evidence?
3. Is `feed_priors_v2` an online current-state cache or an input to every run? Historical runs cannot safely mutate or trust one global snapshot.
4. Will churn move to run-scoped `candidate_rankings`, or must `scores` gain actual observation-time provenance?
5. Is the Phase B interleave a guaranteed exposure or merely a Stage B nomination? What score exists to choose it before Phase C?
6. Can any lease-protected stage or backfill exceed 30 minutes? If yes, fencing and an active heartbeat are mandatory; stage-boundary refresh is insufficient.

