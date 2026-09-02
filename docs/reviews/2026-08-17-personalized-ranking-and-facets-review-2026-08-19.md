# Re-review — Personalized Ranking, Embeddings, Facets, and Feedback (v2)

**Plan:** `docs/plans/2026-08-17-personalized-ranking-and-facets.md`  
**Date:** 2026-08-19  
**Scope:** revision 2, after both 2026-08-18 reviews

## Verdict

The revised plan is substantially better: the evidence ladder, presence-aware normalization, union admission, Stage A facet folding, signed nearest neighbors, run-scoped telemetry, as-of semantics, and cluster-cap direction resolve the important defects in both earlier reviews. The core architecture is sound. It is **not ready to execute unchanged**, however. Two issues are blocking: the evaluator filters on a run status that the application never writes, and the promised provider opt-out still leaks opted-out articles through later DeepSeek stages. Before implementation, the plan also needs to repair an impossible Phase A success criterion, persist historical taste-profile versions if replay is meant to use them, and remove cached-only facet preference from admission. These are localized amendments; they do not require redesigning the overall approach.

## Critical

### C1. Evaluation filters on a nonexistent `complete` run status

Evidence:

- §7.5 says evaluation “must ignore rows whose `runs.status != 'complete'`.”
- §27.1 repeats that `evaluate` “must ignore runs whose `status != 'complete'`.”
- The current `RunStatus` vocabulary is `running | ok | degraded | failed | dry_run` (`src/report.rs:19-41`), and `Db::finish_run` stores those exact values.
- Migration `0002` adds no `complete` status and the plan does not change `RunStatus`.

Implemented literally, every run is excluded from evaluation. Phase A can therefore never accumulate its required completed runs, and failed/truncated-run filtering cannot be tested meaningfully.

**Required amendment:** define evaluation eligibility in terms of the real lifecycle. A reasonable default is:

- production outcome metrics: `status IN ('ok', 'degraded')`;
- shadow diagnostics: the same statuses plus the manifest’s shadow marker;
- dry-run diagnostics: included only when explicitly requested;
- always exclude `running` and `failed`.

Use one typed helper/query predicate everywhere rather than copying SQL strings. Add tests covering all five current statuses. If the intent is instead to rename `ok` to `complete`, specify the migration, enum change, compatibility behavior, and existing-row rewrite.

### C2. `no_external_content_feeds` does not actually keep articles away from DeepSeek

Evidence: §25 promises that matching articles are “never sent to Voyage or DeepSeek,” then only specifies “no embedding, no facets, and no Stage A score.” But such an article remains ranked on heuristic signals and can still:

1. enter the Stage B prompt, whose current renderer includes title, feed, and body-derived blurb (`src/curate/select.rs:157`);
2. be selected and have its body sent to the per-article summary call (`src/curate/editorial.rs:119-170`);
3. contribute title/feed/facet metadata to a later weekly profile rebuild (§22).

This breaks the explicit privacy contract for the private/authenticated-feed use case that motivated the setting.

**Required amendment:** centralize provider eligibility and apply it at every provider call, not only embedding and Stage A orchestration. Define whether the restriction covers article body only or all article-derived metadata. Under the strict meaning currently documented:

- omit protected candidates from Stage B and reinsert any mandatory protected articles deterministically afterward, subject to `hard_max`;
- always use local excerpt summaries for protected picks;
- exclude their rating-history metadata from the DeepSeek profile-rebuild prompt;
- assert with a recording mock that no Voyage or DeepSeek request contains any protected article field.

If titles/metadata may be sent while bodies may not, rename and document the policy accordingly; the current “never sent” wording is broader.

## High

### H1. Phase A requires a counterfactual upvote that admission-only shadowing cannot observe

Evidence: §32 correctly says Phase A shadows admission only while the old selector remains authoritative. Its exit gate nevertheless requires the union to “admit at least one upvoted article per week that the prefilter would have dropped.”

An article dropped by the authoritative prefilter is not shown in the issue, so it cannot receive an upvote. Historical selected/upvoted articles necessarily survived the old funnel on the day they were shown. The first half of the gate—retaining at least 95% of known positives—is observable; the claimed rescued-positive rate is not. This is selection bias, not something another week of shadow data fixes.

**Required amendment:** replace the impossible half of the Phase A gate with an observable measure. Options include:

- weekly operator adjudication of a fixed sample of union-only candidates;
- a small, explicitly bounded interleaving bucket that exposes union-only candidates;
- retrospective labels from an independent source, if one genuinely exists.

Then move measured user upvote yield for rescued candidates to Phase B, after those candidates can actually be exposed. Record impressions/exposure origin so this cohort can be evaluated.

### H2. Replay requires historical profile selection, but the plan stores only profile metadata

Evidence:

- §6.1 requires profile-version selection bounded by `as_of`.
- §6.2 says replay disables prose profile only “if no profile version was effective at `as_of`.”
- §7.4 stores only `profile_version` and `profile_hash` in the run manifest.
- The current implementation overwrites `taste_profile`, `taste_profile_learned`, and `profile_version` singleton keys in `kv` (`src/curate/profile/mod.rs:197-240`).
- Migration `0002` proposes no profile-history table and the manifest does not store profile text.

After the next weekly rebuild, the profile text that was effective for an earlier date is gone. The evaluator cannot select it by `as_of`, and a hash cannot reconstruct it. Silently disabling the profile would also make replay depend on whether an old version happened to survive in `kv`.

**Required amendment:** add immutable profile history, for example:

```sql
CREATE TABLE taste_profile_versions (
    version     INTEGER PRIMARY KEY,
    built_at    TEXT NOT NULL,
    profile_hash TEXT NOT NULL,
    profile_text TEXT NOT NULL
);
CREATE INDEX idx_taste_profiles_built_at ON taste_profile_versions(built_at);
```

Write a new row transactionally whenever the singleton/current pointer changes, select the latest `built_at <= as_of`, and migrate the currently stored profile as the first historical row. If retaining profile text is unwanted, explicitly state that replay always disables the prose profile; do not promise version selection.

### H3. Cached facets create an incumbency-only admission signal

Evidence: §16.4 gives `facet_preference` 0.14 preliminary weight while acknowledging it is absent for new articles and present only for articles previously sent through Stage A. The plan says presence-aware renormalization “handles” the asymmetry.

Presence-aware blending correctly handles outages and genuinely unavailable signals, but it does not make informative missingness fair. Previously admitted recurring articles get an extra positive or negative feature that brand-new articles cannot receive before the same admission cut. Since the 26-hour ingest window overlaps days, this makes prior Stage A admission part of the next day’s ranking and can create self-reinforcing survival. It also makes the admission formula depend on cache history rather than solely on the candidate and declared `as_of` evidence.

**Required amendment:** remove `facet_preference` from the preliminary/admission blend in V1. Use it only in post-Stage-A utility, where all successfully scored candidates have equal opportunity to obtain facets. Promote it into admission only if a later dedicated or deterministic pre-admission facet path provides comparable coverage across the eligible set. Presence-aware normalization should remain for genuine provider/cache failures.

### H4. “Atomic” provider budgeting is underspecified across concurrent processes

Evidence: §24 requires reserve-then-spend accounting to be atomic under concurrency, but §7.6 only persists completed usage on `runs` and preloads same-date spend. An in-process meter can coordinate `buffer_unordered` tasks, but two overlapping `generate` processes can both preload the same balance, reserve locally, publish concurrently, and exceed both provider ceilings.

The same overlap can race issue publication and whole-table feed-prior rebuilds. A systemd timer lowers the probability but does not prevent an operator-triggered rerun from overlapping the scheduled process.

**Required amendment:** choose and specify one model:

- simplest: a database-backed generation lease/mutex, with stale-lease recovery, that permits only one mutating generation process at a time;
- more flexible: a provider reservation ledger updated in an immediate SQLite transaction, plus explicit publication serialization.

Add a two-process/concurrent-connection test. If operational policy guarantees serialization instead, enforce it in the binary rather than relying on convention.

## Medium

### M1. The exploration ramp formula does not reach full strength at `evidence_full`

§17 defines:

```text
exploration_reserve =
  round(exploration_max *
        clamp((W - exploration_floor) / exploration_full, 0, 1))
```

and says `exploration_full = evidence_full = 20`, with `exploration_floor = 15`. At `W = 20`, the reserve is only `8 * 5/20 = 2`; it reaches the configured maximum at `W = 35`. That conflicts with the names and with the evidence ladder’s “full at 20” semantics.

**Recommendation:** either use `(W - exploration_floor) / (evidence_full - exploration_floor)`, or introduce a separate explicit `exploration_ramp_width`/full threshold and document that full exploration begins at 35. Add boundary tests at below-floor, floor, full, and above-full values.

### M2. The diversification algorithm is not single-linkage clustering

§20 calls the method “single-linkage cluster caps,” but its algorithm assigns a candidate to the first existing cluster containing a similar member and never merges two existing clusters bridged by a later candidate.

For A similar to C, B similar to C, and A not similar to B, processing A then B then C leaves two clusters; true single linkage produces one connected component. The current method is deterministic greedy threshold assignment, but its cluster caps and explanations can differ materially from the stated design.

**Recommendation:** either:

- implement actual connected components/union-find over the threshold graph (cheap at 120 candidates), accepting single-linkage chaining; or
- deliberately keep the greedy algorithm, rename it, specify cluster-order semantics, and test bridge cases.

Complete-linkage or leader clustering is also worth considering if chaining entire news cycles into one cluster is undesirable.

### M3. Run-manifest creation conflicts with its required fields

§30 tells `pipeline.rs` to create the manifest “immediately after the run row,” but `run_manifests.rating_evidence_weight` is `NOT NULL` and is only computed when preference state is loaded later in the §5 pipeline. Profile selection and feature availability are also not necessarily final at run creation.

This invites either fake defaults in supposedly authoritative manifests or incremental mutation of a snapshot described as the record needed to interpret a run.

**Recommendation:** distinguish an initial run configuration from a finalized ranking manifest. Either load the bounded profile/rating evidence before inserting the manifest, or allow a clearly defined `running` manifest to be finalized transactionally before candidate rows become evaluable. Evaluation must require finalized manifest state in addition to terminal run status.

### M4. The Stage A example uses an invalid facet enum value

§15.2 defines the scored `format` vocabulary as:

`reported_news | analysis_essay | how_to_technical | first_hand_account | announcement_roundup`.

But §18.1’s canonical response example uses `"postmortem_case_study"`. Under the required tolerant parser, that value is dropped to `None`, so an implementation copied from the example loses precisely the first-hand postmortem signal used throughout the plan’s motivating examples.

**Recommendation:** make the prompt example and all fixtures use the exact enum vocabulary, or add `postmortem_case_study` to the schema and update the claimed cardinality. Add a test that every enum token embedded in prompt examples is accepted by the parser.

## Low

### L1. `article_facets.article_id` lacks the foreign key used by the other feature tables

The proposed `article_facets` schema declares `article_id INTEGER NOT NULL` without `REFERENCES articles(id) ON DELETE CASCADE`. `article_embeddings` has that relationship. Add it so article deletion or future archival cannot leave orphaned facet rows.

### L2. Run mode has two writable sources of truth

§7.4 stores `run_manifests.mode`, while §7.6 also adds `runs.mode`. Without a constraint or one-way derivation they can disagree, undermining the evaluator’s mode filtering. Prefer one authoritative column and expose the other through a join; if both are kept, write them in one transaction and test consistency.

### L3. The lifecycle wording should distinguish “budget-degraded” from “truncated and unusable”

The plan says a Stage A budget trip should exclude the run from metrics, while the current application deliberately marks guardrail trips `degraded` and can still publish a valid fallback issue. Some metrics (provider completion and Stage A accuracy) should exclude such a run, while admission, fallback behavior, issue size, and user ratings remain meaningful.

Use per-stage completeness fields/counts from the report/manifest rather than excluding an otherwise valid run wholesale.

## Nits

- §16.1 says thin hygiene rows make “acceptance criterion 10” testable; in v2 this is acceptance criterion 12.
- The plan alternates between “completed run” as an English phrase and the nonexistent literal status `complete`; use typed status names consistently.
- `source TEXT -- 'stage_a' | 'dedicated'` in `article_facets` should have a `CHECK` constraint if the value is used for cache or provenance decisions.
- `run_manifests.shadow`, `dry_run`, and `mode` should receive the same SQLite `CHECK` treatment already required for candidate flags.

## Alternatives

### Alternative A: Keep facets strictly post-admission in V1

Remove facet preference from preliminary ranking, extract/reuse facets during Stage A, and apply facet preference only to utility. This produces uniform feature opportunity, simplifies Phase A, and preserves facets for explanation, profile rebuilding, and later learning. Prefer this until evaluation justifies a uniform pre-admission facet path.

### Alternative B: Controlled interleaving for counterfactual recall evidence

Reserve one or two issue slots—not merely shortlist slots—for candidates admitted only by the new union, subject to the existing quality floor. Mark their exposure origin and compare their explicit rating rate with baseline picks. Prefer this when the operator accepts a small visible experiment and wants genuine user labels. If not, use blinded operator adjudication during Phase A and postpone user-yield claims to Phase B.

### Alternative C: Central provider-policy wrapper

Represent external-processing permission as a typed policy on each article and require every Voyage/DeepSeek orchestration function to accept only an `ExternallyProcessableArticle` wrapper produced by one central filter. Prefer this over scattered feed checks: it makes accidental Stage B/editorial leakage harder to compile and easier to test.

### Alternative D: Serialize generation rather than building a distributed budget ledger

For this single-reader, single-host service, take a SQLite-backed generation lease before starting a mutating run and release it on terminal completion, with timeout/stale-owner recovery. This is simpler than cross-process provider reservations and also prevents publication and feed-prior races. Prefer the ledger only if overlapping generation is a real requirement.

## Open Questions

1. Which current statuses count as evaluable: `ok` only, `ok + degraded`, and/or `dry_run` for shadow diagnostics?
2. Does `no_external_content_feeds` prohibit body text only, or titles, feed names, facets, and rating-history metadata as well?
3. How will Phase A obtain labels for union-only candidates that the authoritative selector never exposes?
4. Is historical prose-profile fidelity required for replay? If yes, immutable profile text must be stored; if no, replay should always disable that input and say so.
5. Can two `generate` processes overlap in supported operation? If not, should the application reject the second invocation immediately or wait on a lease?
6. Is true single linkage intended despite its chaining behavior, or is the current greedy first-matching-cluster algorithm the desired product rule?

