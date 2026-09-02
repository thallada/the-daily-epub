
---

# YOUR STEP: 6 — Paper telemetry, stats, lock (plan §21 step 6)

Steps 1–5 have landed: the full v2 ranking pipeline (hygiene → embeddings → signals → triage →
admission → deep assessment → utility → cluster-capped shortlist → Claude editor → editorial),
`candidate_runs` telemetry, `explain`, `features`, `ratings`. Read `git log -10`, then
`src/pipeline.rs`, `src/report.rs`, `src/main.rs`, `src/curate/telemetry.rs`, `src/epub/chapters.rs`
and `src/epub/templates/`, `src/types.rs` (`Colophon`), `src/db.rs`, README, `config.example.toml`.

Scope (plan §5 lock, §15.1 Behind the paper + In-this-issue, §15.3 `stats`, §15.4 report/log block,
§16 lock coverage, §19 startup logging, README/config docs):

1. **Behind the paper** (§15.1): a new short chapter after the World Briefing and before the colophon
   (`src/epub/templates/behind.xhtml`, `render_behind_the_paper`) with exactly the content shape in
   the plan: the considered/eligible/triaged/read-closely/shortlisted/selected line; the
   `Admitted via:` line; the `Learned signals:` line (rated-with-embeddings count and knn percentage,
   feed affinity state); `Near misses` — the 10 highest-utility non-selected articles as
   `<title> — <feed> · quality X · fit Y · <stage/reason>`; the `Models:` line with triage/assessment,
   editor/summaries and embeddings models; cost and generation time. Both editions (X4 gets the same
   text, no links). The data comes from the run's `RunReport`/`StageCounts` and the run's
   `candidate_runs` rows (add a `BehindThePaper` struct to `types.rs` filled by the pipeline; keep
   templates pure). Chapter ids stable (`behind`).
2. **In this issue** page: verify each entry shows the `why` line under the summary (step 2 was asked
   to do this; complete it if missing). The colophon keeps its fields and shows per-provider cost
   lines and models (verify; complete if missing).
3. **`stats`** (§15.3): `daily-epub stats [--days 14]` printing issues, articles published, explicit
   ratings by label, ratings per issue, up/down ratio per admitting retriever (`admitted_by[0]` of
   rated picks, from `candidate_runs` joined to `rating_events` via the latest explicit event),
   exploration yield (exploration picks selected / admitted, and how many were rated positively),
   mean issue size, cost per day per provider (from `runs.provider_costs_json`), mean generation time.
   Plain text, one fact per line, no tables wider than 80 columns.
4. **Run report and log block** (§15.4): make sure `StageCounts` has every field listed —
   `eligible`, `embedded`, `triaged`, `admitted`, `admitted_by` (map), `assessed`, `shortlisted`,
   `clusters`, `exploration_admitted`, `exploration_selected`, `verdicts_in_prompt`,
   `rated_with_embeddings`, per-provider usage — and that stage timings include `embed`, `signals`,
   `triage`, `admit`, `assess`, `rank`, `editor`, `summaries`, `brief`. Emit the four-line info block
   from §15.4 once per run (`curation:`, `admission:`, `preference:`, `providers:`). `print_report`
   in `main.rs` prints the same lines.
5. **`src/lock.rs`** (§5): `flock(LOCK_EX | LOCK_NB)` on `<database_path>.lock`, taken in `main`
   for `generate`, `profile rebuild`, `features backfill`, and `backfill-social`; a second invocation
   exits with `generate is already running` (name the command that holds it if cheap, else the
   generic message). `serve`, `explain`, `stats`, `ratings`, `db migrate`, `features prune` do not
   take it. Use `libc`/`rustix` only if already in the dependency tree (check `Cargo.lock`); otherwise
   `std::fs::File` + the `fs2`-free approach via `libc::flock` is acceptable to add as a tiny dep —
   say which in the report. Twenty lines, no table, no TTL.
6. **Docs**: README rewritten where it describes the pipeline, feedback links, costs, prerequisites
   (Anthropic and Voyage keys, dashboard spend limits as the real backstop, the server-side fallback
   note), the CLI (`ratings`, `explain`, `stats`, `features`, `--rescore`, `--skip-embeddings`,
   `--max-articles` as ceiling), and `data/profile.md`. `config.example.toml` carries every key from
   plan §19 with comments. Startup logs resolved models and per-provider enabled state (verify).
7. `systemd/daily-epub-generate.service`: no change needed unless an env var name changed; note the
   two new env vars in the README's systemd section.

Tests (§20 "Lock", "Pipeline" behind-the-paper, stats): two processes/one wins (spawn the binary or
use two `File` handles from separate threads with `try_lock` semantics — no sleeping longer than a
second), a killed holder frees the lock, `serve` does not take it (parse-level or by checking no lock
file is created); the behind-the-paper chapter renders with the counts and near misses on the mocked
pipeline run and is parseable XHTML in both editions (extend `tests/m4_epub.rs`); `stats` runs on a
temp DB with a couple of runs, issues and rating events and prints every line; the info block appears
in the log (capture with `tracing` test subscriber or assert on the formatted strings).
