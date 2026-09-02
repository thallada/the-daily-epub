# Plan Review: Personalized Ranking, Embeddings, Facets, and Feedback (v4)

**Reviewed:** 2026-08-19  
**Plan:** `docs/plans/2026-08-17-personalized-ranking-and-facets.md` (revision 4)  
**Verdict:** **Not ready for implementation.** Revision 4 resolves the prior review's mathematical, privacy, and lease defects, but two fidelity guarantees are still built on overwrite-in-place tables and therefore cannot hold. The remaining High findings should also be made explicit before implementation because they affect schema shape, provider accounting, process coordination, and rollout behavior.

## Critical findings

### C1. Adding `run_id` and `scored_at` does not preserve score observation history

Section 7.8 correctly identifies that nominal `run_date` is not an observation timestamp, but the proposed migration leaves the existing primary key unchanged: `scores` remains keyed by `(article_id, run_date)` (plan lines 646-661; `migrations/0001_init.sql:58-63`). The current `db::upsert_score` writes through that key (`src/db.rs:386-402`). Consequently, a recuration of the same nominal date still overwrites the earlier observation; it merely replaces it with a row containing a newer `run_id` and `scored_at`.

Example:

1. Article A receives score 2.0 for the August 1 run, observed August 1.
2. On August 19, `generate --date 2026-08-01` scores A as 7.0 and overwrites the same `(A, 2026-08-01)` row.
3. A fidelity replay as of August 5 excludes the surviving row because `scored_at = August 19`, but the valid August 1 score is gone. The churn result has changed because of a future recuration.

This directly contradicts the churn observation-time test in §31.12 and the stated purpose of the v4 fix. `run_id` is provenance only if it participates in an append-only identity.

**Required amendment:** create an append-only score-observation relation keyed at least by `(run_id, article_id)` (or rebuild `scores` with that primary key), then have the churn query select the latest compatible observation with `scored_at <= as_of`. If the legacy date-keyed `scores` table must remain for phased compatibility, treat it as a current-value projection and add a separate authoritative `score_observations` table. Add the destructive case above to §31.12; merely inserting one future score row is not sufficient.

### C2. The broader fidelity contract still queries mutable snapshots as though they were histories

The same issue exists outside `scores`:

- `ratings` is keyed by `(issue_date, article_id)`, and a vote flip overwrites both `vote` and `rated_at` (`migrations/0001_init.sql:100-107`, `src/db.rs:526-543`). Section 13.1 calls the overwrite acceptable for decay (plan line 952), but after a future flip a replay before the flip loses the original vote entirely. Filtering `rated_at <= as_of` cannot recover it.
- `issues` is keyed only by date, `upsert_issue` overwrites `generated_at`, and `replace_issue_articles` deletes and replaces the lineup for that date (`src/db.rs:441-486`). Republishing an old nominal date therefore erases the publication fact and lineup that existed at an earlier `as_of`. The §6.1 predicate `issues.generated_at <= as_of` then excludes the replacement without restoring the original.
- Feed-prior reconstruction joins ratings to the article's current `sources_json`. That field is overwritten on re-ingest (`src/db.rs:270-279`), so future source expansion can change historical feed attribution even if the rating row itself did not change.

Persisted `candidate_rankings` makes an original run's scalar output inspectable, but it does not repair `generate --as-of-date`, preference-state reconstruction, churn reconstruction, or previously-published exclusion. The acceptance criterion that replay is unaffected by future ratings or issues is therefore stronger than the schema can satisfy.

**Required amendment:** choose one of these contracts before implementation:

1. **Recommended:** add append-only `rating_events` and publication/issue-version tables, selecting the latest event at or before `as_of`; preserve the feed-attribution/source snapshot needed by each rating event. Together with C1, this makes fidelity a real temporal query.
2. **Narrower alternative:** remove `generate --as-of-date` and stop promising state reconstruction. Define “fidelity” as inspection/reweighting of already-persisted run snapshots only, explicitly excluding vote state before a later flip, historical publication reconstruction, and historical source attribution.

Whichever contract is chosen, §31.12 needs mutation tests: flip an existing rating in the future, republish the same issue date in the future, rescore the same article/date in the future, and verify that the earlier result is unchanged.

## High findings

### H1. “Daily” provider accounting still omits billing-day identity, non-generate commands, and crash persistence

Sections 7.6 and 24 preload spend from `runs` “for the date” (plan lines 588-589 and 1507 onward), following the existing `spend_for_date(date)` query over `runs.date` (`src/db.rs:688-694`). That date is the nominal issue date, not the provider billing day:

- recuration of August 1 on August 19 charges the August 1 bucket;
- recurations of several historical dates on one real day each receive a fresh “daily” ceiling;
- `features backfill` and standalone `profile rebuild` make provider calls but are not specified to create/finalize `runs` rows, so their spend has no durable bucket;
- reservations and conservative failure estimates live only in process memory until the run is finished. A crash after a request but before `finish_run` leaves zero persisted spend; the kernel correctly releases the file lock, and a retry starts from the understated balance.

Serialization prevents concurrent overspend, but it does not fix omitted or crash-lost spend. This is particularly important because the limits are described as runaway guards.

**Required amendment:** account by actual request/reservation time (with a documented UTC/provider-day boundary), across every provider-using command. Persist the estimate before dispatch and reconcile it afterward, so a crash leaves the conservative reservation rather than zero. A small append-only `provider_usage`/`provider_reservations` table keyed by provider, operation, optional `run_id`, and `reserved_at` is the cleanest design. If the plan intentionally keeps only successful-generate, nominal-date accounting, rename and weaken the guarantee accordingly; it is not a daily provider ceiling.

### H2. The lock scope omits the standalone profile rebuild, which spends budget and races profile versioning

Section 24.1 says “Every mutating command” takes the lock, but lists only `generate`, `features backfill`, and `features prune` (plan lines 1517-1523). `profile rebuild` also calls DeepSeek and writes both `taste_profile_versions` and the `kv` current pointer. Without the same lock, it can overlap generation's weekly rebuild, duplicate spend, choose the same next version, or change the profile pointer while a run is establishing its manifest.

There is also a placement mismatch: §30 says `pipeline.rs` takes the lock, but the current CLI calls `Db::open_and_migrate` before entering `pipeline::generate` (`src/main.rs:105-111`). Thus “before doing any work” does not include startup migration/bootstrap, and different commands are likely to acquire at inconsistent points.

**Required amendment:** define the exact command matrix instead of calling `serve` read-only (the rating endpoint writes `ratings`, though its post-`as_of` writes can safely remain unlocked). At minimum, standalone profile rebuild must take the generation/provider lock. Specify whether the lock is acquired in `main` before command-specific DB work or whether migration/bootstrap has its own short critical section. Add a concurrent generate/profile-rebuild test, including profile-version allocation and provider reservations.

### H3. A manifest becomes `final` before its required stage-completeness data exists

Section 7.4 says the manifest is finalized once preference state and profile selection complete, transactionally with the initial ranking rows (plan lines 386-394). In the target pipeline this is before admission, Stage A, facets, Stage B, and publication. Yet the same section requires every final manifest to contain a complete `stage_completeness_json`, and §7.6 uses that object as authoritative per-metric eligibility.

At that early point the implementation can only write placeholder zeroes and mutate a supposedly final authority later. The plan does not specify those later updates or make them atomic with `runs.status`. A crash or error between the independent writes can therefore leave an `ok`/`degraded` run with stale completeness. In addition, the proposed completeness schema contains `embeddings`, `stage_a`, `facets`, and `stage_b`, but §7.6 says admission metrics require the admission stage to have completed; there is no admission field from which to decide that.

**Required amendment:** separate “ranking definition captured” from “run finalized,” or keep the manifest provisional until `finish_run`. Write final stage completeness and the final `runs.status` in one transaction; candidate rows do not need to be coupled to manifest finalization. Include at least admission, utility/diversification, selection, and publication completion states wherever metrics depend on them. Zero-candidate runs still finalize normally at end of run, so the R4 fix is preserved.

### H4. Guaranteed post-Stage-B insertion and a hard maximum still lack a total precedence rule

Revision 4 says protected auto-includes and the Phase B interleave pick are reinserted after Stage B, both subject to `hard_max` (plan lines 1562 and 2015-2023). It simultaneously calls the interleave a reserved slot and guaranteed exposure. If Stage B returns exactly `hard_max` articles, insertion must either exceed the hard maximum, evict a Stage B pick, or fail to expose the interleave. The same ambiguity appears when protected auto-includes and an interleave compete for the last slot, or when Stage B already selected the intended interleave naturally.

Section 21.2 settles only the case where auto-includes alone exceed the ceiling; it does not define precedence among ordinary auto-includes, protected auto-includes, interleave exposure, and editor picks.

**Required amendment:** specify one deterministic merge order and test it at capacity. For example: deduplicate natural Stage-B selections first; reserve/insert mandatory auto-includes (trimming among auto-includes only if they alone exceed the ceiling); insert the interleave by evicting the lowest-ranked non-mandatory editor pick; then fill remaining slots from Stage B. If auto-includes consume all capacity, explicitly decide whether the interleave is not guaranteed that day or may displace an auto-include. Count `interleave_selected` only for a real final exposure.

## Medium findings

### M1. The malformed-profile bootstrap repairs history but leaves the current version pointer malformed

For malformed `kv[profile_version]`, §7.4b seeds `taste_profile_versions.version = 1` and does `ON CONFLICT DO NOTHING` (plan lines 427-440), but it does not repair the malformed `kv` value. The current `stored_version` treats that value as absent (`src/curate/profile/mod.rs:203-220`), so the next rebuild chooses version 1 again. An append then conflicts with the seeded row; an upsert would overwrite the very history the table is meant to preserve.

**Required amendment:** either repair `kv[profile_version]` to the canonical seeded metadata in the same bootstrap transaction, or allocate the next version from `MAX(taste_profile_versions.version) + 1` and update the pointer transactionally. Extend the malformed-state test from “bootstrap succeeds” to “bootstrap followed by rebuild creates version 2 and preserves version 1.”

## Alternatives worth considering

### One append-only observation layer

Instead of solving scores, ratings, issue publication, and provider spend with separate ad hoc exceptions, introduce a small family of append-only event/observation tables and keep the existing tables as current projections. SQLite is well suited to this volume. It gives `as_of` one consistent meaning and makes crash-safe accounting natural.

### Snapshot-only evaluation

If temporal event storage is judged too much scope, keep per-run `candidate_rankings` and manifests as immutable snapshots and constrain evaluation to those snapshots. This is materially simpler, but the plan must then drop claims that it can reconstruct arbitrary historical state or rerun generation faithfully after mutable inputs have changed.

### Pre-reserved final-selection capacity

Rather than post-selection eviction, calculate Stage B's available capacity after mandatory auto-includes and the active interleave reservation. This makes the prompt ceiling truthful and reduces surprising removal of editor choices, at the cost of giving Stage B a slightly smaller slate on those days.

## Open questions

1. Must fidelity remain stable after an existing vote is flipped and after the same nominal issue date is republished? If yes, append-only rating and publication history is mandatory.
2. Does “daily provider ceiling” mean the provider's real UTC billing day across generation, backfill, profile rebuild, retries, and crashes? If not, what narrower operational guarantee is intended?
3. When `hard_max` is full, which has priority: protected auto-includes, ordinary auto-includes, the Phase B interleave exposure, or Stage B's lowest-ranked choice?
4. Is `manifest_status = final` intended to mean “ranking inputs fixed” or “run outcome complete”? The current plan requires it to mean both at different times.

## Bottom line

The ranking, evidence-gating, privacy wrapper, and single-host lock choices are now implementation-worthy. Implementation should still wait for C1 and C2 because they determine whether migration `0002` needs append-only temporal tables. H1-H4 should be resolved in the same amendment so provider limits, manifest eligibility, locking, and rollout exposure have testable semantics rather than being decided piecemeal during coding.
