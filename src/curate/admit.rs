//! Hygiene and union admission into the deep set (plan §8.1, §11).

use std::collections::{BTreeMap, HashSet};

use jiff::Timestamp;
use jiff::civil::Date;
use sha2::{Digest, Sha256};
use sqlx::Row as _;

use super::{prefilter, telemetry};
use crate::config::{CurationConfig, RankingConfig};
use crate::db::{Db, fmt_ts};
use crate::types::{Article, ArticleId, Candidate};

/// Run hygiene before embeddings and write thin telemetry rows for exclusions.
pub async fn hygiene(
    db: &Db,
    run_id: i64,
    articles: Vec<Article>,
    date: Date,
    config: &CurationConfig,
    now: Timestamp,
) -> anyhow::Result<Vec<Candidate>> {
    let published = db
        .previously_published_ids_before(date)
        .await?
        .into_iter()
        .collect::<HashSet<_>>();
    let since = now - jiff::Span::new().hours(config.recent_rejection_days.max(0) * 24);
    let rows = sqlx::query(
        "SELECT DISTINCT article_id FROM article_assessments
         WHERE stage IN ('triage', 'deep') AND score IS NOT NULL AND score < ?
           AND assessed_at >= ?",
    )
    .bind(config.recent_rejection_floor)
    .bind(fmt_ts(since))
    .fetch_all(db.pool())
    .await?;
    let rejected = rows
        .iter()
        .map(|row| row.get::<i64, _>("article_id"))
        .collect::<HashSet<_>>();

    let mut eligible = Vec::new();
    for article in articles {
        let auto_include = prefilter::is_auto_include(&article, config);
        let reason = if auto_include {
            None
        } else if prefilter::is_blocked(&article, config) {
            Some("blocked")
        } else if published.contains(&article.id) {
            Some("published_before")
        } else if rejected.contains(&article.id) {
            Some("recently_rejected")
        } else {
            None
        };
        if let Some(reason) = reason {
            telemetry::thin_excluded(db, run_id, article.id, reason).await?;
        } else {
            eligible.push(Candidate::new(article, auto_include));
        }
    }
    Ok(eligible)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdmissionSummary {
    pub admitted: usize,
    pub admitted_by: BTreeMap<String, usize>,
    pub exploration_admitted: usize,
}

pub fn admit(
    candidates: &mut [Candidate],
    date: Date,
    ranking: &RankingConfig,
) -> AdmissionSummary {
    for candidate in candidates.iter_mut() {
        candidate.admitted_by.clear();
        candidate.exploration = false;
        if candidate.excluded_reason.as_deref() != Some("not_admitted") {
            candidate.excluded_reason = None;
        }
    }
    let mut admitted = HashSet::new();
    for (index, candidate) in candidates.iter_mut().enumerate() {
        if candidate.excluded_reason.is_none() && candidate.auto_include {
            candidate.admitted_by.push("auto_include".into());
            admitted.insert(index);
        }
    }

    let capacity = |admitted: &HashSet<usize>| ranking.deep_keep.saturating_sub(admitted.len());

    let triage = ranked(candidates, |candidate| {
        candidate
            .assessment
            .triage
            .as_ref()
            .filter(|triage| triage.interest >= 5.0)
            .map(|triage| triage.interest)
    });
    let quota = ranking.quotas.triage.min(capacity(&admitted));
    take_retriever(
        candidates,
        &mut admitted,
        &triage,
        ranking.quotas.triage,
        quota,
        "triage",
    );

    let interest = ranked(candidates, |candidate| {
        semantic_floor(candidate, ranking)
            .then_some(candidate.signals.interest)
            .flatten()
    });
    if !interest.is_empty() {
        let quota = ranking.quotas.interest.min(capacity(&admitted));
        take_retriever(
            candidates,
            &mut admitted,
            &interest,
            ranking.quotas.interest,
            quota,
            "interest",
        );
    }

    let knn = ranked(candidates, |candidate| {
        semantic_floor(candidate, ranking)
            .then_some(candidate.signals.knn.filter(|score| *score > 0.0))
            .flatten()
    });
    if !knn.is_empty() {
        let quota = ranking.quotas.knn.min(capacity(&admitted));
        take_retriever(
            candidates,
            &mut admitted,
            &knn,
            ranking.quotas.knn,
            quota,
            "knn",
        );
    }

    let mut by_blend = ranked(candidates, |candidate| candidate.signals.preliminary);
    let band_end = ((ranking.deep_keep as f64) * 2.5).ceil() as usize;
    let band_start = ranking.deep_keep.min(by_blend.len());
    by_blend.truncate(band_end.min(by_blend.len()));
    let mut exploration = by_blend
        .into_iter()
        .skip(band_start)
        .filter(|index| {
            let candidate = &candidates[*index];
            candidate.article.word_count >= 300
                && !prefilter::looks_like_roundup(&candidate.article.title)
                && candidate
                    .assessment
                    .triage
                    .as_ref()
                    .is_some_and(|triage| triage.interest >= 4.0)
        })
        .collect::<Vec<_>>();
    exploration.sort_by_key(|index| exploration_key(date, candidates[*index].article.id));
    let quota = ranking.exploration_slots.min(capacity(&admitted));
    take_retriever(
        candidates,
        &mut admitted,
        &exploration,
        ranking.exploration_slots,
        quota,
        "exploration",
    );
    for index in &admitted {
        if candidates[*index]
            .admitted_by
            .first()
            .is_some_and(|name| name == "exploration")
        {
            candidates[*index].exploration = true;
        }
    }

    let blend = ranked(candidates, |candidate| candidate.signals.preliminary);
    let quota = capacity(&admitted);
    take_retriever(candidates, &mut admitted, &blend, quota, quota, "blend");

    for (index, candidate) in candidates.iter_mut().enumerate() {
        if admitted.contains(&index) {
            candidate.stage = "admitted".into();
            candidate.excluded_reason = None;
        } else if candidate.excluded_reason.is_none() {
            candidate.stage = if candidate.assessment.triage.is_some() {
                "triaged".into()
            } else {
                "eligible".into()
            };
            candidate.excluded_reason = Some("not_admitted".into());
        }
    }
    let mut summary = AdmissionSummary {
        admitted: admitted.len(),
        exploration_admitted: admitted
            .iter()
            .filter(|index| candidates[**index].exploration)
            .count(),
        ..AdmissionSummary::default()
    };
    for index in admitted {
        if let Some(first) = candidates[index].admitted_by.first() {
            *summary.admitted_by.entry(first.clone()).or_default() += 1;
        }
    }
    summary
}

fn semantic_floor(candidate: &Candidate, ranking: &RankingConfig) -> bool {
    candidate.article.word_count >= ranking.semantic_min_words
        && !prefilter::looks_like_roundup(&candidate.article.title)
        && candidate
            .assessment
            .triage
            .as_ref()
            .is_none_or(|triage| triage.interest >= 3.0)
}

fn ranked(candidates: &[Candidate], signal: impl Fn(&Candidate) -> Option<f64>) -> Vec<usize> {
    let mut values = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.excluded_reason.is_none())
        .filter_map(|(index, candidate)| signal(candidate).map(|value| (index, value)))
        .collect::<Vec<_>>();
    values.sort_by(|(left_index, left), (right_index, right)| {
        right.total_cmp(left).then_with(|| {
            candidates[*left_index]
                .article
                .id
                .cmp(&candidates[*right_index].article.id)
        })
    });
    values.into_iter().map(|(index, _)| index).collect()
}

fn take_retriever(
    candidates: &mut [Candidate],
    admitted: &mut HashSet<usize>,
    ranked: &[usize],
    would_take: usize,
    admit_quota: usize,
    name: &str,
) {
    // Record overlap among this retriever's own top-N.
    for index in ranked.iter().take(would_take) {
        if admitted.contains(index)
            && !candidates[*index]
                .admitted_by
                .iter()
                .any(|value| value == name)
        {
            candidates[*index].admitted_by.push(name.into());
        }
    }
    let mut taken = 0;
    for index in ranked {
        if admitted.contains(index) {
            continue;
        }
        if taken >= admit_quota {
            break;
        }
        candidates[*index].admitted_by.push(name.into());
        admitted.insert(*index);
        taken += 1;
    }
}

fn exploration_key(date: Date, article_id: ArticleId) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(date.to_string().as_bytes());
    hasher.update(article_id.to_string().as_bytes());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CurationConfig, RankingConfig, RankingQuotas};
    use crate::curate::prefilter::tests::article;
    use crate::types::Triage;

    fn candidate(
        id: i64,
        words: i64,
        interest: Option<f64>,
        knn: Option<f64>,
        triage: Option<f64>,
    ) -> Candidate {
        let mut candidate = Candidate::new(article(id, &format!("article {id}"), words), false);
        candidate.signals.interest = interest;
        candidate.signals.knn = knn;
        candidate.signals.preliminary = Some(id as f64);
        candidate.assessment.triage = triage.map(|interest| Triage {
            interest,
            kind: "essay".into(),
            why: "specific".into(),
            model: "mock".into(),
            prompt_version: 1,
            assessed_at: "2026-09-02T05:30:00Z".parse().expect("timestamp"),
        });
        candidate
    }

    #[test]
    fn semantic_retrievers_reject_stubs_and_honor_quotas() {
        let config = RankingConfig {
            deep_keep: 4,
            exploration_slots: 0,
            quotas: RankingQuotas {
                triage: 0,
                interest: 2,
                knn: 1,
            },
            ..RankingConfig::default()
        };
        let mut candidates = vec![
            candidate(1, 60, Some(100.0), Some(100.0), None),
            candidate(2, 600, Some(9.0), None, None),
            candidate(3, 600, Some(8.0), None, None),
            candidate(4, 600, None, Some(0.8), None),
            candidate(5, 600, None, None, None),
        ];
        let summary = admit(
            &mut candidates,
            "2026-09-02".parse().expect("date"),
            &config,
        );
        assert_eq!(summary.admitted_by.get("interest"), Some(&2));
        assert_eq!(summary.admitted_by.get("knn"), Some(&1));
        assert!(
            !candidates[0]
                .admitted_by
                .iter()
                .any(|by| by == "interest" || by == "knn")
        );
        assert_eq!(summary.admitted, 4);
        assert_eq!(summary.admitted_by.get("blend"), Some(&1));
    }

    #[test]
    fn strong_interest_weak_heuristic_reaches_deep_set_and_auto_always_wins() {
        let config = RankingConfig {
            deep_keep: 2,
            exploration_slots: 0,
            quotas: RankingQuotas {
                triage: 0,
                interest: 1,
                knn: 0,
            },
            ..RankingConfig::default()
        };
        let mut candidates = vec![
            candidate(1, 400, Some(9.0), None, None),
            candidate(2, 100, None, None, None),
            candidate(3, 4000, None, None, None),
        ];
        candidates[0].signals.heuristic = Some(0.0);
        candidates[1].auto_include = true;
        let summary = admit(
            &mut candidates,
            "2026-09-02".parse().expect("date"),
            &config,
        );
        assert_eq!(summary.admitted, 2);
        assert_eq!(
            candidates[0].admitted_by.first().map(String::as_str),
            Some("interest")
        );
        assert_eq!(
            candidates[1].admitted_by.first().map(String::as_str),
            Some("auto_include")
        );
    }

    #[test]
    fn inactive_retrievers_release_slots_to_blend() {
        let config = RankingConfig {
            deep_keep: 3,
            exploration_slots: 0,
            ..RankingConfig::default()
        };
        let mut candidates = (1..=5)
            .map(|id| candidate(id, 500, None, None, None))
            .collect::<Vec<_>>();
        let summary = admit(
            &mut candidates,
            "2026-09-02".parse().expect("date"),
            &config,
        );
        assert_eq!(summary.admitted_by.get("blend"), Some(&3));
    }

    #[test]
    fn exploration_is_stable_for_a_date_and_rotates() {
        let config = RankingConfig {
            deep_keep: 4,
            exploration_slots: 2,
            quotas: RankingQuotas {
                triage: 0,
                interest: 0,
                knn: 0,
            },
            ..RankingConfig::default()
        };
        let base = (1..=12)
            .map(|id| candidate(id, 500, None, None, Some(5.0)))
            .collect::<Vec<_>>();
        let mut first = base.clone();
        admit(&mut first, "2026-09-02".parse().expect("date"), &config);
        let ids = |items: &[Candidate]| {
            items
                .iter()
                .filter(|candidate| candidate.exploration)
                .map(|candidate| candidate.article.id)
                .collect::<Vec<_>>()
        };
        let expected = ids(&first);
        let mut again = base.clone();
        admit(&mut again, "2026-09-02".parse().expect("date"), &config);
        assert_eq!(ids(&again), expected);
        let mut next = base;
        admit(&mut next, "2026-09-03".parse().expect("date"), &config);
        assert_ne!(ids(&next), expected);
    }

    #[tokio::test]
    async fn recent_low_triage_is_excluded_but_auto_include_is_spared() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open_and_migrate(&dir.path().join("hygiene.db"))
            .await
            .expect("db");
        sqlx::query(
            "INSERT INTO articles (id, canonical_url, title, first_seen) VALUES
             (1, 'https://example.com/1', 'Rejected', '2026-09-02T00:00:00Z'),
             (2, 'https://example.com/2', 'Auto', '2026-09-02T00:00:00Z')",
        )
        .execute(db.pool())
        .await
        .expect("articles");
        sqlx::query(
            "INSERT INTO article_assessments
             (article_id, stage, model, prompt_version, score, assessed_at) VALUES
             (1, 'triage', 'model', 1, 2.0, '2026-09-02T04:00:00Z'),
             (2, 'triage', 'model', 1, 1.0, '2026-09-02T04:00:00Z')",
        )
        .execute(db.pool())
        .await
        .expect("assessments");
        let run_id = db
            .start_run(
                "2026-09-02".parse().expect("date"),
                "2026-09-02T05:30:00Z".parse().expect("timestamp"),
            )
            .await
            .expect("run");
        let config = CurationConfig {
            always_include_feeds: vec!["99".into()],
            ..CurationConfig::default()
        };
        let normal = article(1, "Rejected", 500);
        let mut auto = article(2, "Auto", 500);
        auto.feed_id = 99;
        let eligible = hygiene(
            &db,
            run_id,
            vec![normal, auto],
            "2026-09-02".parse().expect("date"),
            &config,
            "2026-09-02T05:30:00Z".parse().expect("timestamp"),
        )
        .await
        .expect("hygiene");
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].article.id, 2);
        assert!(eligible[0].auto_include);
        let reason: String = sqlx::query_scalar(
            "SELECT excluded_reason FROM candidate_runs WHERE run_id = ? AND article_id = 1",
        )
        .bind(run_id)
        .fetch_one(db.pool())
        .await
        .expect("thin row");
        assert_eq!(reason, "recently_rejected");
    }

    #[tokio::test]
    async fn provider_rejected_rows_do_not_mark_an_article_recently_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open_and_migrate(&dir.path().join("hygiene.db"))
            .await
            .expect("db");
        sqlx::query(
            "INSERT INTO articles (id, canonical_url, title, first_seen) VALUES
             (1, 'https://example.com/1', 'Refused', '2026-09-02T00:00:00Z')",
        )
        .execute(db.pool())
        .await
        .expect("article");
        sqlx::query(
            "INSERT INTO article_assessments
             (article_id, stage, model, prompt_version, score, fit, kind, rationale, assessed_at)
             VALUES (1, 'triage', 'model', 1, NULL, NULL, 'provider_rejected',
                     'deepseek: Content Exists Risk', '2026-09-02T04:00:00Z'),
                    (1, 'deep', 'model', 1, NULL, NULL, 'provider_rejected',
                     'deepseek: Content Exists Risk', '2026-09-02T04:00:00Z')",
        )
        .execute(db.pool())
        .await
        .expect("rejection rows");
        let run_id = db
            .start_run(
                "2026-09-02".parse().expect("date"),
                "2026-09-02T05:30:00Z".parse().expect("timestamp"),
            )
            .await
            .expect("run");
        let eligible = hygiene(
            &db,
            run_id,
            vec![article(1, "Refused", 500)],
            "2026-09-02".parse().expect("date"),
            &CurationConfig::default(),
            "2026-09-02T05:30:00Z".parse().expect("timestamp"),
        )
        .await
        .expect("hygiene");
        assert_eq!(eligible.len(), 1, "a NULL score is not a low score");
        assert_eq!(eligible[0].article.id, 1);
    }
}
