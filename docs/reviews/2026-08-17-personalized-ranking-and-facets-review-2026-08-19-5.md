# Plan Review: Personalized Ranking, Embeddings, Facets, and Feedback (v6)

**Reviewed:** 2026-08-19  
**Plan:** `docs/plans/2026-08-17-personalized-ranking-and-facets.md` (revision 6)  
**Verdict:** **Close, but not yet safe to implement verbatim.** Revision 6 correctly repairs the churn query, makes the event layer authoritative in every mode, snapshots final feed attribution, adds attempt-level budget accounting, and closes the migration/publication consistency gaps from the previous review. No architectural rewrite remains. Four High-severity contracts still need to be made implementable: protected-feed classification is not actually based on feed identity, the shadow sub-limit does not reserve production capacity, concurrent ledger reservations have no specified SQLite atomicity mechanism, and the legacy churn fallback cannot implement the new observation-time/latest-value semantics. A final normative consistency pass is also still required.

## Critical

No remaining Critical finding. The ranking, observation-history, manifest-lifecycle, and publication designs are now internally sound in their primary paths.

## High

### H1. The provider-policy type prevents bypasses only after classification; its host matcher can misclassify a private feed

Section 25.1 defines `no_external_ai_feeds` as feed IDs or host substrings “matched exactly like `always_include_feeds`” and then gives `externally_processable` only an `Article` and `CurationConfig` (§25.1, §30). That existing matcher does not inspect the feed URL. In `src/curate/prefilter.rs:136-165`, numeric values match any source feed ID, but string values are searched as case-insensitive substrings of `article.url` and `article.canonical_url`. `SourceRef` contains `feed_id`, title, category, and kind, but no feed URL (`src/types.rs:62-70`).

That is insufficient for the stated threat model. A private Miniflux feed may live at `reader.internal/private.xml` while its entries link to public sites. Configuring `reader.internal` appears valid but does not protect those articles; `externally_processable` constructs the wrapper and the strong type then faithfully sends the protected data. Substring matching also has the wrong security properties for a deny policy: it can match unrelated hosts and does not define exact-host/subdomain behavior.

This is especially easy to miss in a deduplicated cluster whose best article URL is public but one secondary `SourceRef.feed_id` is protected. Numeric IDs can classify that case correctly, but the advertised host form cannot.

**Required amendment:** make the privacy configuration identity-safe before relying on the type-level guarantee. The safest V1 is a typed `no_external_ai_feed_ids: Vec<FeedId>` and a startup error for nonnumeric entries; Miniflux feed IDs are already present on every source and survive deduplication. If host policies must remain, separate them into explicitly named fields and pass actual Miniflux feed URL/site metadata into the policy gate. Parse URLs and compare normalized hosts (with a documented exact-host/subdomain rule), never arbitrary substrings of article URLs.

Add tests where:

1. a protected feed URL host differs from the linked article host;
2. the best source is public but a secondary source feed is protected;
3. a lookalike hostname does not accidentally match an unrelated protected hostname.

The full-run recording mock should use one of these adversarial classifications rather than only a numeric best-feed match.

### H2. A shadow sub-limit inside a shared ceiling does not preserve a production slice

Section 7.6 says all classes share the provider's `max_daily_usd`, while `shadow_max_daily_usd` merely places an additional cap on shadow (§7.6 lines 672-687). Section 24 then claims shadow “can never consume the production slice” (§24 line 1734). Those statements are not equivalent.

With the documented Voyage defaults, the provider ceiling is $0.25 and the shadow sub-limit is $0.20. A shadow invocation that spends $0.20 first leaves only $0.05 for a later production run. Production-first ordering protects calls only within one invocation; it does nothing across runs or across the UTC day. The same issue arises if a standalone non-publication command runs before the timer. The ledger accurately records the depletion, but it does not reserve capacity for the newspaper.

**Required amendment:** choose and state one of these contracts:

- If publication capacity is guaranteed, add a per-provider `production_reserve_daily_usd` (or explicit class allocations) and admit shadow/backfill only when `provider_total + estimate <= provider_max - production_reserve`. Define which operations may consume the reserve and what happens after the publication run completes.
- If the ceiling is only an account-wide runaway guard, remove “production slice” and state plainly that shadow is bounded but may reduce capacity available to later production.

The Phase A promise that shadowing does not affect the paper favors the first option. Add an order-sensitive test: spend shadow first, then prove the configured production reserve is still dispatchable. A mixed-class test inside one run, which §31.11b currently requires, does not cover this defect.

### H3. “Atomic check-and-reserve” needs an explicit SQLite serialization mechanism

The plan correctly requires checking and reserving before each concurrently spawned request, but it never defines the transaction primitive that makes the read-sum-insert sequence atomic. `buffer_unordered` can run sibling reservations concurrently inside one process. The process-wide `flock` serializes commands, not async tasks or SQLite connections within that command.

A naive SQLx transaction is deferred in SQLite: two tasks can both read the same pre-reservation total, both decide they fit, and then contend when writing. Depending on timing, that either admits estimates beyond the ceiling or produces `SQLITE_BUSY` at a point the plan currently treats like ordinary provider degradation. A unique key on `(request_id, attempt)` does not serialize different requests, and the spend index does not enforce a sum constraint.

**Required amendment:** specify one implementation-grade reservation path. Viable choices are:

- a provider-scoped in-process async mutex around a short `BEGIN IMMEDIATE` transaction that re-sums and inserts before commit; or
- a single reservation actor/connection that serializes all check-and-insert operations.

Because every provider-spending command holds the OS lock, an in-process mutex plus `BEGIN IMMEDIATE` is sufficient for the declared single-host deployment. Specify busy-timeout/retry behavior for this short transaction and keep external HTTP work outside it.

Add a barrier-based concurrency test that releases many reservation tasks simultaneously near the ceiling and asserts that the sum of admitted estimates never exceeds either the provider cap or the applicable class cap. Also assert that refusal occurs before the corresponding mock HTTP dispatch.

### H4. The legacy `scores` fallback cannot satisfy the churn rule unless its precedence and lifetime are bounded

Section 7.8 now gives `candidate_rankings` a correct latest-observation query over `runs.started_at`, but then says pre-`0002` dates fall back to `scores` in live/recurate (line 795). `scores` is exactly the mutable, nominal-date-keyed projection the section rejected: it has no observation timestamp and may contain multiple rows for an article across nominal dates. The plan gives no query, merge precedence, or retirement point for that fallback.

A straightforward union of low IDs recreates the v5 defect: one legacy low row can suppress an article despite a newer high `candidate_rankings` observation. Using `scores.run_date` as recency recreates the wrong-time-axis defect. Because new runs continue writing the compatibility projection, “pre-migration score” also cannot be inferred merely from the row's existence.

**Required amendment:** either delete the fallback—the database is effectively at cold start, so this is the cleanest option—or define it as a strictly temporary compatibility bridge:

- capture a migration timestamp/marker;
- consider legacy `scores` only for articles with no `candidate_rankings` observation at all;
- document the nominal-date approximation explicitly;
- disable the fallback after one `recent_rejection_lookback_days` interval from migration, so an unverifiable projection cannot suppress forever.

Do not let a projection row compete with an observed candidate row. Add tests for legacy-low → new-high, legacy-high → new-low, two legacy nominal dates for one article, and fallback expiry. Fidelity should continue to exclude these unverifiable rows.

## Medium

### M1. The normative worklist still contains v5 authorities and one lifecycle contradiction

Revision 6's core sections are clear, but the later implementation instructions still tell an agent to build several superseded forms:

- §23 line 1720 and §30 `src/server.rs` line 2065 say to capture distinct direct-feed IDs. The authoritative event schema requires the completed, versioned `feed_credits_json` after fallback.
- §25.1 line 1810 says protected feed affinity and `W_global` derive from `ratings` and `sources_json`. They must derive from latest `rating_events` and stored feed credits; using current sources reintroduces the temporal bug §7.9 removes.
- §30 `src/db.rs` lines 2007-2008 says to load/derive from ratings joined to current sources/facets. The normative source is latest rating events, with article/facet joins only for compatible local signals.
- §30 `src/db.rs` lists `bootstrap_profile_history()` but omits `bootstrap_observation_history()`.
- §30 `src/publish.rs` line 2069 says publication events commit with `replace_issue_articles`; the actual contract is one transaction containing `upsert_issue`, `replace_issue_articles`, and events.
- §30 `src/main.rs` line 2079 still releases and reacquires the lock and runs only the profile bootstrap, contradicting §24.1's same-file-descriptor rule and the two-bootstrap contract.
- The eligibility table says `explain` accepts only `ranking_fixed`/`final`, while the failure semantics and §31.8 say `explain --run-id` accepts `provisional` too.

These are execution instructions, not historical review tables, and several directly reintroduce bugs v6 says are fixed.

**Required amendment:** update §23, §25.1, §30, and the eligibility table so each has one authority. Add `bootstrap_observation_history` to both the startup sequence and migration concurrency test. Search the normative text for `distinct direct-feed`, `ratings and sources_json`, `load ratings`, `transactionally with replace_issue_articles`, and `release → ... re-acquire`; none should remain except in explicitly labelled historical discussion.

## Low

No separate Low-severity finding. The remaining cleanup belongs in the normative consistency pass above.

## Alternatives

### Prefer typed feed IDs for provider opt-out

Feed IDs are the least ambiguous privacy boundary in this codebase: every clustered source carries one, and the Miniflux account owns the mapping. A separate article-domain rule can be added later for public-domain policy, but it should not masquerade as feed identity.

### Retire legacy churn state at migration

Given the near-empty production history, accepting at most one week without legacy churn suppression is lower risk than maintaining a second, semantically weaker query. If temporary continuity is still desired, snapshot the legacy low set once with an explicit expiry instead of continuing to consult the mutable `scores` projection.

### Serialize ledger admission, parallelize HTTP only

Reservation transactions are tiny. Serialize those locally, commit, then allow HTTP attempts to proceed concurrently. This preserves throughput where it matters while making the monetary invariant easy to state and test.

## Open Questions

1. Is `no_external_ai_feeds` intended to identify Miniflux feed subscriptions, article domains, or both? If both, what are the exact matching semantics for each namespace?
2. Must Phase A/shadow work be incapable of reducing a later production run's available budget, or is `shadow_max_daily_usd` only an additional runaway cap?
3. Is preserving pre-migration churn suppression worth a temporary second semantic path, given the production store has approximately one issue of history?

## Bottom line

The main ranking and temporal architecture is ready. Amend H1-H4 before implementation because each concerns a guarantee the current schema/API description cannot actually enforce: privacy classification, production budget availability, atomic budget admission, and latest-observation churn semantics. M1 should be fixed in the same edit so §30 becomes a reliable implementation checklist rather than a source of regressions.
