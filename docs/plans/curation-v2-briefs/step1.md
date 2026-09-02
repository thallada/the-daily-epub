
---

# YOUR STEP: 1 — Feedback and profile (plan §21 step 1)

Scope (plan §6, §7, §8; tests in §20 under Vote, Rating events, Migration, and the prompt):

1. **Migration `migrations/0002_curation_v2.sql`** (§7). Create `rating_events`,
   `article_embeddings`, `interest_embeddings`, `article_assessments`, `candidate_runs` with
   the exact columns/indexes in §7; add `runs.config_json`, `runs.provider_costs_json`,
   `issue_articles.why`. Copy `ratings` rows into `rating_events` per §6.2 (`vote=1 → 'loved',1.0`;
   `vote=-1 → 'not_for_me',-1.0`; `source='migration'`; `kind='explicit'`; `event_at=rated_at`),
   then `DROP TABLE ratings; DROP TABLE feed_priors;`.
   **Orchestrator decision:** do NOT drop `scores` in this step. Stage A scoring
   (`db::upsert_score`, `recently_low_scored_ids`) still writes/reads it until step 4 replaces
   it with `article_assessments`; step 4 will add a `0003` migration dropping it. Do not edit
   `0001_init.sql`. Migration is plain SQL, no Rust bootstrap.
2. **Three-way `Vote`** (§6.1) in `src/types.rs`: `Loved | Good | NotForMe`, `as_str` →
   `"loved" | "good" | "down"`, `parse` also accepts legacy `"up"` → `Loved`, `value(&FeedbackConfig)`
   → configured 1.0 / 0.35 / −1.0. Add `[curation.feedback]` config
   (`loved_value`, `good_value`, `not_for_me_value`, `verdicts_in_prompt = 60`). Remove `Vote::as_i64`,
   `Rating`, `FeedPrior`, and everything that depended on `ratings`/`feed_priors`
   (`db::upsert_rating`, `ratings_with_feed`, `upsert_feed_prior`, `feed_priors`,
   `profile::rebuild_feed_priors`, the feed-prior term in `prefilter.rs` and `ScoredArticle.feed_prior`
   — set the prefilter's feed-prior contribution to nothing; the field can go). HMAC links
   (`src/auth.rs`) must verify for all three votes; message stays `{issue_date}/{article_id}/{vote}`.
3. **Footer + confirmation page** (§6.1): `RatingLinks` in `src/epub/chapters.rs` and
   `src/epub/templates/chapter.xhtml` render three links on one line sized for e-ink:
   `Was this a good pick?   [ Loved it ]   [ Good ]   [ Not for me ]      Read online ↗`.
   X4 edition still renders no links. `server::handle_rating` appends one `rating_events` row
   (`kind='explicit'`, `source='epub'`, label `loved|good|not_for_me`, value from config) and
   returns "Recorded: Loved it — thanks." (etc.) plus the other two links so a mis-tap can be
   corrected. No feed-prior rebuild, no provider call.
4. **`rating_events` access** (§6.2) in `src/db.rs`: `append_rating_event(...)`,
   `db::current_ratings(lookback_days) -> Vec<RatedArticle>` implementing the latest-explicit-event
   rule (`ORDER BY event_at DESC, id DESC`, `cleared` removes the article from the learned set),
   joining `articles`, the best entry's feed title, and the most recent `issue_articles.summary`.
   Move `RatedArticle` to `src/types.rs` with `summary: Option<String>`, `facets: Option<Facets>`
   (define `Facets` per §12.1 now, all `Option`s, unused until step 5), `note: Option<String>`,
   plus `RatingEvent`. Also define `RatedArticle.value: f64` and `label`.
5. **Ratings CLI** (§6.3) in `src/main.rs`: `ratings list [--days 90] [--label loved|good|down|cleared]`,
   `ratings set --article ID|--url URL --label loved|good|down [--note "..."]`,
   `ratings clear --article ID|--url URL`. `set`/`clear` append rows with `source='cli'` and
   `issue_date` from the latest `issue_articles` row for the article if any; `--url` canonicalizes
   with `dedupe`'s canonical URL function then `db::article_id_for_url`. Print something useful.
6. **`data/profile.md`** (§8.2): create the file with the initial content shown in §8.2 verbatim;
   add `profile_path` config (default `data/profile.md`); loader that parses any `## Interests`
   section (one per line, leading `- ` stripped) and unions it case-insensitively with the OPML
   interests, passing everything else through verbatim. Remove `STATED_PREFERENCES` and the
   reader/wants/does-not-want/how-to-judge parts of `PROFILE_PREAMBLE` from code (they now live
   in the file); the editor-in-chief framing paragraph stays in code. If the file is missing,
   warn and continue with the OPML interests only.
7. **System prompt** (§8.4): rebuilt every run in this exact order: framing → profile.md verbatim
   (minus Interests) → standing interests grouped by `profile::themes::group_into_themes` →
   learned adjustments → **recent verdicts**: up to `verdicts_in_prompt` most recent explicit
   ratings, newest first, `LOVED | title | feed | one-line summary` (labels `LOVED`/`GOOD`/`NOT FOR ME`),
   cleared and duplicate articles removed. Byte-identical within a run. `profile_version` bumps only
   on the weekly rebuild, as today. `kv` keeps `ingest_watermark`, `taste_profile`,
   `taste_profile_learned`, `profile_version` (§7.8).
8. **Weekly rebuild** (§8.3): still on the DeepSeek client this step (the Claude editor client
   arrives in step 2). Each rated line carries
   `LOVED | title | feed | summary | facets: format/depth/evidence/technicality/topic_group | note: …`
   (omit empty parts), up to 200 most recent explicit ratings, cleared excluded; the softened
   "strong prior, not a rule" instruction; the required "Diversity check:" bullet.
9. Pipeline: remove the `rebuild_feed_priors` call and every `feed_priors` reference; the paper
   must still build. `handle_rating` and `profile rebuild` must not touch dropped tables.

Tests to add/adapt (§20): Vote parse/serialize incl. `up`→Loved; HMAC for all three; template
renders three links standard / none X4; latest explicit event wins; `cleared` removes from the
learned set; migration on a temp DB through 0001 then 0002 copies old ratings with the right
labels/values and `ratings`/`feed_priors` are gone; CLI `set`/`clear` append `source='cli'` rows
(test the db-level function); profile.md Interests parsing and union; system prompt section order
and verdict block content. Update `tests/m7_server.rs` and `tests/m4_epub.rs` as needed.
