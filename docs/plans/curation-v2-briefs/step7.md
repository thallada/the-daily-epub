
---

# YOUR STEP: 7 — Cleanup (plan §21 step 7)

Steps 1–6 have landed. This step removes what the plan retired and updates the implementation
notes. Read `git log -12` and skim every file under `src/` for leftovers.

1. **Dead code**: anything only the old prefilter/scores/select/feed-prior paths used — unused
   functions, types (`LlmScore` if nothing reads it, `ScoredArticle`, `FeedPrior`, `Rating`),
   `kv` keys no longer read, config aliases and `#[serde(alias)]`s introduced as transition aids,
   `// filled in by step N` comments, stale doc comments that still describe 👍/👎, "From the Editor",
   section intros, prefilter gating, or `async-openai`. `cargo clippy --all-targets -- -W dead_code`
   must be clean; do not add `#[allow(dead_code)]`.
2. **Prune paths** (§7.1, §7.4): confirm `features prune` covers `article_embeddings`
   (`embedding_retention_days`) and `candidate_runs` (`telemetry_retention_days`); add
   `article_assessments` older than `telemetry_retention_days` to it. Make `generate` call the prune
   once per run after publishing (best effort, logged).
3. **Docs**: update `docs/plans/2026-08-15-implementation-notes.md`: fix item 5 (hand-rolled
   `reqwest` client, not `async-openai`), add the new providers (Anthropic Messages API facts from
   plan §4.2, Voyage facts from §4.3, each with "verified 2026-09-02"), the new tables, the lock, the
   budget-day rule, and a short "curation v2" section pointing at
   `docs/plans/2026-09-02-personalized-curation-v2.md`. This is the one step allowed to edit `docs/`.
   Do not edit the plan itself.
4. **Consistency pass**: `config.example.toml` matches `Config::default()` key for key (write a test
   that loads the example and compares every `[curation.*]`, `[anthropic]`, `[voyage]`, `[editorial]`
   value to the defaults, since the defaults are the plan's numbers); README CLI section matches
   `daily-epub --help` output for every subcommand (spot check by running the binary).
5. Run `cargo fmt`, `cargo clippy --all-targets`, `cargo test`; report the summary lines.
