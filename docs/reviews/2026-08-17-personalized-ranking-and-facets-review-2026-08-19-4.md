# Plan Review: Personalized Ranking, Embeddings, Facets, and Feedback (v5)

**Reviewed:** 2026-08-19  
**Plan:** `docs/plans/2026-08-17-personalized-ranking-and-facets.md` (revision 5)  
**Verdict:** **Nearly ready, but not yet safe to implement verbatim.** Revision 5 fixes the prior temporal-history, billing-day, lock-scope, manifest-lifecycle, capacity, and profile-bootstrap blockers. No new architectural rewrite is needed. Three High-severity execution defects remain: the proposed churn query does not implement its stated “latest observation wins” rule, the plan sometimes bypasses its new event authority in live/recurate modes, and the provider ledger cannot enforce the promised shadow budget or conservatively account for retries. Several stale v4 instructions should also be removed so an implementation agent is not given two incompatible authorities.

## Critical

No remaining Critical finding. The append-only observation layer, UTC provider ledger, three-state manifest, expanded lock matrix, and pre-reserved Stage B capacity are sound architectural responses to the previous review.

## High

### H1. The shown churn SQL neither selects the latest observation nor uses observation time for the lookback window

Section 7.8 says “the latest observation per article wins,” but the displayed query returns every qualifying low-score row:

```sql
SELECT cr.article_id
FROM candidate_rankings cr
JOIN runs r ON r.id = cr.run_id
WHERE cr.llm_quality_score IS NOT NULL
  AND r.started_at <= :as_of
  AND cr.run_date >= :since
  AND cr.llm_quality_score < :floor
  AND r.status IN ('ok', 'degraded')
```

There is no grouping, window function, correlated `NOT EXISTS`, or maximum observation selection. If an article scored 2.0 and was later rescored 7.0, the old 2.0 row still satisfies this query and suppresses the article. The new mutation test can therefore pass only if the implementation diverges from the SQL the plan tells it to write.

The lookback also remains anchored to `cr.run_date`, the nominal issue date, even though §7.8 calls `runs.started_at` the “true observation time.” A low score observed today while recurating an old nominal date is immediately outside a seven-day nominal-date window; conversely, a future nominal issue date could enter the window despite when it was observed. Pruning ranking snapshots by nominal date would reproduce the same defect.

**Required amendment:** define whether churn recency means observation recency or nominal issue recency. The v5 rationale strongly implies observation recency, so rank eligible observations with something equivalent to:

```sql
WITH ranked AS (
  SELECT cr.article_id,
         cr.llm_quality_score,
         ROW_NUMBER() OVER (
           PARTITION BY cr.article_id
           ORDER BY r.started_at DESC, cr.run_id DESC
         ) AS rn
  FROM candidate_rankings cr
  JOIN runs r ON r.id = cr.run_id
  WHERE cr.llm_quality_score IS NOT NULL
    AND r.started_at >= :observation_since
    AND r.started_at <= :as_of
    AND r.status IN ('ok', 'degraded')
)
SELECT article_id
FROM ranked
WHERE rn = 1 AND llm_quality_score < :floor;
```

Prune `candidate_rankings` by `runs.started_at`, not `candidate_rankings.run_date`. Add both directions to the regression suite: low→high must not suppress; high→low must suppress. Include a historical-date recuration observed today so the time-axis choice is tested rather than inferred.

### H2. `rating_events` and `publication_events` are declared authoritative, then bypassed in live/recurate modes

Section 6.1 correctly says temporal reads always use the event tables. Section 7.9 then says “`live` and `recurate` may read the projections directly.” That is not equivalent to asking the event layer for the latest state now:

- `ratings` is keyed by `(issue_date, article_id)`. If the same article is republished and rated in two issues, reading the projection can count it twice, while “latest event per article” counts it once.
- `issue_articles` contains only the current lineup for each nominal date. If a republish removes an article, a live projection read says it was never published, while `publication_events` correctly says it was published earlier. The article can then re-enter the paper despite the hard “already published” exclusion.
- Maintaining separate projection and event queries gives live and fidelity subtly different product semantics, not merely different time bounds.

There is also an incomplete attribution snapshot. `rating_events.source_feeds_json` stores only distinct **direct** feed IDs, while §7.7 explicitly falls back to the `best_entry_id` feed when no direct feed exists. The event does not store that fallback feed, so a discovery-only rating cannot be reproduced without consulting mutable current article state—the exact dependency the event row was introduced to remove.

**Required amendment:** use the event tables for preference and previously-published reads in **all** modes; live/recurate simply pass `as_of = now`. Keep projections only for serving the current issue/current vote UI. Store the exact local attribution result on each rating event—preferably a versioned `feed_credits_json` map of feed ID to weight, or at minimum the post-fallback attributed feed set—not merely the pre-fallback direct set. Specify `ORDER BY event_at DESC, id DESC` so equal timestamps are deterministic.

Add regressions for:

1. an article removed by same-date republication remains “previously published” in a subsequent live run;
2. an article rated through two issue dates contributes only its latest vote once;
3. a discovery-only rating retains its vote-time fallback feed after current article provenance changes.

### H3. The provider ledger lacks the dimensions and attempt semantics required by §24

The append-only `provider_usage` ledger fixes UTC bucketing, cross-command spend, and crash persistence, but two promised controls are not representable yet.

First, §24 says shadow work draws from `shadow_max_daily_usd` and “can never consume the production slice.” `provider_usage` has provider, operation, and optional `run_id`, but no `budget_class`/`slice`. Production and shadow calls can occur within the same run, so joining through `run_manifests.shadow` cannot classify individual requests. The ledger can enforce one provider-wide ceiling or a shadow ceiling, but not both the stated production and shadow slices.

Second, retries are under-specified. The plan reserves before “the request,” retries network/429/5xx failures, and keeps an estimate when a failed attempt has no usage payload. If one ledger row covers a logical request, a failed possibly-billed attempt followed by a successful retry will usually settle that row to the final attempt's actual usage, erasing the failed attempt's conservative estimate. This violates the rationale in §24 precisely on the retry path most likely to lack usage metadata.

**Required amendment:**

- Add a `budget_class` such as `production | shadow | backfill` (or an equivalent explicit allocation key) and define whether the provider-wide ceiling also caps the sum of all classes. State whether `shadow_max_daily_usd` is inside the production provider maximum or additive to it.
- Reserve and reconcile **per outbound HTTP attempt**, linking attempts with a logical request ID, or reserve the worst-case cost of all allowed attempts and decrement safely as attempts become known not to have been billed. The per-attempt model is easier to audit.
- Define the conservative estimate formula. For DeepSeek it should include input plus the request's maximum possible output tokens at their respective prices, assuming no cache discount unless known; Voyage is input-only.
- Add a test where attempt 1 returns a 5xx with no usage and attempt 2 succeeds: the day total must contain attempt 1's standing estimate plus attempt 2's actual usage. Add a mixed production/shadow run proving each slice and the aggregate ceiling.

## Medium

### M1. The new authorities were not propagated through the normative file-by-file instructions

Several later sections still instruct an implementer to build the v4 design:

- §9.4 says Voyage is “preloaded per date” and to keep DeepSeek budget semantics unchanged.
- §23 says feed priors and flips derive from canonical `ratings`; §25.1 says protected ratings derive from `ratings` and current `sources_json`.
- §24 says meters preload from `runs` by date, despite §7.6 making `provider_usage` authoritative, and says “rather than build a cross-process reservation ledger” immediately after adding one.
- §30's `src/db.rs` list still requests `voyage_spend_for_date` and “finalize [the manifest] transactionally with the first ranking rows.” Its `src/pipeline.rs` entry likewise says to “finalize” after preference loading instead of transition to `ranking_fixed`.
- The `PreferenceState` type comment and `preference.rs` entry still call `ratings` canonical.
- `RunReport`'s listed completeness block still omits admission, utility, diversification, selection, and publication.
- §31.8 says every mid-run failure leaves a provisional manifest, although a failure after the new transition must leave at least `ranking_fixed` (or deliberately finalize a failed outcome).
- Phase A's adjudication paragraph describes the superseded date-keyed table rather than the `adjudication_batches` schema.

These are not cosmetic in an “implementation-grade” plan: §30 is exactly where an implementation agent will derive its worklist.

**Required amendment:** run one terminology/authority pass and replace every stale use with `rating_events`, `publication_events`, `provider_usage`, `ranking_fixed`, and the full completeness schema. Reserve “projection” for explicitly non-temporal UI/compatibility paths. Update the R1/R2/R3 resolution tables where they still describe superseded mechanisms, or label those cells as historical resolutions superseded by R5.

### M2. Observation seeding is coupled to profile bootstrap and does not have an explicit one-time marker

Section 7.4b makes `bootstrap_profile_history()` also seed rating and publication events. Its earlier step says that an absent/empty taste profile does nothing, making it unclear whether event seeding still runs on a database that has issue/rating projections but no profile. These are unrelated migrations and should not share an early-return condition.

“Seed if the event table is empty” is also a state heuristic, not a migration marker. Every command runs the bootstrap after migration, while an already-running `serve` process does not hold the file lock and can append rating events concurrently. At cutover, an emptiness check plus projection copy can race a legitimate first event or make retry behavior ambiguous.

**Required amendment:** split this into `bootstrap_profile_history()` and `bootstrap_observation_history()`. Record completion in a durable bootstrap/migration marker inside the same transaction as seeding, and make the copy idempotent by a stable seed identity. Event seeding must run independently of whether a taste profile exists. Test a projection-only database with no profile, and test restart after a partially completed/rolled-back seed.

### M3. Failure-state manifest semantics remain contradictory

Section 7.4 defines `final` as the state written in the same transaction as `finish_run`, and the current pipeline calls `finish_run` on errors. The eligibility table even permits `explain --run-id` over failed runs with `ranking_fixed` **or final**. But §31.8 requires every run that fails mid-way to retain a provisional manifest.

That cannot hold for failures after the run reached `ranking_fixed`, and it discards useful completeness data if all failed runs are deliberately kept provisional.

**Required amendment:** specify transitions by failure point. A coherent rule would be: failure before preference/profile capture remains `provisional`; failure after it remains `ranking_fixed` or transitions to `final` with terminal completeness in the same status transaction; evaluation always excludes `failed`, while `explain --run-id` accepts all three states with whatever data exists. Add one test for failure before and one after `ranking_fixed`.

### M4. Publication-event authority stops just short of the actual publish boundary

The plan appends `publication_events` transactionally with `replace_issue_articles`, which is necessary, but current execution publishes files first and records the issue afterward (`src/pipeline.rs:513-528`, `557-582`). A crash or SQLite error after the atomic file copy but before the database transaction leaves an issue visible through the publish directory/OPDS without a publication event. Also, current `upsert_issue` and `replace_issue_articles` are separate database transactions; adding events only to the latter can leave the projections half-updated.

**Required amendment:** at minimum, put `issues`, `issue_articles`, and `publication_events` in one database transaction after file publication and define a recovery check for “files published, DB commit missing” on rerun/startup. If strict exposure-time fidelity is not required for that crash window, state that `publication_events` means “successfully published and recorded,” not every instant a file may have been externally visible.

## Low

### L1. The migration-lock release/reacquire introduces an avoidable race

Section 24.1 has mutating commands acquire the file lock for migration, release it, then immediately reacquire it for the command. Another command can win the gap, causing a generation that completed startup successfully to fail before doing work. This is safe but surprising.

For a lock-holding command, retain the same file descriptor after migration/bootstrap and upgrade the guard's diagnostic state once the database is available. Only non-lock-holding commands such as `serve` need to release after the migration section.

### L2. The deferred-options table still says a provider ledger is deferred

Section 35 lists “Fenced SQLite lease or a cross-process provider ledger instead of the file lock” as deferred, but v5 now includes a cross-process provider ledger. The actual deferred choice is a distributed generation lock/fenced multi-host coordinator; rename the row so it does not suggest removing or postponing `provider_usage`.

## Nits

- `source_feeds_json` should have a versioned typed schema and validation just like the other authoritative JSON fields; storing the final credit map makes this natural.
- Add `CHECK (estimated_usd >= 0)` and `CHECK (actual_usd IS NULL OR actual_usd >= 0)` to `provider_usage`, and validate that `billing_day` is derived internally from `reserved_at` rather than independently supplied by callers.
- The Stage B merge rule says editor picks are admitted in model order and excess picks are trimmed by `ordering_score`; choose one ordering rule for malformed over-cap responses.
- §23 says `serve` “mutates nothing that a run reads mid-flight.” It does mutate the event authority; the correct statement is that timestamp-bounded reads make concurrent later events invisible to the run.

## Alternatives

### Always read the observation layer (recommended)

Use `rating_events` and `publication_events` for every ranking/history query, with `as_of = now` for live/recurate. This produces one tested semantic path. The projection tables remain valuable for the current issue page and current vote state, but never decide ranking history.

### Maintain query-equivalent projections

If event scans ever become measurably expensive, introduce purpose-built current projections such as one row per article's latest rating and a durable “ever published” set, updated transactionally from events. Those projections must be defined and tested as exact query-equivalent caches; the existing `ratings` and `issue_articles` shapes are not equivalent.

### Retry budget alternatives

Per-attempt ledger rows are the most auditable solution. Reserving the maximum cost of every possible retry up front is simpler but needlessly blocks budget during transient failures and requires careful release semantics; use it only if per-attempt IDs are judged too invasive.

## Open Questions

1. Does “recent rejection” mean recently **observed** by an LLM or associated with a recent nominal issue date? The v5 prose and event-time rationale imply the former, but the SQL implements the latter.
2. Is `shadow_max_daily_usd` a sub-limit within each provider's overall daily ceiling, or an additive allowance beyond it? Which budget class owns feature collection performed during a shadow run but reusable by production?
3. Should a failed run after `ranking_fixed` be finalized with partial completeness, or remain `ranking_fixed`? Either is workable; “always provisional” is not.
4. Does a publication event represent external file visibility or the later successful database commit? What recovery behavior is expected if those diverge?

## Bottom line

The plan's architecture is now fundamentally sound. Resolve H1-H3 before implementation because they affect the correctness of the churn rule and the schemas for `rating_events` and `provider_usage`. The Medium findings should be amended in the same pass; most are consistency work, but leaving them in an implementation-grade document would cause the code to regress toward mechanisms revision 5 explicitly replaced.
