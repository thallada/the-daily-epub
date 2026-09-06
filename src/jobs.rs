//! Operator jobs (dashboard plan §14): the fixed catalogue the Jobs page can
//! start, the `jobs` table lifecycle shared by the page and `daily-epub job
//! run`, and the systemd-backed [`JobRunner`].
//!
//! A job is a `daily-epub-job@<name>.service` instance. The web server never
//! runs the pipeline in-process: it inserts a `requested` row, asks systemd to
//! start the unit (polkit allows `start` on exactly that unit pattern), and the
//! unit's `job run <name>` claims the row, does the work and finishes it.

use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use jiff::civil::Date;
use sqlx::Row as _;

use crate::db::{Db, fmt_ts};
use crate::web::{JobRunner, UnitStatus};

/// Prefix of every job unit; the polkit rule matches the same pattern.
pub const UNIT_PREFIX: &str = "daily-epub-job@";

/// Marker written by the page's 30-second rule (§14.4).
pub const EXITED_BEFORE_START: &str = "unit exited before the job started; see the log";

// ---------------------------------------------------------------------------
// Catalogue (§14.1)
// ---------------------------------------------------------------------------

/// The jobs an admin can start from the dashboard (§14.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// `generate` (today) or `generate-YYYY-MM-DD`.
    Generate { date: Option<Date> },
    /// `dry-run` → `generate --dry-run`.
    DryRun,
    /// `profile-rebuild` → `profile rebuild`.
    ProfileRebuild,
    /// `features-backfill` → `features backfill --days 30 --yes`.
    FeaturesBackfill,
    /// `backfill-social` → `backfill-social --days 7`.
    BackfillSocial,
    /// `features-prune` → `features prune`.
    FeaturesPrune,
    /// `import-ratings` → process ratings-dashboard URL imports.
    ImportRatings,
}

impl Job {
    /// The catalogue in the order the Jobs page lists it.
    pub const CATALOGUE: [Job; 7] = [
        Job::Generate { date: None },
        Job::DryRun,
        Job::ProfileRebuild,
        Job::FeaturesBackfill,
        Job::BackfillSocial,
        Job::FeaturesPrune,
        Job::ImportRatings,
    ];

    /// `^[a-z0-9-]+$`: the only characters a job (and so a unit instance) name
    /// may contain.
    pub fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    }

    /// Parse a catalogue name or the dated `generate-YYYY-MM-DD` form; anything
    /// else (including `../x`, uppercase, unknown names) is `None`.
    pub fn parse(name: &str) -> Option<Job> {
        if !Self::valid_name(name) {
            return None;
        }
        match name {
            "generate" => Some(Job::Generate { date: None }),
            "dry-run" => Some(Job::DryRun),
            "profile-rebuild" => Some(Job::ProfileRebuild),
            "features-backfill" => Some(Job::FeaturesBackfill),
            "backfill-social" => Some(Job::BackfillSocial),
            "features-prune" => Some(Job::FeaturesPrune),
            "import-ratings" => Some(Job::ImportRatings),
            _ => {
                let date = name.strip_prefix("generate-")?;
                // Exactly `YYYY-MM-DD`; the round trip rejects `2026-9-3`.
                let parsed: Date = date.parse().ok()?;
                (parsed.to_string() == date).then_some(Job::Generate { date: Some(parsed) })
            }
        }
    }

    pub fn name(&self) -> String {
        match self {
            Job::Generate { date: None } => "generate".into(),
            Job::Generate { date: Some(date) } => format!("generate-{date}"),
            Job::DryRun => "dry-run".into(),
            Job::ProfileRebuild => "profile-rebuild".into(),
            Job::FeaturesBackfill => "features-backfill".into(),
            Job::BackfillSocial => "backfill-social".into(),
            Job::FeaturesPrune => "features-prune".into(),
            Job::ImportRatings => "import-ratings".into(),
        }
    }

    /// `daily-epub-job@<name>.service`.
    pub fn unit(&self) -> String {
        unit_for(&self.name())
    }

    pub fn description(&self) -> &'static str {
        match self {
            Job::Generate { date: None } => {
                "Build and publish today's issue, exactly as the morning timer does."
            }
            Job::Generate { date: Some(_) } => {
                "Build and publish the issue of a given date, republishing it if it exists."
            }
            Job::DryRun => "Run the whole pipeline but publish nothing and record no issue.",
            Job::ProfileRebuild => {
                "Regenerate the learned taste adjustments from ratings with the editor model."
            }
            Job::FeaturesBackfill => {
                "Embed rated and recently published articles (30 days) and interests into the cache."
            }
            Job::BackfillSocial => "Re-poll social scores for the last 7 days of entries.",
            Job::FeaturesPrune => {
                "Drop stale embeddings, old candidate telemetry and old assessments per the retention config."
            }
            Job::ImportRatings => "Fetch, embed and rate the URLs queued from the Ratings page.",
        }
    }

    /// The run-lock name `main::lock_holder` uses for the same command, when
    /// the job takes the lock at all.
    pub fn takes_lock(&self) -> Option<&'static str> {
        match self {
            Job::Generate { .. } | Job::DryRun => Some("generate"),
            Job::ProfileRebuild => Some("profile rebuild"),
            Job::FeaturesBackfill => Some("features backfill"),
            Job::BackfillSocial => Some("backfill-social"),
            Job::FeaturesPrune | Job::ImportRatings => None,
        }
    }

    /// Needs a confirmation dialog: `generate` republishes an issue.
    pub fn dangerous(&self) -> bool {
        matches!(self, Job::Generate { .. })
    }
}

/// `daily-epub-job@<name>.service` for a validated job name.
pub fn unit_for(name: &str) -> String {
    format!("{UNIT_PREFIX}{name}.service")
}

// ---------------------------------------------------------------------------
// `jobs` table lifecycle (§14.2, §14.4)
// ---------------------------------------------------------------------------

/// One `jobs` row.
#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: i64,
    pub name: String,
    pub unit: String,
    pub requested_by: Option<i64>,
    /// The requester's username, when the user still exists.
    pub requested_by_name: Option<String>,
    pub requested_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub status: String,
    pub message: Option<String>,
    pub run_id: Option<i64>,
}

impl JobRow {
    pub fn is_active(&self) -> bool {
        matches!(self.status.as_str(), "requested" | "running")
    }
}

const ROW_SELECT: &str =
    "SELECT j.id, j.name, j.unit, j.requested_by, u.username AS requested_by_name,
            j.requested_at, j.started_at, j.finished_at, j.status, j.message, j.run_id
     FROM jobs j LEFT JOIN users u ON u.id = j.requested_by";

fn row_from(row: &sqlx::sqlite::SqliteRow) -> JobRow {
    JobRow {
        id: row.get("id"),
        name: row.get("name"),
        unit: row.get("unit"),
        requested_by: row.get("requested_by"),
        requested_by_name: row.get("requested_by_name"),
        requested_at: row.get("requested_at"),
        started_at: row.get("started_at"),
        finished_at: row.get("finished_at"),
        status: row.get("status"),
        message: row.get("message"),
        run_id: row.get("run_id"),
    }
}

/// One job by id.
pub async fn get(db: &Db, id: i64) -> Result<Option<JobRow>, sqlx::Error> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!("{ROW_SELECT} WHERE j.id = ?")))
        .bind(id)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.as_ref().map(row_from))
}

/// The newest `limit` jobs.
pub async fn list(db: &Db, limit: i64) -> Result<Vec<JobRow>, sqlx::Error> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{ROW_SELECT} ORDER BY j.id DESC LIMIT ?"
    )))
    .bind(limit)
    .fetch_all(db.pool())
    .await?;
    Ok(rows.iter().map(row_from).collect())
}

/// The id of a `requested`/`running` job for this unit, if any (the page
/// refuses a duplicate start, §14.4).
pub async fn active_for_unit(db: &Db, unit: &str) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM jobs WHERE unit = ? AND status IN ('requested', 'running')
         ORDER BY id DESC LIMIT 1",
    )
    .bind(unit)
    .fetch_optional(db.pool())
    .await
}

/// Insert a `requested` row (the dashboard, or `job run` when nobody asked).
pub async fn insert_requested(
    db: &Db,
    job: &Job,
    requested_by: Option<i64>,
    now: Timestamp,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        "INSERT INTO jobs (name, unit, requested_by, requested_at, status)
         VALUES (?, ?, ?, ?, 'requested') RETURNING id",
    )
    .bind(job.name())
    .bind(job.unit())
    .bind(requested_by)
    .bind(fmt_ts(now))
    .fetch_one(db.pool())
    .await?;
    Ok(row.get::<i64, _>("id"))
}

/// `job run`'s first step (§14.2): the newest `requested` row with this name,
/// or a fresh one with `requested_by NULL`, flipped to `running`.
pub async fn claim(db: &Db, job: &Job, now: Timestamp) -> Result<i64, sqlx::Error> {
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM jobs WHERE name = ? AND status = 'requested' ORDER BY id DESC LIMIT 1",
    )
    .bind(job.name())
    .fetch_optional(db.pool())
    .await?;
    let id = match existing {
        Some(id) => id,
        None => insert_requested(db, job, None, now).await?,
    };
    sqlx::query("UPDATE jobs SET status = 'running', started_at = ? WHERE id = ?")
        .bind(fmt_ts(now))
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(id)
}

/// Terminal state of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Failed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Failed => "failed",
        }
    }
}

/// Finish a job: status, `finished_at`, the one-line message and, for a
/// generate job, the run it produced.
pub async fn finish(
    db: &Db,
    id: i64,
    outcome: Outcome,
    message: &str,
    run_id: Option<i64>,
    now: Timestamp,
) -> Result<(), sqlx::Error> {
    let message = message.split_whitespace().collect::<Vec<_>>().join(" ");
    sqlx::query(
        "UPDATE jobs SET status = ?, finished_at = ?, message = ?, run_id = ? WHERE id = ?",
    )
    .bind(outcome.as_str())
    .bind(fmt_ts(now))
    .bind(message)
    .bind(run_id)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// SystemdRunner (§14.4)
// ---------------------------------------------------------------------------

/// Talks to systemd with `systemctl`/`journalctl` (10 s timeout each, stderr
/// in the error). Production runner when `server.jobs_enabled` is true.
#[derive(Debug, Clone)]
pub struct SystemdRunner {
    timeout: Duration,
}

impl Default for SystemdRunner {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
        }
    }
}

const SHOW_PROPERTIES: &str =
    "ActiveState,SubState,Result,ExecMainStatus,ExecMainStartTimestamp,ExecMainExitTimestamp";

impl SystemdRunner {
    async fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let command = format!("{program} {}", args.join(" "));
        let output = tokio::time::timeout(
            self.timeout,
            tokio::process::Command::new(program)
                .args(args)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| format!("{command}: timed out after {:?}", self.timeout))?
        .map_err(|error| format!("{command}: {error}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            Ok(stdout)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "{command}: exit {}: {}",
                output
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".into()),
                stderr.trim()
            ))
        }
    }
}

/// Parse `systemctl show -p …` output (`Key=Value` lines).
pub fn parse_unit_status(text: &str) -> UnitStatus {
    let mut status = UnitStatus::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "ActiveState" => status.active_state = value.to_string(),
            "SubState" => status.sub_state = value.to_string(),
            "Result" => status.result = value.to_string(),
            "ExecMainStatus" => status.exit_status = value.parse().ok(),
            "ExecMainStartTimestamp" => {
                status.started = (!value.is_empty()).then(|| value.to_string())
            }
            "ExecMainExitTimestamp" => {
                status.exited = (!value.is_empty()).then(|| value.to_string())
            }
            _ => {}
        }
    }
    status
}

#[async_trait]
impl JobRunner for SystemdRunner {
    async fn start(&self, unit: &str) -> Result<(), String> {
        self.run("systemctl", &["start", "--no-block", unit])
            .await
            .map(|_| ())
    }

    async fn status(&self, unit: &str) -> Result<UnitStatus, String> {
        self.run("systemctl", &["show", "-p", SHOW_PROPERTIES, unit])
            .await
            .map(|text| parse_unit_status(&text))
    }

    async fn log(&self, unit: &str, lines: usize) -> Result<String, String> {
        let lines = lines.to_string();
        self.run(
            "journalctl",
            &["-u", unit, "-n", &lines, "--no-pager", "-o", "short-iso"],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLKIT_RULES: &str = include_str!("../systemd/50-daily-epub.rules");
    const JOB_UNIT: &str = include_str!("../systemd/daily-epub-job@.service");
    const SERVER_UNIT: &str = include_str!("../systemd/daily-epub.service");
    const POLKIT_UNIT_REGEX: &str = r"/^daily-epub-job@[a-z0-9-]+\.service$/";

    /// The polkit rule's regex, by hand: `^daily-epub-job@[a-z0-9-]+\.service$`.
    fn polkit_regex_matches(unit: &str) -> bool {
        unit.strip_prefix(UNIT_PREFIX)
            .and_then(|rest| rest.strip_suffix(".service"))
            .is_some_and(Job::valid_name)
    }

    #[test]
    fn parse_accepts_the_catalogue_and_the_dated_form() {
        for job in Job::CATALOGUE {
            assert_eq!(Job::parse(&job.name()), Some(job), "{}", job.name());
        }
        assert_eq!(
            Job::parse("generate-2026-09-03"),
            Some(Job::Generate {
                date: Some("2026-09-03".parse().unwrap())
            })
        );
        assert_eq!(
            Job::parse("generate-2026-09-03").unwrap().unit(),
            "daily-epub-job@generate-2026-09-03.service"
        );
        assert_eq!(
            Job::parse("generate").unwrap().takes_lock(),
            Some("generate")
        );
        assert_eq!(Job::parse("features-prune").unwrap().takes_lock(), None);
        assert_eq!(Job::parse("import-ratings"), Some(Job::ImportRatings));
        assert_eq!(Job::ImportRatings.takes_lock(), None);
        assert_eq!(
            Job::ImportRatings.description(),
            "Fetch, embed and rate the URLs queued from the Ratings page."
        );
        assert!(Job::parse("generate-2026-09-03").unwrap().dangerous());
        assert!(!Job::parse("dry-run").unwrap().dangerous());
    }

    #[test]
    fn parse_rejects_traversal_uppercase_and_unknown_names() {
        for name in [
            "../x",
            "Generate",
            "GENERATE",
            "generate ",
            "",
            "backup",
            "generate-2026-9-3",
            "generate-2026-13-01",
            "generate-",
            "generate-2026-09-03.service",
            "dry_run",
            "features-prune;rm",
        ] {
            assert_eq!(Job::parse(name), None, "{name:?}");
        }
    }

    #[test]
    fn polkit_rule_is_present_and_matches_the_unit_names() {
        assert!(
            POLKIT_RULES.contains(POLKIT_UNIT_REGEX),
            "the rule must carry the unit regex verbatim"
        );
        assert!(POLKIT_RULES.contains(r#"action.lookup("verb") == "start""#));
        assert!(POLKIT_RULES.contains(r#"subject.user == "daily-epub""#));
        assert!(POLKIT_RULES.contains("org.freedesktop.systemd1.manage-units"));
        for job in Job::CATALOGUE {
            assert!(polkit_regex_matches(&job.unit()), "{}", job.unit());
        }
        assert!(polkit_regex_matches(
            &Job::parse("generate-2026-09-03").unwrap().unit()
        ));
        for unit in [
            "daily-epub-job@../x.service",
            "daily-epub-job@.service",
            "daily-epub-job@Generate.service",
            "daily-epub.service",
            "daily-epub-generate.service",
            "daily-epub-job@generate.service.d",
        ] {
            assert!(!polkit_regex_matches(unit), "{unit}");
        }
    }

    #[test]
    fn unit_files_carry_the_job_template_and_journal_group() {
        assert!(JOB_UNIT.contains("Description=The Daily EPUB job %i"));
        assert!(JOB_UNIT.contains(
            "ExecStart=/usr/local/bin/daily-epub --config /etc/daily-epub/config.toml job run %i"
        ));
        assert!(JOB_UNIT.contains("TimeoutStartSec=45min"));
        assert!(JOB_UNIT.contains("ProtectSystem=strict"));
        assert!(SERVER_UNIT.contains("SupplementaryGroups=systemd-journal"));
    }

    #[test]
    fn unit_status_parses_systemctl_show_output() {
        let status = parse_unit_status(
            "ActiveState=inactive\nSubState=dead\nResult=exit-code\nExecMainStatus=1\n\
             ExecMainStartTimestamp=Thu 2026-09-03 05:30:01 EDT\nExecMainExitTimestamp=\n",
        );
        assert_eq!(status.active_state, "inactive");
        assert_eq!(status.sub_state, "dead");
        assert_eq!(status.result, "exit-code");
        assert_eq!(status.exit_status, Some(1));
        assert_eq!(
            status.started.as_deref(),
            Some("Thu 2026-09-03 05:30:01 EDT")
        );
        assert_eq!(status.exited, None);
        assert_eq!(parse_unit_status("garbage").active_state, "");
    }

    #[tokio::test]
    async fn claim_reuses_the_requested_row_and_finish_records_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("jobs.db"))
            .await
            .unwrap();
        let now: Timestamp = "2026-09-03T10:00:00Z".parse().unwrap();
        let job = Job::Generate {
            date: Some("2026-09-03".parse().unwrap()),
        };
        let requested = insert_requested(&db, &job, None, now).await.unwrap();
        assert_eq!(
            active_for_unit(&db, &job.unit()).await.unwrap(),
            Some(requested)
        );

        let claimed = claim(&db, &job, now).await.unwrap();
        assert_eq!(
            claimed, requested,
            "the requested row is claimed, not duplicated"
        );
        let row = get(&db, claimed).await.unwrap().unwrap();
        assert_eq!(row.status, "running");
        assert_eq!(row.started_at.as_deref(), Some("2026-09-03T10:00:00Z"));
        assert!(row.is_active());

        let run_id = db.start_run(job_date(&job), now).await.unwrap();
        finish(
            &db,
            claimed,
            Outcome::Ok,
            "curation: done",
            Some(run_id),
            now,
        )
        .await
        .unwrap();
        let row = get(&db, claimed).await.unwrap().unwrap();
        assert_eq!(row.status, "ok");
        assert_eq!(row.run_id, Some(run_id));
        assert_eq!(row.message.as_deref(), Some("curation: done"));
        assert_eq!(row.finished_at.as_deref(), Some("2026-09-03T10:00:00Z"));
        assert_eq!(active_for_unit(&db, &job.unit()).await.unwrap(), None);

        // Nothing requested: `claim` inserts a row with no requester.
        let fresh = claim(&db, &Job::FeaturesPrune, now).await.unwrap();
        let row = get(&db, fresh).await.unwrap().unwrap();
        assert_eq!(row.status, "running");
        assert_eq!(row.requested_by, None);
        assert_eq!(row.name, "features-prune");
        assert_eq!(list(&db, 10).await.unwrap().len(), 2);
        assert_eq!(list(&db, 10).await.unwrap()[0].id, fresh);

        finish(
            &db,
            fresh,
            Outcome::Failed,
            "provider failed\n  after retry",
            None,
            now,
        )
        .await
        .unwrap();
        assert_eq!(
            get(&db, fresh).await.unwrap().unwrap().message.as_deref(),
            Some("provider failed after retry"),
            "job messages are always one line"
        );
    }

    fn job_date(job: &Job) -> Date {
        match job {
            Job::Generate { date: Some(date) } => *date,
            _ => panic!("expected a dated generate"),
        }
    }
}
