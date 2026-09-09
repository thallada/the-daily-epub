//! Utility normalization, leader clustering and diversified shortlisting (§12.2–§12.5).

use std::collections::{HashMap, HashSet};

use crate::config::{RankingConfig, UtilityWeights};
use crate::curate::embedding::dot;
use crate::curate::signals;
use crate::types::{ArticleId, Candidate};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RankSummary {
    pub shortlisted: usize,
    pub clusters: usize,
}

/// Re-normalize cheap signals over the deep set and calculate utility on 0–100,
/// scaled by the slop-author factor (§9.3).
pub fn calculate_utility(candidates: &mut [Candidate], configured: &UtilityWeights) {
    let indices = (0..candidates.len()).collect::<Vec<_>>();
    calculate_utility_for(candidates, &indices, configured);
}

fn calculate_utility_for(
    candidates: &mut [Candidate],
    indices: &[usize],
    configured: &UtilityWeights,
) {
    let mut normalized = indices
        .iter()
        .map(|index| candidates[*index].signals.clone())
        .collect::<Vec<_>>();
    for signals in &mut normalized {
        signals.norm.clear();
        signals.weights.clear();
    }
    let mut signal_refs = normalized.iter_mut().collect::<Vec<_>>();
    signals::normalize(&mut signal_refs);
    for (index, signals) in indices.iter().zip(normalized) {
        candidates[*index].signals.norm = signals.norm;
        candidates[*index].signals.weights.clear();
    }

    for index in indices {
        let candidate = &mut candidates[*index];
        if let Some(triage) = &candidate.assessment.triage {
            candidate
                .signals
                .norm
                .insert("triage".into(), (triage.interest / 10.0).clamp(0.0, 1.0));
        }
        if let Some(deep) = &candidate.assessment.deep {
            candidate
                .signals
                .norm
                .insert("quality".into(), (deep.quality / 10.0).clamp(0.0, 1.0));
            candidate
                .signals
                .norm
                .insert("fit".into(), (deep.fit / 10.0).clamp(0.0, 1.0));
        }
        let weighted = [
            ("quality", configured.quality, 1.0),
            ("fit", configured.fit, 1.0),
            ("knn", configured.knn, candidate.signals.knn_gate),
            ("interest", configured.interest, 1.0),
            ("feed", configured.feed, candidate.signals.feed_gate),
            ("triage", configured.triage, 1.0),
            ("social", configured.social, 1.0),
            ("heuristic", configured.heuristic, 1.0),
        ]
        .into_iter()
        .filter_map(|(name, weight, gate)| {
            let value = candidate.signals.norm.get(name).copied()?;
            let effective = weight * gate;
            (effective > 0.0).then_some((name, effective, value))
        })
        .collect::<Vec<_>>();
        let total = weighted.iter().map(|(_, weight, _)| weight).sum::<f64>();
        if total <= 0.0 {
            candidate.utility = None;
            continue;
        }
        candidate.signals.weights = weighted
            .iter()
            .map(|(name, weight, _)| ((*name).to_string(), weight / total))
            .collect();
        candidate.utility = Some(
            weighted
                .iter()
                .map(|(_, weight, value)| weight / total * value)
                .sum::<f64>()
                * 100.0
                * candidate.signals.slop_factor(),
        );
    }
}

fn ranked_indices(candidates: &[Candidate], indices: &[usize]) -> Vec<usize> {
    let mut sorted = indices.to_vec();
    sorted.sort_by(|left, right| {
        candidates[*right]
            .utility
            .unwrap_or(f64::NEG_INFINITY)
            .total_cmp(&candidates[*left].utility.unwrap_or(f64::NEG_INFINITY))
            .then_with(|| {
                candidates[*left]
                    .article
                    .id
                    .cmp(&candidates[*right].article.id)
            })
    });
    sorted
}

#[derive(Debug)]
struct Cluster {
    id: i64,
    leader: usize,
}

/// Rank the admitted deep set and leave only the diversified shortlist at
/// `stage = shortlisted`. All deep-set articles receive ranks and clusters.
pub fn shortlist(
    candidates: &mut [Candidate],
    embeddings: &HashMap<ArticleId, Vec<f32>>,
    ranking: &RankingConfig,
) -> RankSummary {
    let deep = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| matches!(candidate.stage.as_str(), "admitted" | "assessed"))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    calculate_utility_for(candidates, &deep, &ranking.weights.utility);
    let sorted = ranked_indices(candidates, &deep);
    for (rank, index) in sorted.iter().enumerate() {
        candidates[*index].rank_utility = Some(rank as i64 + 1);
        candidates[*index].cluster = None;
        candidates[*index].cluster_rank = None;
    }

    let mut clusters = Vec::<Cluster>::new();
    let mut members: HashMap<i64, Vec<usize>> = HashMap::new();
    for index in &sorted {
        let assigned = embeddings
            .get(&candidates[*index].article.id)
            .and_then(|vector| {
                clusters.iter().find_map(|cluster| {
                    let leader_id = candidates[cluster.leader].article.id;
                    let leader = embeddings.get(&leader_id)?;
                    dot(vector, leader)
                        .ok()
                        .filter(|cosine| *cosine >= ranking.diversity.cluster_threshold)
                        .map(|_| cluster.id)
                })
            });
        let cluster_id = assigned.unwrap_or_else(|| {
            let id = clusters.len() as i64 + 1;
            clusters.push(Cluster { id, leader: *index });
            id
        });
        let cluster_members = members.entry(cluster_id).or_default();
        cluster_members.push(*index);
        candidates[*index].cluster = Some(cluster_id);
        candidates[*index].cluster_rank = Some(cluster_members.len() as i64);
    }

    let protected = sorted
        .iter()
        .take(ranking.diversity.utility_protected)
        .copied()
        .collect::<HashSet<_>>();
    let mut admitted = HashSet::new();
    let mut admitted_per_cluster = HashMap::<i64, usize>::new();
    let admit = |index: usize, admitted: &mut HashSet<usize>, counts: &mut HashMap<i64, usize>| {
        if admitted.insert(index)
            && let Some(cluster) = candidates[index].cluster
        {
            *counts.entry(cluster).or_default() += 1;
        }
    };

    for index in &sorted {
        if protected.contains(index) || candidates[*index].auto_include {
            admit(*index, &mut admitted, &mut admitted_per_cluster);
        }
    }
    for index in sorted
        .iter()
        .filter(|index| candidates[**index].exploration)
        .take(3)
    {
        if admitted.len() >= ranking.shortlist_keep {
            break;
        }
        admit(*index, &mut admitted, &mut admitted_per_cluster);
    }

    let target = ranking.shortlist_keep.max(admitted.len());
    let mut suppressed_at_base_cap = HashSet::new();
    admit_under_cap(
        candidates,
        &sorted,
        target,
        ranking.diversity.per_cluster_cap,
        &mut admitted,
        &mut admitted_per_cluster,
        Some(&mut suppressed_at_base_cap),
    );
    if admitted.len() < target {
        admit_under_cap(
            candidates,
            &sorted,
            target,
            3,
            &mut admitted,
            &mut admitted_per_cluster,
            None,
        );
    }
    if admitted.len() < target {
        for index in &sorted {
            if admitted.len() >= target {
                break;
            }
            admit(*index, &mut admitted, &mut admitted_per_cluster);
        }
    }

    for index in deep {
        if admitted.contains(&index) {
            candidates[index].stage = "shortlisted".into();
            candidates[index].excluded_reason = None;
        } else {
            // The stage stays where the article stopped (`admitted` when the
            // deep assessment never happened, else `assessed`).
            candidates[index].excluded_reason = Some(
                if suppressed_at_base_cap.contains(&index) {
                    "cluster_suppressed"
                } else {
                    "shortlist_cap"
                }
                .into(),
            );
        }
    }
    RankSummary {
        shortlisted: admitted.len(),
        clusters: clusters.len(),
    }
}

#[allow(clippy::too_many_arguments)]
fn admit_under_cap(
    candidates: &[Candidate],
    sorted: &[usize],
    target: usize,
    cap: usize,
    admitted: &mut HashSet<usize>,
    admitted_per_cluster: &mut HashMap<i64, usize>,
    mut suppressed: Option<&mut HashSet<usize>>,
) {
    for index in sorted {
        if admitted.len() >= target || admitted.contains(index) {
            continue;
        }
        let Some(cluster) = candidates[*index].cluster else {
            continue;
        };
        if admitted_per_cluster.get(&cluster).copied().unwrap_or(0) >= cap {
            if let Some(suppressed) = suppressed.as_deref_mut() {
                suppressed.insert(*index);
            }
            continue;
        }
        admitted.insert(*index);
        *admitted_per_cluster.entry(cluster).or_default() += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DiversityConfig;
    use crate::curate::prefilter::tests::article;
    use crate::curate::signals::Signals;
    use crate::types::{Deep, Facets};

    fn candidate(id: i64, utility_hint: f64) -> Candidate {
        let mut candidate = Candidate::new(article(id, &format!("article {id}"), 800), false);
        candidate.stage = "assessed".into();
        candidate.signals = Signals {
            heuristic: Some(utility_hint),
            ..Signals::default()
        };
        candidate.assessment.deep = Some(Deep {
            quality: utility_hint,
            fit: utility_hint,
            category: Some("Top Stories".into()),
            rationale: "specific".into(),
            paywalled_guess: false,
            facets: Facets::default(),
            model: "mock".into(),
            prompt_version: 1,
            assessed_at: "2026-09-02T00:00:00Z".parse().expect("timestamp"),
        });
        candidate
    }

    fn vector(angle: f32) -> Vec<f32> {
        vec![angle.cos(), angle.sin()]
    }

    fn ranking(keep: usize, protected: usize, cap: usize, threshold: f64) -> RankingConfig {
        RankingConfig {
            shortlist_keep: keep,
            diversity: DiversityConfig {
                cluster_threshold: threshold,
                per_cluster_cap: cap,
                utility_protected: protected,
            },
            ..RankingConfig::default()
        }
    }

    #[test]
    fn utility_renormalizes_present_signals_and_gates_learned_ones() {
        let mut a = candidate(1, 8.0);
        let mut b = candidate(2, 4.0);
        a.signals.interest = Some(1.0);
        b.signals.interest = None;
        a.signals.knn = Some(0.9);
        a.signals.knn_gate = 0.5;
        let mut values = vec![a, b];
        calculate_utility(&mut values, &UtilityWeights::default());
        let [a, b] = values.as_slice() else {
            panic!("two values")
        };
        for candidate in [&a, &b] {
            assert!((candidate.signals.weights.values().sum::<f64>() - 1.0).abs() < 1e-9);
            assert!(candidate.utility.is_some());
        }
        assert!(!b.signals.weights.contains_key("interest"));
        assert!(
            (a.signals.weights["knn"] / a.signals.weights["quality"] - (0.15 * 0.5) / 0.40).abs()
                < 1e-9
        );
    }

    #[test]
    fn slop_authors_keep_their_weights_but_lose_most_of_their_utility() {
        let mut reported = candidate(1, 8.0);
        let mut other = candidate(2, 8.0);
        reported.signals.slop_author = true;
        reported.signals.slop_penalty = 0.75;
        other.signals.slop_author = false;
        other.signals.slop_penalty = 0.75;
        let mut values = vec![reported, other];
        calculate_utility(&mut values, &UtilityWeights::default());
        let [reported, other] = values.as_slice() else {
            panic!("two values")
        };
        assert_eq!(reported.signals.weights, other.signals.weights);
        assert!((reported.utility.unwrap() - other.utility.unwrap() * 0.25).abs() < 1e-9);
    }

    #[test]
    fn duplicates_cluster_and_the_third_is_suppressed() {
        let ranking = ranking(3, 0, 2, 0.85);
        let mut candidates = vec![
            candidate(1, 9.0),
            candidate(2, 8.0),
            candidate(3, 7.0),
            candidate(4, 6.0),
        ];
        let embeddings = HashMap::from([
            (1, vector(0.0)),
            (2, vector(0.1)),
            (3, vector(0.2)),
            (4, vector(2.0)),
        ]);
        let summary = shortlist(&mut candidates, &embeddings, &ranking);
        assert_eq!(summary.shortlisted, 3);
        assert_eq!(candidates[0].cluster, candidates[1].cluster);
        assert_eq!(candidates[1].cluster, candidates[2].cluster);
        assert_eq!(
            candidates[2].excluded_reason.as_deref(),
            Some("cluster_suppressed")
        );
    }

    #[test]
    fn protected_items_survive_and_count_toward_the_cap() {
        let ranking = ranking(3, 2, 1, 0.85);
        let mut candidates = vec![
            candidate(1, 9.0),
            candidate(2, 8.0),
            candidate(3, 7.0),
            candidate(4, 6.0),
        ];
        let embeddings = HashMap::from([
            (1, vector(0.0)),
            (2, vector(0.05)),
            (3, vector(0.1)),
            (4, vector(2.0)),
        ]);
        shortlist(&mut candidates, &embeddings, &ranking);
        assert_eq!(candidates[0].stage, "shortlisted");
        assert_eq!(candidates[1].stage, "shortlisted");
        assert_ne!(candidates[2].stage, "shortlisted");
    }

    #[test]
    fn bridge_case_uses_leaders_not_transitive_components() {
        let ranking = ranking(3, 0, 2, 0.80);
        let mut candidates = vec![candidate(1, 9.0), candidate(2, 8.0), candidate(3, 7.0)];
        // A at 0°, B at 72°, C at 36°: A~C and B~C, but A not~B.
        let embeddings =
            HashMap::from([(1, vector(0.0)), (2, vector(1.2566)), (3, vector(0.6283))]);
        let summary = shortlist(&mut candidates, &embeddings, &ranking);
        assert_eq!(summary.clusters, 2);
        assert_eq!(candidates[0].cluster, candidates[2].cluster);
        assert_ne!(candidates[0].cluster, candidates[1].cluster);
    }

    #[test]
    fn missing_embeddings_are_singletons_and_never_suppressed() {
        let ranking = ranking(3, 0, 1, 0.85);
        let mut candidates = vec![candidate(1, 9.0), candidate(2, 8.0), candidate(3, 7.0)];
        shortlist(&mut candidates, &HashMap::new(), &ranking);
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.stage == "shortlisted")
        );
        assert_eq!(
            candidates
                .iter()
                .filter_map(|candidate| candidate.cluster)
                .collect::<HashSet<_>>()
                .len(),
            3
        );
    }

    #[test]
    fn caps_relax_to_three_then_uncapped_when_the_shortlist_is_short() {
        // Six near-duplicates, keep 5: cap 2 admits two, cap 3 admits a third,
        // and the uncapped pass fills the remaining two slots in utility order.
        let ranking = ranking(5, 0, 2, 0.85);
        let mut candidates = (1..=6)
            .map(|id| candidate(id, 10.0 - id as f64))
            .collect::<Vec<_>>();
        let embeddings = (1..=6)
            .map(|id| (id, vector(0.01 * id as f32)))
            .collect::<HashMap<_, _>>();
        let summary = shortlist(&mut candidates, &embeddings, &ranking);
        assert_eq!(summary.clusters, 1);
        assert_eq!(summary.shortlisted, 5);
        let shortlisted = candidates
            .iter()
            .filter(|candidate| candidate.stage == "shortlisted")
            .map(|candidate| candidate.article.id)
            .collect::<Vec<_>>();
        assert_eq!(shortlisted, vec![1, 2, 3, 4, 5], "filled in utility order");
        assert_eq!(candidates[5].stage, "assessed");
        assert_eq!(
            candidates[5].excluded_reason.as_deref(),
            Some("cluster_suppressed")
        );
        assert_eq!(
            candidates
                .iter()
                .map(|c| c.rank_utility)
                .collect::<Vec<_>>(),
            (1..=6).map(Some).collect::<Vec<_>>()
        );
        assert_eq!(
            candidates
                .iter()
                .map(|c| c.cluster_rank)
                .collect::<Vec<_>>(),
            (1..=6).map(Some).collect::<Vec<_>>()
        );
    }

    #[test]
    fn shortlist_cap_is_the_reason_beyond_the_keep() {
        let ranking = ranking(2, 0, 2, 0.85);
        let mut candidates = vec![candidate(1, 9.0), candidate(2, 8.0), candidate(3, 7.0)];
        shortlist(&mut candidates, &HashMap::new(), &ranking);
        assert_eq!(candidates[0].stage, "shortlisted");
        assert_eq!(candidates[1].stage, "shortlisted");
        assert_eq!(candidates[2].stage, "assessed");
        assert_eq!(
            candidates[2].excluded_reason.as_deref(),
            Some("shortlist_cap")
        );
    }

    #[test]
    fn exploration_picks_get_up_to_three_reserved_slots() {
        // Keep 4 with the four best by utility being ordinary articles: three
        // exploration picks are still reserved seats, the fourth is not.
        let ranking = ranking(4, 0, 2, 0.85);
        let mut candidates = (1..=8)
            .map(|id| candidate(id, 10.0 - id as f64))
            .collect::<Vec<_>>();
        for candidate in candidates.iter_mut().skip(4) {
            candidate.exploration = true;
        }
        let summary = shortlist(&mut candidates, &HashMap::new(), &ranking);
        assert_eq!(summary.shortlisted, 4);
        let shortlisted = candidates
            .iter()
            .filter(|candidate| candidate.stage == "shortlisted")
            .map(|candidate| candidate.article.id)
            .collect::<Vec<_>>();
        assert_eq!(shortlisted, vec![1, 5, 6, 7]);
        assert_eq!(
            candidates[7].excluded_reason.as_deref(),
            Some("shortlist_cap")
        );
    }

    #[test]
    fn auto_includes_are_admitted_regardless_and_count_toward_their_cluster() {
        let ranking = ranking(2, 0, 1, 0.85);
        let mut candidates = vec![candidate(1, 9.0), candidate(2, 8.0), candidate(3, 1.0)];
        candidates[2].auto_include = true;
        let embeddings = HashMap::from([(1, vector(0.0)), (2, vector(2.0)), (3, vector(0.05))]);
        let summary = shortlist(&mut candidates, &embeddings, &ranking);
        assert_eq!(summary.shortlisted, 2);
        assert_eq!(candidates[2].stage, "shortlisted", "auto-include survives");
        // The auto-include filled its cluster's single seat, so the stronger
        // near-duplicate is suppressed and the unrelated article gets the slot.
        assert_eq!(candidates[0].cluster, candidates[2].cluster);
        assert_eq!(candidates[0].stage, "assessed");
        assert_eq!(
            candidates[0].excluded_reason.as_deref(),
            Some("cluster_suppressed")
        );
        assert_eq!(candidates[1].stage, "shortlisted");
    }

    #[test]
    fn unassessed_articles_rank_on_present_signals_and_keep_their_stage() {
        // DeepSeek down: no quality/fit anywhere, utility comes from what is present.
        let ranking = ranking(2, 0, 2, 0.85);
        let mut candidates = vec![candidate(1, 3.0), candidate(2, 6.0), candidate(3, 9.0)];
        for candidate in &mut candidates {
            candidate.assessment.deep = None;
            candidate.stage = "admitted".into();
        }
        candidates[0].assessment.triage = Some(crate::types::Triage {
            interest: 9.0,
            kind: "essay".into(),
            why: "promising".into(),
            model: "mock".into(),
            prompt_version: 1,
            assessed_at: "2026-09-02T00:00:00Z".parse().expect("timestamp"),
        });
        let summary = shortlist(&mut candidates, &HashMap::new(), &ranking);
        assert_eq!(summary.shortlisted, 2);
        for candidate in &candidates {
            assert!(candidate.utility.is_some(), "scored on present signals");
            assert!(!candidate.signals.weights.contains_key("quality"));
            assert!(!candidate.signals.weights.contains_key("fit"));
            assert!((candidate.signals.weights.values().sum::<f64>() - 1.0).abs() < 1e-9);
        }
        // Triage (0.05) outweighs heuristic (0.02): the triaged article with
        // the weakest heuristic overtakes the middle one.
        assert_eq!(candidates[2].rank_utility, Some(1));
        assert_eq!(candidates[0].rank_utility, Some(2));
        assert_eq!(candidates[1].rank_utility, Some(3));
        assert_eq!(
            candidates[1].stage, "admitted",
            "never assessed, so not `assessed`"
        );
        assert_eq!(
            candidates[1].excluded_reason.as_deref(),
            Some("shortlist_cap")
        );
    }

    #[test]
    fn percentiles_are_taken_over_the_deep_set_only() {
        // The eligible-but-not-admitted article has the strongest heuristic;
        // it must not shift the deep set's percentiles or receive a utility.
        let ranking = ranking(10, 0, 2, 0.85);
        let mut candidates = vec![candidate(1, 5.0), candidate(2, 5.0), candidate(3, 5.0)];
        candidates[2].stage = "triaged".into();
        candidates[2].excluded_reason = Some("not_admitted".into());
        candidates[2].signals.heuristic = Some(99.0);
        candidates[1].signals.heuristic = Some(5.0);
        shortlist(&mut candidates, &HashMap::new(), &ranking);
        assert_eq!(
            candidates[0].signals.norm["heuristic"], 0.5,
            "ties share a percentile"
        );
        assert_eq!(candidates[1].signals.norm["heuristic"], 0.5);
        assert!(candidates[2].utility.is_none());
        assert!(candidates[2].rank_utility.is_none());
    }
}
