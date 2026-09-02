# Plan Review: Personalized Ranking, Embeddings, Facets, and Feedback (v7)

**Reviewed:** 2026-08-19  
**Plan:** `docs/plans/2026-08-17-personalized-ranking-and-facets.md` (revision 7)  
**Verdict:** **The ranking architecture is ready, but the plan is not yet safe to execute verbatim.** Revision 7 resolves all four v6 High findings and the prior normative inconsistencies. Three narrower High-severity guarantees remain: provider protection is not durable across article re-ingestion, non-publication work can still consume the new production reserve by being labelled `production`, and the claimed conservative reservation uses an estimator that is not an upper bound. Two migration/test consistency issues should be corrected in the same pass. These amendments are localized; no redesign of the ranking pipeline is needed.

## Critical

No Critical finding.

## High

### H1. Protected-feed classification is correct for one ingest cluster but is not durable across re-ingestion

Section 25.1 now correctly checks every `SourceRef.feed_id` in the current deduplicated cluster. The plan also explicitly recognizes elsewhere that `db::upsert_article` overwrites `sources_json` on every re-ingest (§7.9, §27.2). The current implementation does exactly that in `src/db.rs:266-279`.

Those facts leave a temporal privacy hole. Consider this sequence:

1. Day 1 ingests canonical article A from protected feed 42. A is correctly withheld from providers but is not selected.
2. Day 2's overlapping window sees the same canonical URL only through a public mirror/feed. `upsert_article` replaces A's `sources_json`; feed 42 disappears.
3. The provider-policy constructor sees only the new public `Article.sources`, constructs `ExternallyProcessable`, and sends A.

The type-level gate is again faithfully enforcing an incomplete classification. The same issue affects a later profile rebuild, which reloads current article state. A `rating_events.feed_credits_json` row may preserve old provenance for rated articles, but it does not protect unrated articles and the wrapper does not consult it anyway.

This is not solved by merging historical sources into `articles.sources_json`: §7.7 deliberately uses current provenance for current candidate feed affinity. Privacy provenance and ranking provenance answer different questions.

**Required amendment:** persist durable source membership separately from the current cluster. For example:

```sql
CREATE TABLE article_feed_observations (
    article_id  INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    feed_id     INTEGER NOT NULL,
    first_seen  TEXT NOT NULL,
    last_seen   TEXT NOT NULL,
    PRIMARY KEY (article_id, feed_id)
);
```

Upsert every observed source during article persistence without deleting older observations. Build `ProviderPolicy` from the intersection of this table and the current configured protected IDs, then let the wrapper constructor consult the resulting protected-article set. Removing a feed ID from configuration can deliberately unprotect its articles; merely failing to observe that feed on a later day cannot.

Bootstrap the table from current `sources_json` at migration, state that older already-overwritten provenance is unrecoverable, and require the operator to verify the configured private subscriptions before rollout. Add a two-run regression: ingest A through a protected source, re-ingest the same canonical URL through public sources only, then prove every provider path still rejects A.

### H2. The production reserve is bypassed by classifying reusable shadow work as `production`

The reserve formulas in §7.6 are now correct, but their protection is only as strong as `budget_class`. Immediately after defining the reserve, the plan says embeddings produced during a shadow run are classed `production` because the cache is reusable (line 695). `profile rebuild` is also unconditionally listed as production (line 674), and dry-run classification is not specified.

That reopens the exact cross-invocation failure the reserve was introduced to close:

- Phase A is explicitly a shadow feature-collection phase and primarily performs embeddings.
- Those embedding attempts are labelled `production`, so they may consume the entire provider ceiling, including the reserve.
- A later publication run can then be refused despite the claim that Phase A cannot affect the paper.

Cache reuse may make a later run cheaper, but it does not make an evaluation request publication-critical. A shadow run over a different date, a broad feature collection, or an interrupted partial cache fill can spend the reserve without producing the exact artifacts the 05:30 run needs. Likewise, a standalone profile rebuild or dry run can consume DeepSeek's reserve before Stage A/B even though neither invocation publishes an issue.

**Required amendment:** classify by the purpose of the HTTP attempt, not by whether its output might someday be reusable. Prefer naming the privileged class `publication` to make the invariant explicit:

- provider calls required by an active issue-producing `generate` invocation: `publication`;
- calls made only for a shadow/dry run: `shadow`, including embeddings;
- standalone profile rebuild and feature backfill: `maintenance`/`backfill`, unless the profile rebuild is an in-run prerequisite for the issue currently being produced.

Only the issue-producing class may use `production_reserve_daily_usd`. Thread an explicit execution/budget context into provider orchestration; do not infer it from operation name or cacheability. Add order-sensitive tests for shadow embeddings, a dry run, and standalone profile rebuild before a live generation. Each must leave the reserve dispatchable.

### H3. The “conservative” reservation is based on a token estimate that can underestimate actual input

Section 7.6 calls the reservation conservative but says input tokens use the existing character approximation (lines 661-670). That helper is `text.len().div_ceil(4)` in `src/curate/mod.rs:159-163`, documented as a crude English-prose average. It is not an upper bound. Punctuation-heavy text, code, unusual Unicode, and provider tokenization can all use materially more than one token per four bytes/characters.

`max_output_tokens` safely bounds the output side, but an underestimated input reservation can be admitted just below the ceiling and then settle to an `actual_usd` above the estimate. At that point the provider call already happened and the day's ledger exceeds a ceiling acceptance criterion 22 says is enforced. The atomic transaction prevents races; it cannot repair an underestimated reservation.

**Required amendment:** either make reservation amounts genuine upper bounds or weaken the contract to a best-effort threshold with a stated maximum overshoot. For the strict contract currently promised, reserve input at a tokenizer-independent upper bound over the exact assembled payload (for example, one token per UTF-8 byte, if verified safe for both providers), plus maximum output at output price. Settle downward only from trustworthy usage. A provider-specific tokenizer is also acceptable if it is available locally and versioned with the model, but an English average plus a safety factor is still not a proof.

Add tests using adversarial prompt text and a mock response whose actual input usage exceeds `approx_tokens`. The admitted reservation must already cover that usage; settlement must never turn an under-ceiling admitted total into an over-ceiling total. If a strict upper bound is operationally too conservative, change the wording and acceptance criterion rather than claiming a hard ceiling.

## Medium

### M1. The observation bootstrap assumes an already-running `serve` process is the new dual-writing binary

Section 7.4b says a concurrent live vote is harmless because it appends an event newer than the seeded rows (line 469). That is true only after `serve` has been upgraded. During the `0002` rollout, an already-running old binary writes only the `ratings` projection. A new `generate` or migration command can acquire the new file lock, seed the event table, and commit; an old `serve` process can then accept a vote into `ratings` without appending `rating_events`. The durable bootstrap marker prevents any later repair, so the new event authority permanently misses that vote.

The generation lock cannot solve this because `serve` intentionally does not take it.

**Required amendment:** add an explicit cutover protocol: stop/drain the existing serve unit, install/start the new binary so migration and both bootstraps complete, then reopen the rating endpoint. Alternatively ship a compatibility release that dual-writes after the new tables exist before making events authoritative, but that is unnecessary complexity for this single-host service. Document the brief downtime and test the migration from a projection-only database; do not claim an old concurrent writer is safe.

### M2. Two migration tests still require the deleted legacy churn fallback

Section 7.8 and §18.4 correctly say pre-`0002` `scores` rows never suppress in any mode. Two later requirements say the opposite:

- §31.12: “Legacy pre-`0002` rows ... are excluded from fidelity and used in `recurate`.”
- §31.13: confirm “a v1 `scores` row is still usable by the churn rule.”

An implementation cannot satisfy those tests and the no-fallback contract simultaneously.

**Required amendment:** change both tests to assert that the v1 row remains readable as a compatibility projection but is never consulted by churn. The migration test should prove the old row survives schema migration, while a separate churn test proves it does not suppress in live, recurate, or fidelity modes.

## Low

### L1. The normative `VoyageConfig` shape omits the new reserve field

Section 9.1's Rust struct lists `max_daily_usd` and `embedding_retention_days` but not `production_reserve_daily_usd`, while §38 places that field under `[voyage]`. Add it to the struct and §30's file-by-file list. Also state where the shared `shadow_max_daily_usd` lives if it intentionally applies identically to both providers.

## Nits

- Section 24.0 is a subsection placed after section 24; `24.1`/`24.2` would read more naturally, though this has no implementation impact.
- Historical resolution tables still use `no_external_ai_feeds` in some older-review rows. They are clearly historical, so this is harmless, but adding “superseded by R7-H1” would reduce search noise.

## Alternatives

### Sticky Boolean restriction

Instead of a general source-observation table, add an `external_ai_protected` bit to `articles` and set it monotonically when any protected source is observed. This is smaller, but changing the configured feed list cannot automatically unprotect previously marked articles and loses the audit trail explaining why the bit was set. It is acceptable only if unprotection is intentionally a manual operation.

### Purpose-based budget classes (recommended)

Rename `production` to `publication` and pass a typed `BudgetContext` from the top-level command. This makes misuse difficult: cache code cannot promote itself merely because its output is reusable. The alternative is per-operation allowlists, which are easier to forget when adding a provider call.

### Best-effort budget threshold

If a byte-level upper bound rejects too much useful work, retain the existing estimator but explicitly define the limit as an advisory threshold, reserve with a documented safety factor, trip immediately when settlement exceeds it, and report maximum observed estimation error. This is operationally reasonable, but it is a different guarantee from “ceilings are enforced before dispatch.”

## Open Questions

1. Does an article remain protected after it was ever observed through a protected feed, until the operator removes that feed ID from configuration? The plan's “no field ever leaves” wording implies yes.
2. Which exact invocations may consume the production reserve: only issue-producing generation, or also dry runs, standalone profile rebuilds, and shadow cache warming?
3. Is the daily budget intended as a strict pre-dispatch ceiling or a best-effort runaway threshold? The estimator and acceptance criterion currently answer differently.

## Bottom line

Revision 7 resolves the prior review and leaves the core recommendation design in good shape. Amend H1-H3 before implementation because they affect the two strongest operational guarantees: private content never leaves the host, and shadow/maintenance work cannot exhaust publication capacity. M1 is a deployment-order requirement, while M2 and L1 are quick consistency fixes. After those changes, the plan should be ready to implement.
