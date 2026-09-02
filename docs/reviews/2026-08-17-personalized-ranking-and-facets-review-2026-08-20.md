# Plan review: personalized ranking and facets, revision 8

## Verdict

Revision 8 is substantially stronger and most earlier architectural defects are now resolved, but it is not quite implementation-ready. Two remaining issues affect hard guarantees rather than tuning: the durable privacy authority is described inconsistently enough that an implementation can still fail open, and the provider reservation formula is not yet a proven upper bound for batched requests with provider-added tokens. Resolve those before implementation; the remaining medium/low items can be folded into the same amendment pass.

## Critical

### C1. The durable privacy authority is not wired into one atomic, fail-closed contract

The new `article_feed_observations` table is the right data model, but four normative parts of the plan disagree about how it becomes authoritative:

- Section 25.1 says `bootstrap_observation_history()` seeds `article_feed_observations`, while §7.4b defines that bootstrap as seeding only `rating_events` and `publication_events` before writing its durable marker. If implemented from §7.4b, the marker can permanently certify an incomplete privacy bootstrap.
- Section 25.1 says `ProviderPolicy::load` builds the protected set and the wrapper consults it, but §30 specifies `externally_processable(&Article, &CurationConfig)`. That signature has neither the loaded policy nor the durable observation set and invites reimplementation of the v7 current-source check.
- “Article persistence upserts one row per observed source” does not require those writes to be in the same transaction as `articles`/`sources_json`. If the article upsert commits and an observation insert fails, the newly persisted protected article is indistinguishable from a public one to the next policy load.
- The general fallback rule says new external stages are non-fatal, but no rule says what happens when `ProviderPolicy::load` or an observation write fails. Treating an error as an empty protected set would disclose content under exactly the failure mode the type is supposed to prevent.

This is a confidentiality boundary, so ambiguity is itself a blocker. Make one normative contract:

1. Put `article_feed_observations` in the migration/observation-layer work item, and have `bootstrap_observation_history()` seed all three observation authorities plus its marker in one transaction.
2. Persist an article and all feed observations from that ingest in one transaction. A failure rolls back both.
3. Make the only constructor `externally_processable(&ProviderPolicy, &Article)` (or a method on `ProviderPolicy`); it must not accept configuration alone.
4. Define failure as closed: if the policy cannot load or privacy provenance cannot be committed, make no Voyage or DeepSeek calls. The issue may continue through the local-only path, but an empty/default policy must never be substituted.
5. Fix §33 sequencing. Step 0 is called independent, yet durable classification requires the table currently assigned nowhere explicitly and the bootstrap currently placed later. Either move the privacy table/bootstrap into step 0 or split “introduce the wrapper” from “activate provider calls” so no intermediate commit claims the guarantee without its authority.

Add fault-injection tests for failure between article and observation writes, a bootstrap with existing protected `sources_json`, a pre-existing bootstrap marker, and a failed policy query. Each must result in zero external dispatches.

## High

### H1. `bytes + 256` is not yet a strict upper bound on provider-billed input tokens

Section 7.6 correctly rejects `len/4`, but the replacement only bounds caller-visible payload bytes. The plan itself states in §11.1 that Voyage prepends a retrieval instruction server-side for `input_type = "query"`. That text is not in `payload_utf8_bytes`; moreover, a request may contain up to 1,000 inputs, so provider-added framing or instructions can scale per input rather than once per request. DeepSeek chat framing similarly scales with message structure. A fixed `per_request_overhead_tokens = 256` is therefore an assumption, not a demonstrated bound.

The acceptance test is also internally contradictory: it requires settlement never to turn an admitted under-ceiling total into an over-ceiling one, while the next bullet says usage above the reservation merely trips the meter. Tripping after settlement detects that the guarantee failed; it does not preserve the pre-dispatch ceiling.

Define a provider-specific bound over the actual request shape, for example:

```text
payload/body byte bound
+ request framing bound
+ per-message bound * message_count
+ per-input bound * input_count
+ maximum output tokens at the undiscounted price
```

The constants must be justified by provider limits or conservatively replaced with a documented maximum-context reservation. If no stable bound exists, weaken the product contract to “conservative guardrail” instead of “strict ceiling”; do not claim both. Add tiny-input/maximum-batch and many-message tests, with the ledger one reservation below the ceiling, so hidden overhead—not only adversarial article text—is exercised. Also add every bound constant, including `per_request_overhead_tokens`, to §38 and the manifest; the appendix currently claims every number is there, but this one is absent.

## Medium

### M1. Existing cached features conflict with the protected-article semantics

Section 25.1 says a protected article has “no embedding, no facets” and that its rating cannot raise `W_embedding` or `W_facet`. Durable classification can, however, discover protection after an embedding/facet was already cached: the operator may add a feed ID later, or the migration may recover a currently visible protected source after an earlier public run processed the article. Nothing currently says whether those cached rows are deleted, ignored, or remain locally usable.

Choose and specify one behavior. The simplest contract matching the current prose is to filter protected article IDs out of embedding/facet loads and all evidence-weight calculations, without requiring destructive deletion. Add a test that caches both features first, then marks the article protected, and verifies zero provider calls plus no contribution to either learned signal.

### M2. Conservative reservations can accumulate beyond the in-flight set

Section 7.6 says the roughly 4× reservation inflation applies only to at most four in-flight attempts and “never to the day’s accumulated total.” That is false for the deliberately conservative crash/retry behavior: `failed_estimated` rows and process-death `reserved` rows retain their estimates for the rest of the billing day. Several failures can therefore consume substantially more apparent budget than the concurrency limit suggests.

The safe accounting rule should stay, but correct the capacity claim and make the operational consequence visible. Report standing estimated reservations separately, including stale `reserved` rows, and state that repeated ambiguous failures may intentionally halt the provider for the day. Do not add an automatic release timeout unless provider billing semantics can prove the request was not charged.

## Low

### L1. One normative ledger test still uses the superseded class name

Section 31.11b says a run mixing “production and shadow” calls should stop shadow while production continues. The actual closed vocabulary is now `publication | shadow | maintenance`. Rename the test wording so an implementation does not recreate or alias a fourth class. Historical resolution-table uses can remain when explicitly marked superseded.

## Nits

- The plan header and the R8 resolution table are dated 2026-08-19, while this revision is being reviewed on 2026-08-20. Updating the revision date would make the review chain easier to audit.
- Section 29’s example log still prints a single `W=0.0`; use the four evidence weights already required elsewhere so the canonical example does not teach an obsolete field.

## Alternatives

For privacy, the strongest alternative is to make externally processable status a persisted monotone article flag maintained transactionally during ingest. The separate observation table is preferable because it preserves auditability and supports deliberate unprotection by configuration, but only if all provider access goes through a successfully loaded policy and failures close the gate.

For budgets, reserving the provider’s maximum accepted context per request is coarser but easier to prove than maintaining tokenizer/framing constants. It may reduce concurrency near a small ceiling, but it is a sound fallback if provider-specific hidden overhead cannot be bounded from a stable contract.

## Open Questions

1. On a policy-load or privacy-observation write failure, should generation abort entirely, or continue as a local-only degraded issue? The plan should choose; either is safe, while continuing external calls is not.
2. Are cached embeddings/facets for a newly protected article allowed for local ranking, or must protection also remove their influence? The current prose chooses the latter implicitly, but the storage rules do not enforce it.
3. Can each provider’s billed-token definition and hidden framing overhead be bounded from a stable API contract? If not, is a conservative guardrail acceptable in place of the stated strict daily ceiling?
