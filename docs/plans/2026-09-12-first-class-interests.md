# First-class interests: a table, a page, categories, links, and a rating-driven weight

**Date:** 2026-09-12
**Repository:** `thallada/the-daily-epub`
**Status:** implementation plan, ready to execute
**Builds on:** `docs/plans/2026-09-02-personalized-curation-v2.md` (§8 profile and interests, §9 cheap signals, §12 blend and utility), `docs/plans/2026-09-03-web-dashboard.md` (page conventions), `docs/plans/2026-09-07-feed-discovery.md` (the most recent page + job + CLI addition; copy its shapes)

Written for a fresh implementation agent. Facts about this repo were checked against `main` (`e82f7b0`) on 2026-09-12. Nothing here depends on an external service beyond the LLM providers and Voyage that the pipeline already uses.

---

## 1. Goal

Today the reader's ~230 standing interests live in a Scour OPML export (`data/scour-interests.opml`) plus an optional `## Interests` section of `profile.md`. They are parsed on every run, embedded, matched against each day's articles (`signals::interest_matches`), grouped for the system prompt by a hand-written keyword table (`profile/themes.rs`), and — since the last commit — shown to the reader as the **Matches:** line under every article header. They cannot be added from the dashboard, ratings never touch them, and nothing links from an interest to the articles that matched it.

This plan makes interests a first-class record:

1. **A database table is the only source of interests.** The OPML and the profile's `## Interests` section become one-time import inputs.
2. **An Interests dashboard page** lists them with category, weight, and match counts; adds new ones; filters by category; changes a category by hand; deletes.
3. **Categories** are kept (the grouped view the operator likes) and assigned by an LLM in one daily batch, only when uncategorized interests exist, with a manual **Categorize now** button.
4. **Every interest name in the web UI is a link** to the Articles page filtered (and sorted) to the articles that matched it, with a cut-off so unrelated articles never appear. The Articles page gains an interest filter and an interests column.
5. **Ratings nudge interests.** Each explicit rating credits the interests the article matched, scaled by how strongly it matched; the resulting per-interest weight is shown on the Interests page (default sort: highest first) and feeds curation as one bounded signal, so diversity is preserved.
6. **Backfill** is two CLI commands the operator runs once.

## 2. Verified facts

### Interests today

- `profile::parse_interests(opml)` reads `<outline text="…">` names; `parse_profile_str` strips `## Interests` from `profile.md` and returns its lines; `union_interests` dedupes case-insensitively (`src/curate/profile/mod.rs:36–150`). `load_standing_interests(opml, profile)` is called from `pipeline::prepare_features` (`src/pipeline.rs:946`), `embedding::plan_backfill` (`src/curate/embedding.rs:901`), and `profile::load_or_build`/`rebuild` (through `prompt_inputs`). `config.interests_opml` is referenced in 20 places in `src/` (mostly tests that write a temp OPML), `config.example.toml:29`, README (lines 103 and 369), and `docs/runbooks/curation-v2-migration.md`.
- The system prompt's "Standing interests" section groups names with `profile::group_into_themes` (`src/curate/profile/themes.rs`): twelve keyword themes plus "Other standing interests", deterministic, sorted. The profile dashboard page renders the same grouping (`src/web/dashboard/profile.rs:249`, template `dashboard/profile.html:42`).
- `interest_embeddings` (migration 0002) is keyed by the interest **name**; `EmbeddingService::interests(&[String])` returns cached vectors and fetches misses (`src/curate/embedding.rs:743`). A new name is embedded automatically the next time it is passed.
- `signals::interest_matches(article_embeddings, interest_embeddings)` (`src/curate/signals.rs:492`) computes cosine per interest × article, z-scores each interest across the day's embedded articles (std floored at 1e-3; raw top-1 cosine fallback under 30 articles), keeps the top `RECORDED_TOP = 3` per article as `TopInterest { name, z, cos }`, and scores `0.7·z₁ + 0.3·mean(top-3 z)`. `signals.top_interests` is serialized into `candidate_runs.signals_json` (`telemetry::serialize_signals`), copied into `Pick.top_interests` as bare names by the editor (`src/curate/editor.rs:598`), and rendered as the Matches line by `chapters::understanding` → `_understanding.html` (web) and `chapter.xhtml` / `in_this_issue.xhtml` (EPUB). Prompts list interests with `z ≥ 1.5` as "matches" (`triage.rs:135`, `assess.rs:192`, `editor.rs:159`).
- Interest names also appear on: the dashboard article detail (`dashboard/article.html:61`, "Top interests" table), `_signals_table.html:5` (runs and article history), and the Feeds page "Why" badges (`dashboard/feeds.html:14`, from `discovery::why`).
- Daily volume: ~365 entries → a few hundred articles per run; 512-dim `voyage-4-lite` vectors; `embedding_retention_days = 120`, so the embedding cache holds on the order of 30–50 k vectors (≈2 KB each). Scanning the cache per web request is not an option; a per-run cosine pass is milliseconds (§9.1 of the curation plan).

### Ratings and the learned signals

- `rating_events` is append-only; `Db::current_ratings(lookback_days) -> Vec<RatedArticle>` returns the latest explicit verdict per article with `value` (`loved` 1.0, `good` 0.35, `not_for_me` −1.0, `slop` per `feedback.slop_value`) and `event_at` (`src/db.rs:713`). Ratings are written by `web::rate::post` (dashboard and issue pages), `rate::record_explicit` (CLI), and `imports::run`.
- `signals::PreferenceState::load` (`src/curate/signals.rs:238`) builds the run's learned state from current ratings: decayed weights `value × 0.5^(age/half_life)`, kNN over rated embeddings, and **feed/author affinity** as Beta-smoothed rates `(up+1)/(up+down+2)` with a gate `gate(n, feed_floor=15, feed_full=40)`. Every cheap signal is `Option<f64>`, percentile-normalized over the eligible set (`normalize`, `PERCENTILE_SIGNALS`), blended with renormalized weights (`preliminary_blend`; `rank::calculate_utility_for` for the deep set). Absent is never zero. The slop-author factor scales both blends. Diversity is enforced downstream by `rank::shortlist` (cluster threshold 0.85, `per_cluster_cap = 2`, `utility_protected = 10`) and by the editor prompt.
- Signal names are enumerated in: `signals::PERCENTILE_SIGNALS`, `Signals::raw`, `preliminary_blend`'s candidate list, `rank::calculate_utility_for`'s weighted list, `telemetry::serialize_signals` (raw list) and `RENDERED_SIGNALS`, `dashboard::SIGNAL_NAMES`, `config::{PreliminaryWeights, UtilityWeights}`, and `settings::SETTINGS_HELP`. A new signal touches all of them.
- The Ratings dashboard page computes each verdict's feed credit on the fly with a pure function (`ratings::contribution`) rather than storing it. Follow that precedent.

### Dashboard, jobs, CLI, config

- One submodule per page group under `src/web/dashboard/`, each with `routes()`, merged in `dashboard::router()`; admin gating is applied by the caller. Nav tabs are hard-coded in `src/web/templates/layout.html` (keyed on `page.active_nav`); overview tiles in `dashboard/overview.html`. POST → flash → redirect via `jobs::set_flash`. `Page::is_admin()` is available in every template. Pager partial `dashboard/_pager.html`; client-side row filter `data-table-filter`.
- The Articles list (`src/web/dashboard/articles.rs`) builds `ARTICLE_INNER` (articles ⨝ best entry ⨝ latest `candidate_runs` row ⨝ assessments, with correlated subqueries for rating and publication) and wraps it in `SELECT * FROM (…) x WHERE 1=1 {clauses} ORDER BY {sort}`. Filters are allow-listed; sorts come from `ARTICLE_SORTS`; `Pager::new(pagination, path, &filters.params())` round-trips them.
- Jobs are a fixed catalogue (`jobs::Job`, `CATALOGUE`, `parse`, `name`, `description`, `takes_lock`, `dangerous`) run as `daily-epub-job@<name>.service` and dispatched in `main::run_job`. The profile page starts `profile-rebuild` with a plain form posting to `/dashboard/jobs/profile-rebuild`. The only daily entry point is the `daily-epub-generate.timer` (05:30 America/New_York).
- `pipeline::build_llms` (`src/pipeline.rs:1133`) builds the taste-profile prompt, the clients, and runs the weekly learned-adjustments rebuild, then rebuilds the clients with the new prompt. An LLM step that must precede the prompt goes here.
- Config sections are `#[serde(deny_unknown_fields, default)]`; an unknown top-level key makes `Config` fail to load. The settings page has a hard-coded `GROUP_ORDER`, `PATH_KEYS`, and `SETTINGS_HELP`, with a test over the section list. Migrations: `sqlx::migrate!("./migrations")`; latest is `0012_article_publication.sql`, so the new file is `0013_interests.sql`. `db.rs` has a migration test asserting a table list (`src/db.rs:1720`).

## 3. Options considered

### 3.1 Where "articles matching interest X" comes from

| Option | How | Verdict |
|---|---|---|
| A. Compute on request | Load the interest vector and every cached article embedding, dot, sort. | **Rejected.** 30–50 k × 2 KB per request; the dashboard is 1–7 ms today and should stay there. |
| B. Query `candidate_runs.signals_json` with `json_each` | The top-3 names are already persisted per run. | **Rejected.** A JSON scan over every telemetry row (hundreds of thousands, pruned at 180 days) per request, no index, and rows vanish with telemetry retention. |
| C. **A junction table written by the signals stage** | `article_interests(article_id, interest_id, cos, z)`: the same top-3 the run already computes, one row each, indexed by interest. The filter is an indexed join. | **Chosen.** ~1 k rows/day, zero extra computation, and the backfill is one pass over cached embeddings. |

### 3.2 How an interest weight is computed and stored

| Option | How | Verdict |
|---|---|---|
| A. Stored counters updated on every rating event | Add `up/down` columns to `interests`; the rating handlers, CLI, and importer bump them; write a backfill migration script. | **Rejected.** Four write paths to keep in sync, decay cannot be stored (it is a function of *now*), and it duplicates the ratings history that already exists. |
| B. **Derived on the fly from current ratings × matches** | One pure function over `current_ratings` and their `article_interests` rows, exactly like feed affinity. The run computes it in `PreferenceState::load`; the Interests page computes it on render. | **Chosen.** No new write path, always current, decay and lookback for free, backfill is "make sure rated articles have match rows". ≤ a few hundred ratings × 3 rows: microseconds. |
| C. Ask the weekly learned-adjustments rebuild to write per-interest weights | An LLM judges the rating history per interest. | **Rejected.** Non-deterministic, weekly, and the numbers would not be explainable. The existing prose rebuild already sees the ratings. |

### 3.3 How the weight enters curation

| Option | How | Verdict |
|---|---|---|
| A. **A new bounded cheap signal, `affinity`** | Per article: match-strength-weighted mean of its matched interests' weights; percentile-normalized; gated on rating count; small configured weight in the preliminary blend and the utility. | **Chosen.** Fits the existing design (absent ≠ zero, renormalized weights, `explain` shows it), and its influence is capped at its weight share, so one runaway interest cannot dominate. Diversity machinery downstream is untouched. |
| B. Multiply each interest's z by its weight before the top-3 is taken | Changes which interests appear as matches. | **Rejected.** Entangles "what does this article match" with "what does he like", and the Matches line would drift with ratings. |
| C. Annotate the prompt's Standing interests with ↑/↓ | Cheap and the LLM would use it. | **Deferred** (§9). Worth adding once the weights have a few weeks of ratings behind them; it is a five-line change on top of this plan. |
| D. A new deep-set admission retriever by affinity | Like the `interest` and `knn` quotas. | **Rejected.** More slots for the same signal; the blend fill already admits high-affinity articles. |

### 3.4 Categories

| Option | Verdict |
|---|---|
| A. **`interests.category TEXT NULL`; the category set is the distinct values** | **Chosen.** No FK, no second page, renaming is an `UPDATE`. |
| B. A separate `interest_categories` table with FK | Rejected: a table with one meaningful column. |
| C. Keep the keyword table in `themes.rs` | Rejected as the source of truth (the user wants DB-tracked interests and LLM categorization), but **kept for the one-time import** so the current grouping survives unchanged. |

### 3.5 When the categorizer runs

| Option | Verdict |
|---|---|
| A. **Inside `generate`, before the prompt is built, only when uncategorized interests exist; plus a catalogue job for the button** | **Chosen.** The morning timer is the only daily trigger that exists; a run without new interests spends nothing. |
| B. Its own systemd timer | Rejected: another unit to install for a call that takes seconds. |
| C. Synchronously in the Add handler | Rejected: an LLM call in a request path, and the user asked for a daily batch. |

## 4. Design decisions (settled)

| Topic | Decision |
|---|---|
| Match rule (one definition everywhere) | An interest **matches** an article when it is among the article's top three interests by z **and** `z ≥ MATCH_MIN_Z = 1.0`. `interest_matches` applies this when it truncates, so `signals.top_interests`, the Matches line, the stored rows, the Articles filter, and the interest weights all agree. The score formula is unchanged (computed before the cut). The prompts keep their stricter `z ≥ 1.5` for "matches interests". |
| Stored rows | `article_interests(article_id, interest_id, cos, z, run_id)`, upserted per eligible article per run (the same article can be eligible on consecutive days; the latest run wins; cosine is stable, z is that day's). Rows are never pruned (≈40 bytes each, ~1 k/day). |
| Link target and filter key | `/dashboard/articles?interest=<name>`: names are unique (case-insensitive), human-readable, and `Pick.top_interests` already carries names, so no id has to travel through `issue_json`. The handler resolves the name to an id; an unknown name yields an empty list, never an error. When `interest` is set and `sort` is absent, the sort defaults to `match` (cosine descending). |
| Who sees links | The dashboard is admin-only, so the Matches line links only for admin viewers (`page.is_admin()`); readers and anonymous visitors see plain text as today. The EPUB never links. |
| Interest weight | Beta-smoothed rate over decayed, strength-scaled credits (§5.2): `(up + 1) / (up + down + 2)`, in (0, 1), 0.5 = no information. Shown with two decimals plus `up`, `down`, and the number of rated matches. |
| Match strength | `s = clamp(z / 3, 0, 1)`: a z of 3 credits the full rating, a bare match (z = 1) a third. |
| Curation signal | `affinity` (§5.3): signed, centred on zero, gated on the number of ratings that credited at least one interest (`affinity_floor = 15`, `affinity_full = 40`, same shape as the feed gate). Default weights: preliminary 0.10 (taken from `interest` 0.35→0.30 and `social` 0.10→0.05), utility 0.05 (taken from `knn` 0.15→0.10). |
| Diversity | Guaranteed by construction: the signal's share of the blend is its configured weight (≤ 10 % / 5 %), percentile normalization caps a favourite interest's articles at percentile 1.0 of that one signal, Beta smoothing means a single *loved* moves a weight from 0.50 to at most 0.67, ratings decay with the 60-day half-life, and `rank::shortlist`'s per-cluster cap and the editor's diversity instructions are unchanged. No new cap is needed. |
| Source of truth | The `interests` table. `interests_opml` is **removed** from config; `profile.md`'s `## Interests` section is still stripped by the parser but no longer read (the importer consumes it once; the profile page says so). |
| Prompt grouping | "Standing interests" groups by `interests.category` (sorted by category name, members sorted case-insensitively); `NULL` renders under the existing label "Other standing interests". `themes.rs` is used only by the importer. |
| Categorizer | One JSON call on the bulk provider over every uncategorized interest, given the existing category list; may create a category only when none fits. Unassigned names stay `NULL` and are retried the next day. Also the catalogue job `interests-categorize` behind the **Categorize now** button. |
| Interests page actions | Add (name + optional category), change category (per-row select + Save), Delete (confirm; cascades match rows, deletes the cached embedding). No rename (delete + add). |
| Backfill | `daily-epub interests import` (OPML + profile section → rows, categorized by the keyword table) and `daily-epub interests backfill` (match rows for every cached embedding). Weights need no backfill: they are derived. |
| Config | No new section. `[curation.ranking]` gains `affinity_floor`, `affinity_full`; the two weight tables gain `affinity`. |

## 5. The numbers

### 5.1 Match rows

In `prepare_features` (`src/pipeline.rs`), after `signals::compute_all` and before the candidates are handed on: for every candidate with non-empty `signals.top_interests`, upsert `(article_id, interest_id, cos, z, run_id)`. One transaction, `INSERT … ON CONFLICT(article_id, interest_id) DO UPDATE SET cos, z, run_id`. Names map to ids through the `interests` rows loaded at the top of the stage (the same rows whose names go to `service.interests`). Best effort: a failure is a report warning, never a failed run.

### 5.2 Interest weight (pure, `interests::rates`)

Inputs: `current_ratings(rating_lookback_days)` and the `article_interests` rows of those articles.

```text
for each current rating r on article a  (value v_r, decay d_r = 0.5^(age_days / half_life_days))
  for each match row (a, i, z):
     s      = clamp(z / 3, 0, 1)
     credit = v_r × d_r × s
     up_i   += max(credit, 0)
     down_i += max(−credit, 0)
     n_i    += 1
weight_i = (up_i + 1) / (up_i + down_i + 2)          # 0.5 when n_i = 0
```

`cleared` verdicts are already excluded by `current_ratings`; `slop` carries `feedback.slop_value` like everywhere else. A rating is *attributable to interests* when it credits at least one interest; that count drives the gate.

### 5.3 The `affinity` signal (pure, in `PreferenceState`)

```text
matched = the article's top_interests with n_i > 0
affinity = Σ s_i × (weight_i − 0.5) / Σ s_i        absent when matched is empty or the gate is 0
gate     = gate(attributable_interest_ratings, affinity_floor, affinity_full)
```

The raw value lives in [−0.5, 0.5]; it is percentile-normalized with the other cheap signals, weighted by `weights.preliminary.affinity × gate` in the blend and `weights.utility.affinity × gate` in the utility. `explain` and `_signals_table.html` show it like any other signal. The once-per-run preference log line gains `affinity gate … (n=…)`.

## 6. Data model — `migrations/0013_interests.sql`

```sql
CREATE TABLE interests (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT NOT NULL COLLATE NOCASE UNIQUE,
    category       TEXT,                      -- NULL until categorized
    created_at     TEXT NOT NULL,
    categorized_at TEXT
);
CREATE INDEX idx_interests_category ON interests(category);

CREATE TABLE article_interests (
    article_id  INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    interest_id INTEGER NOT NULL REFERENCES interests(id) ON DELETE CASCADE,
    cos         REAL NOT NULL,
    z           REAL NOT NULL,
    run_id      INTEGER,                      -- NULL for backfilled rows
    PRIMARY KEY (article_id, interest_id)
);
CREATE INDEX idx_article_interests_interest ON article_interests(interest_id, cos DESC);
```

`interest_embeddings` stays keyed by name (no migration): a deleted interest also deletes its embedding row; a renamed one is a new interest.

## 7. Implementation steps

Sizes are rough line counts including tests. Steps 1–3 must be in order; 4–8 can proceed in parallel after 3; 9 last.

1. **Migration + `src/interests.rs`** (~350). The table above; the db-list test in `db.rs` gains both tables. Module (following `discovery.rs`: data access, pure functions, and the LLM step in one file):
   - `Interest { id, name, category, created_at, categorized_at }`; `list(db) -> Vec<Interest>` (ordered by name), `add(db, name, category, now) -> Result<id, Duplicate>` (trimmed, 1–80 chars, unique case-insensitive), `set_category(db, id, Option<category>, now)`, `delete(db, id)` (also `DELETE FROM interest_embeddings WHERE interest = name`), `names(db) -> Vec<String>`, `grouped(db) -> Vec<(String, Vec<String>)>` (for the prompt), `uncategorized(db)`.
   - `replace_matches(db, run_id, &[(ArticleId, &[TopInterest])], names→ids)`; `matches_for_articles(db, ids) -> Vec<MatchRow { article_id, interest_id, name, cos, z }>`; `match_counts(db) -> HashMap<interest_id, i64>`.
   - `pub fn rates(ratings: &[(ArticleId, value, decay)], rows: &[(ArticleId, interest_id, z)]) -> (HashMap<interest_id, Rate { up, down, n }>, attributable)` per §5.2, and `Rate::weight()`.
   - Tests: uniqueness is case-insensitive; `rates` matches hand-checked numbers (a loved z=3 → up 1.0; a good z=1.5 → up 0.175; a not-for-me z=0.9 → nothing, below the cut); delete cascades; `grouped` puts `NULL` last under "Other standing interests".

2. **Signals** (~250). In `signals.rs`: `MATCH_MIN_Z = 1.0` applied inside `interest_matches` after truncation; `Signals.affinity: Option<f64>`; `"affinity"` in `PERCENTILE_SIGNALS`, `Signals::raw`, `preliminary_blend` (gate `affinity_gate`); `PreferenceState` gains `interest_rates: HashMap<String, Rate>` (keyed by name — `TopInterest` carries names), `affinity_gate`, `attributable_interest_ratings`, `fn affinity(&self, top: &[TopInterest]) -> Option<f64>`, loaded in `load` from `interests::matches_for_articles(rated ids)` and exposed on `PreferenceSummary` and in `log`. `rank::calculate_utility_for` adds `("affinity", configured.affinity, candidate.signals.affinity_gate)`. `telemetry`: `serialize_signals` raw list and `RENDERED_SIGNALS`; `render_explain` prints it with the others. `dashboard::SIGNAL_NAMES` and `_signals_table.html` pick it up automatically once the name is in the list. Config: `RankingConfig { affinity_floor: 15, affinity_full: 40 }`, `PreliminaryWeights { affinity: 0.10, interest: 0.30, social: 0.05 }`, `UtilityWeights { affinity: 0.05, knn: 0.10 }`, `SETTINGS_HELP` lines, `config.example.toml`, README table.
   Tests: the top-3 cut drops a z=0.4 third interest; affinity is absent under the gate and with no rated interests; a candidate matching a 0.8-weight interest outranks an otherwise identical one matching a 0.3-weight interest in the blend; blend weights still renormalize to 1.

3. **Replace the OPML/profile plumbing** (~300, mostly deletions and test edits). `profile::load_standing_interests`, `parse_interests`, `union_interests`, and `prompt_inputs`' OPML argument go; `build`, `load_or_build`, `rebuild`, and `weekly_rebuild_if_due` take `grouped: Vec<(String, Vec<String>)>` from `interests::grouped(db)` instead of `interests: &[String]` + `group_into_themes`. `pipeline::prepare_features` and `embedding::plan_backfill` take names from `interests::names(db)`. `themes.rs` stays but is only referenced by the importer (step 8); `pub use themes::group_into_themes` is removed from the profile module. Remove `Config.interests_opml` (struct, `Default`, `config check` line, the `deny_unknown_fields` implication is a rollout note in §10), `settings::PATH_KEYS`/`SETTINGS_HELP`, `config.example.toml`, README (lines 103, 369), and the runbook mention. Every test that writes a temp OPML instead inserts rows with `interests::add`. The profile page loses the OPML card and the "Extracted `## Interests` lines" preview; in their place one line: "N standing interests in M categories — manage them on the Interests page", and the editor note says the `## Interests` section is ignored.

4. **Match rows in the run** (~80). `prepare_features` writes them per §5.1 (timing folded into the existing `signals` timing; count `interest_matches` on the report counts is optional — skip unless free).

5. **Articles page** (~200). `ArticlesQuery.interest: Option<String>` → `ArticleFilters.interest: Option<(i64, String)>` resolved by name (case-insensitive) in `from_query`'s caller (it needs the db; resolve in `list` before building filters, or make `from_query` async — pick the former). When set: `ARTICLE_INNER` gains `JOIN article_interests ai ON ai.article_id = a.id AND ai.interest_id = ?` (inside the inner query so `idx_article_interests_interest` drives it; the join is a `{interest_join}` placeholder that is empty otherwise) and exposes `ai.cos AS match_cos`; `ARTICLE_SORTS` gains `("match", "x.match_cos DESC, x.id DESC")`, which is the default when `interest` is set and `sort` is absent, and is ignored (falls back to `first_seen`) when it is not. Filter UI: a `<select name="interest">` over all interest names (blank = any), populated from `interests::list`. New column **interests**: for the page's ≤ 50 rows, one query through `matches_for_articles`, grouped per article, rendered as badges linking to `?interest=<name>` (ordered by z desc). `params()` round-trips `interest` and `sort=match`. Test: a seeded match row is found by the filter, an unknown name yields an empty table (200), and the column links.

6. **Interests page** (~450). `src/web/dashboard/interests.rs` + `dashboard/interests.html`; routes `GET /dashboard/interests`, `POST /dashboard/interests` (add), `POST /dashboard/interests/{id}/category`, `POST /dashboard/interests/{id}/delete`. Nav tab **Interests** after Ratings; overview tile "N interests · M uncategorized". Page: add form (name, category select with a blank "let the categorizer decide" option), category filter tabs or select (`?category=<name>` and `?category=uncategorized`), sort select (`weight` default, `name`, `matches`, `added`), the `data-table-filter` row filter, no pager (≤ a few hundred rows). Columns: interest (link → `/dashboard/articles?interest=<name>`), category (link → `?category=`), weight (2 decimals, tabular; `—` when n = 0), up, down, rated matches n, matched articles (count), added (`format_time`), actions (category select + Save; Delete with `confirm`). Weights come from `interests::rates` over `current_ratings(rating_lookback_days)` computed on render; the header says which lookback and half-life it uses and shows the affinity gate state ("affinity gate 0.35 · 22 of 40 attributable ratings"), mirroring the Ratings page. **Categorize now** button posts to `/dashboard/jobs/interests-categorize` (the existing generic job route), shown enabled when `jobs_enabled` and disabled with the manual command otherwise, with the uncategorized count next to it. Flash messages for add/duplicate/deleted. Tests: page renders with weights sorted descending; add rejects a duplicate differing only in case; delete removes match rows and the embedding row; routes are admin-only.

7. **Links everywhere** (~120). `chapters::Understanding.interests` becomes `Vec<InterestRef { name, href }>` (`href = "/dashboard/articles?interest=" + encode_component(name)`) with `interests_line()` for the EPUB templates; `_understanding.html` renders `{% for %}…{% if page.is_admin() %}<a href>…` joined by ` · `. Dashboard article detail "Top interests" table, `_signals_table.html`, and the Feeds page "Why" badges link the same way. Existing tests asserting `Matches: Filesystems · Rust` keep passing for non-admin views; add one admin assertion.

8. **Categorizer + job + CLI** (~350).
   - `interests::categorize(config, db) -> Result<String>`: load uncategorized; if none, return "nothing to categorize"; build `Llms::from_config` with the current taste prompt (as `cmd_profile_rebuild` does) and take `bulk` (fall back to `editor_or_bulk`); one `complete_json` at temperature 0.2 with:
     ```text
     TASK: file each new standing interest under one of the reader's interest categories.
     Existing categories (reuse these names verbatim): <list>
     Create a new category only when none of the existing ones fits; a new category must be broad enough to hold several interests and named like the existing ones (two to five words, sentence case). Every interest gets exactly one category.
     New interests: <one per line>
     Return JSON exactly: {"assignments":[{"interest":"…","category":"…"}]}
     ```
     Validate: interest must be one of the batch (case-insensitive), category trimmed, 1–60 chars; unknown names are ignored; anything unassigned stays `NULL`. Log "categorized N (M new categories: …)". Return that as the job message.
   - `pipeline::build_llms`: after the clients exist, if `interests::uncategorized(db)` is non-empty and a client is available, run `categorize` (warning on error), then rebuild the prompt and the clients exactly as the weekly rebuild path does (the grouping changed). Both the categorizer and the weekly rebuild share that "prompt changed, remake clients" tail.
   - `jobs::Job::InterestsCategorize` (`interests-categorize`, description "File uncategorized interests under categories with the bulk model, creating new ones only when needed.", `takes_lock` None, not dangerous); `main::run_job` dispatch; catalogue tests.
   - CLI `daily-epub interests <import|backfill>` in `main.rs`:
     - `import [--opml PATH] [--profile PATH]` (defaults `data/scour-interests.opml` and `config.profile_path`): parse the OPML with the old `parse_interests` (moved into `interests.rs` as a private helper) and the profile's `## Interests` lines, insert with `add` skipping duplicates, assign each new row's category with `themes::group_into_themes` mapped back per name ("Other standing interests" → `NULL` so the LLM gets a go at them). Print "imported N, skipped M existing, K left for the categorizer".
     - `backfill`: load every `article_embeddings` row compatible with the configured model/dimension in chunks, load interest vectors from `interest_embeddings` (fetching misses through the service if Voyage is enabled), run `interest_matches` over the whole set (z over the whole cached set stands in for the per-day cohort; say so in the docstring), and `INSERT OR IGNORE` the rows with `run_id NULL` so real-run rows win. Print the row count. Takes the run lock (it writes what the run writes).
   - Tests: the categorizer with `MockBackend` assigns two interests, creates one new category, ignores a name not in the batch; `import` is idempotent; `backfill` on a two-article fixture writes the expected rows and leaves an existing run-written row alone.

9. **Docs** (~40). README: the "Web site and dashboard" paragraph gets a sentence on Interests; the Commands section gets `interests import` / `interests backfill`; the configuration table gains the three keys and loses `interests_opml`; the Layout section lists `src/interests.rs`. `docs/ideas.md`: strike the "what interests most match" item.

Expect roughly 2 100 lines including tests.

## 8. Verification

- `cargo test`, `cargo clippy`, `cargo fmt` green.
- Dev seed (`dev/`): `interests import --opml data/scour-interests.opml`, then `interests backfill` (no embeddings in the seed → "0 rows", exits 0), open `/dashboard/interests`: 230 rows, every one categorized except a handful under Uncategorized, weight `—` everywhere; add "Rust macros" → appears uncategorized; **Categorize now** with jobs disabled shows the manual command.
- After deploy, on the server: run the two commands (§10), open the page, click **Categorize now**, watch the job page, refresh: no Uncategorized left, new categories (if any) look sane. Rate one article from the issue page; its matched interests' weights move on the next refresh with the expected magnitude (`0.5 → 0.67` for a full-strength *loved*).
- Next morning's run: `explain` on a selected article shows `affinity` present with a weight, or absent with the gate at 0 if fewer than 15 ratings credited an interest (expected at first; the log line says which). The Articles page filtered by an interest the reader knows well returns articles that obviously belong, sorted by cosine; nothing unrelated appears at the bottom of the first page. Dashboard timing for that page stays single-digit ms (`Server-Timing` header).
- The system prompt on the profile page groups Standing interests by the DB categories with the same members as before the change (byte-stable apart from category names that the LLM added).

## 9. Deferred, with triggers

- **Prompt annotation** of Standing interests with the strongest and weakest weights (e.g. `Rust ↑`, `Coffee ↓`) — once the affinity gate is open and the weights look right for a couple of weeks.
- **Renaming** an interest in place, and merging duplicates — when the operator actually needs it (delete + add loses nothing but the match rows, which the next runs rebuild).
- **Match-row retention** — if `article_interests` ever exceeds a few million rows; today it grows ~1 k/day.
- **Implicit ratings** feeding the weights — already designed for in the curation plan §6.4; nothing here blocks it.
- **Scour** — the Scour feeds in Miniflux keep working as sources; nothing here syncs interests back to Scour.

## 10. Rollout notes

1. Deploy the binary. **Before restarting the service, delete `interests_opml` from `/etc/daily-epub/config.toml`** (the key is gone and `Config` rejects unknown keys). Migrations run on the first command.
2. `daily-epub interests import --opml /var/lib/daily-epub/data/scour-interests.opml` (the profile's `## Interests` section is read from `profile_path`; it has been empty so far). Then delete that section from `profile.md`, or leave it — it is ignored either way.
3. `daily-epub features backfill --rated-only` if any rated article lacks an embedding (it prints the count), then `daily-epub interests backfill`.
4. Open `/dashboard/interests`, press **Categorize now** for the leftovers, and read the weights: they are live from that moment because they are derived from the existing ratings.
5. Optional: `daily-epub generate --dry-run` to see `affinity` in a run before the morning.

## 11. Non-goals (do not add)

- A category editor page, category colours, or ordering beyond alphabetical.
- Per-interest notes, pinning, or manual weight overrides — the weight is derived from ratings only, so the operator changes it by rating.
- Interest suggestions from articles ("you seem to like X") — a separate feature with its own plan if wanted.
- Syncing interests to or from Scour, or OPML export.
- A stored weight column, incremental counters, or a scheduled recompute.
- Configurable `MATCH_MIN_Z` or match strength curve — constants until there is a reason.
- Public (anonymous or reader-role) links from the Matches line; the dashboard is admin-only.
