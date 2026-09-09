//! Cheap per-article ranking signals, the mid-rank percentile normalizer and the
//! preliminary blend (plan §9, §12.2, §12.4).
//!
//! Every signal is an `Option<f64>`: `None` means *absent*, which is never a
//! numeric zero. Absent signals are left out of the percentile computation and
//! of the blend, whose remaining weights are renormalized (§12.2, §12.4).

use std::collections::{BTreeMap, HashMap, HashSet};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::config::{PreliminaryWeights, RankingConfig, VoyageConfig};
use crate::curate::embedding::{dot, load_article_embeddings};
use crate::curate::prefilter;
use crate::db::Db;
use crate::types::{Article, ArticleId, FeedId, SourceKind};

/// Below this many embedded eligible articles the z-score is too noisy, so the
/// interest signal falls back to the raw top-1 cosine (§9.1).
pub const INTEREST_ZSCORE_MIN_ARTICLES: usize = 30;
/// Standard-deviation floor for the per-interest z-score (§9.1).
const ZSCORE_STD_FLOOR: f64 = 1e-3;
/// How many interests and rated neighbours `signals_json` records (§7.5).
const RECORDED_TOP: usize = 3;
/// Aggregators carried the link rather than authored the article, so they get
/// only a small share of an aggregator-only article's feed-affinity credit.
pub const AGGREGATOR_FEED_SHARE: f64 = 0.25;

/// The signal names that go through the percentile normalizer, in the order
/// they are rendered (§12.2). LLM scores (`triage`, `quality`, `fit`) are
/// absolute and arrive in steps 4–5.
pub const PERCENTILE_SIGNALS: [&str; 5] = ["interest", "knn", "feed", "social", "heuristic"];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TopInterest {
    pub name: String,
    pub z: f64,
    pub cos: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Neighbour {
    pub article_id: ArticleId,
    pub label: String,
    pub cos: f64,
    pub title: String,
}

/// Every cheap signal for one article, plus what the normalizer and the blend
/// derived from them (§9, §12.2, §12.4).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Signals {
    pub interest: Option<f64>,
    /// Raw top-1 cosine behind `interest`, recorded for `explain` (§7.5).
    pub interest_top1_cos: Option<f64>,
    pub knn: Option<f64>,
    pub feed: Option<f64>,
    pub social: Option<f64>,
    pub heuristic: Option<f64>,
    /// Mid-rank percentiles of the present signals (§12.2).
    #[serde(default)]
    pub norm: BTreeMap<String, f64>,
    /// Effective preliminary weights after gating and renormalization (§12.4).
    #[serde(default)]
    pub weights: BTreeMap<String, f64>,
    #[serde(default)]
    pub top_interests: Vec<TopInterest>,
    #[serde(default)]
    pub neighbours: Vec<Neighbour>,
    #[serde(default)]
    pub notes: Vec<String>,
    /// Preliminary blend on a 0–100 scale; `None` when nothing is present.
    pub preliminary: Option<f64>,
    /// The author has a current *AI slop* verdict (§9.3).
    #[serde(default)]
    pub slop_author: bool,
    /// Gate ramps applied to the learned signals' weights (§9.2, §9.3).
    #[serde(skip)]
    pub knn_gate: f64,
    #[serde(skip)]
    pub feed_gate: f64,
    /// `ranking.slop_author_penalty`, applied when `slop_author` is set.
    #[serde(skip)]
    pub slop_penalty: f64,
}

impl Signals {
    /// The multiplier the slop-author penalty applies to the blend and the
    /// utility: `1 − penalty` for a reported author, `1` otherwise (§9.3).
    pub fn slop_factor(&self) -> f64 {
        if self.slop_author {
            (1.0 - self.slop_penalty).clamp(0.0, 1.0)
        } else {
            1.0
        }
    }
}

impl Signals {
    /// The raw value of a named signal, `None` when absent or unknown.
    pub fn raw(&self, name: &str) -> Option<f64> {
        match name {
            "interest" => self.interest,
            "interest_top1_cos" => self.interest_top1_cos,
            "knn" => self.knn,
            "feed" => self.feed,
            "social" => self.social,
            "heuristic" => self.heuristic,
            _ => None,
        }
    }

    pub fn present(&self, name: &str) -> bool {
        self.raw(name).is_some()
    }

    /// The signals every eligible article gets without embeddings or ratings.
    pub fn baseline(article: &Article) -> Self {
        Self {
            social: (!article.social.is_empty()).then(|| article.social_score()),
            heuristic: Some(prefilter::text_heuristic(article)),
            ..Self::default()
        }
    }
}

/// What the run log and the report say about the learned signals (§9.2, §15.4).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PreferenceSummary {
    pub rated_with_embeddings: usize,
    pub attributable_feed_ratings: usize,
    pub knn_gate: f64,
    pub feed_gate: f64,
}

/// One rated article with an embedding: the unit of the preference state (§9.2).
#[derive(Debug, Clone, PartialEq)]
pub struct RatedExample {
    pub article_id: ArticleId,
    pub label: String,
    pub title: String,
    /// The vote's value (`loved` 1.0, `good` 0.35, `not_for_me` −1.0).
    pub value: f64,
    /// `0.5 ^ (age_days / half_life_days)` at the time of the run.
    pub decay: f64,
    pub embedding: Vec<f32>,
    /// Distinct direct feeds that carried the rated article (§9.3).
    pub feeds: Vec<FeedId>,
    /// Whitespace-normalized, lowercase author key (§9.3).
    pub author: Option<String>,
    /// Whether the article arrived only through link aggregators (§9.3).
    pub aggregator_only: bool,
}

impl RatedExample {
    /// `weight_i = value_i × decay_i` (§9.2).
    pub fn weight(&self) -> f64 {
        self.value * self.decay
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct FeedRate {
    up: f64,
    down: f64,
}

impl FeedRate {
    /// Beta-smoothed rate `(up + 1) / (up + down + 2)` (§9.3).
    fn rate(self) -> f64 {
        (self.up + 1.0) / (self.up + self.down + 2.0)
    }
}

/// Rated-neighbour and feed-affinity state, built once per run (§9.2, §9.3).
#[derive(Debug, Clone, Default)]
pub struct PreferenceState {
    pub examples: Vec<RatedExample>,
    feed_rates: HashMap<FeedId, FeedRate>,
    author_rates: HashMap<String, FeedRate>,
    /// Normalized keys of authors with a current *AI slop* verdict (§9.3).
    slop_authors: HashSet<String>,
    pub slop_author_penalty: f64,
    pub attributable_feed_ratings: usize,
    pub knn_gate: f64,
    pub feed_gate: f64,
}

impl PreferenceState {
    /// Build the state from already-loaded examples (pure; tests use this).
    pub fn build(mut examples: Vec<RatedExample>, ranking: &RankingConfig) -> Self {
        for example in &mut examples {
            example.author = normalize_author(example.author.as_deref());
        }
        let (feed_rates, author_rates, attributable_feed_ratings) = feed_rates(&examples);
        Self {
            knn_gate: gate(examples.len(), ranking.knn_floor, ranking.knn_full),
            feed_gate: gate(
                attributable_feed_ratings,
                ranking.feed_floor,
                ranking.feed_full,
            ),
            examples,
            feed_rates,
            author_rates,
            slop_authors: HashSet::new(),
            slop_author_penalty: ranking.slop_author_penalty,
            attributable_feed_ratings,
        }
    }

    /// Register the authors whose current verdict is *AI slop*; keys are
    /// normalized like [`normalize_author`] and empty ones are dropped.
    pub fn with_slop_authors<I, S>(mut self, authors: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.slop_authors = authors
            .into_iter()
            .filter_map(|author| normalize_author(Some(author.as_ref())))
            .collect();
        self
    }

    /// Whether the article's author has a current *AI slop* verdict (§9.3).
    pub fn is_slop_author(&self, article: &Article) -> bool {
        normalize_author(article.author.as_deref())
            .is_some_and(|author| self.slop_authors.contains(&author))
    }

    pub fn slop_author_count(&self) -> usize {
        self.slop_authors.len()
    }

    /// Load `db::current_ratings(rating_lookback_days)` joined to
    /// `article_embeddings`; ratings without an embedding are skipped (§9.2).
    pub async fn load(
        db: &Db,
        voyage: &VoyageConfig,
        ranking: &RankingConfig,
        now: Timestamp,
    ) -> anyhow::Result<Self> {
        let ratings = db.current_ratings(ranking.rating_lookback_days).await?;
        let ids = ratings
            .iter()
            .map(|rating| rating.article_id)
            .collect::<Vec<_>>();
        let embeddings = load_article_embeddings(db, voyage, &ids).await?;
        let mut examples = Vec::new();
        for rating in ratings {
            let Some(embedding) = embeddings.get(&rating.article_id).cloned() else {
                continue;
            };
            let article = db.get_article(rating.article_id).await?;
            let feeds = article.as_ref().map(direct_feeds).unwrap_or_default();
            let author = article
                .as_ref()
                .and_then(|article| normalize_author(article.author.as_deref()));
            let aggregator_only = article
                .as_ref()
                .is_some_and(crate::discovery::aggregator_only);
            let age_days = (now.as_second() - rating.event_at.as_second()).max(0) as f64 / 86_400.0;
            examples.push(RatedExample {
                article_id: rating.article_id,
                label: rating.label,
                title: rating.title,
                value: rating.value,
                decay: decay(age_days, ranking.rating_half_life_days),
                embedding,
                feeds,
                author,
                aggregator_only,
            });
        }
        let slop_authors = db.slop_authors().await?;
        Ok(Self::build(examples, ranking).with_slop_authors(slop_authors))
    }

    pub fn summary(&self) -> PreferenceSummary {
        PreferenceSummary {
            rated_with_embeddings: self.examples.len(),
            attributable_feed_ratings: self.attributable_feed_ratings,
            knn_gate: self.knn_gate,
            feed_gate: self.feed_gate,
        }
    }

    /// The once-per-run log line of §9.2.
    pub fn log(&self, ranking: &RankingConfig) {
        let feed_detail = if self.feed_gate > 0.0 {
            format!("(n={})", self.attributable_feed_ratings)
        } else {
            format!(
                "(n={} < {})",
                self.attributable_feed_ratings, ranking.feed_floor
            )
        };
        tracing::info!(
            rated_with_embeddings = self.examples.len(),
            knn_gate = self.knn_gate,
            feed_gate = self.feed_gate,
            slop_authors = self.slop_authors.len(),
            "preference: {} rated articles with embeddings → knn gate {:.2}; feed gate {:.1} {}; {} slop authors (penalty {:.2})",
            self.examples.len(),
            self.knn_gate,
            self.feed_gate,
            feed_detail,
            self.slop_authors.len(),
            self.slop_author_penalty
        );
    }

    /// Signed rated-neighbour preference and the three nearest rated articles
    /// (§9.2). Absent when the gate is closed or there are no examples.
    pub fn knn(&self, candidate: &[f32], ranking: &RankingConfig) -> (Option<f64>, Vec<Neighbour>) {
        if self.knn_gate <= 0.0 || self.examples.is_empty() {
            return (None, Vec::new());
        }
        let mut scored = self
            .examples
            .iter()
            .filter_map(|example| {
                dot(candidate, &example.embedding)
                    .ok()
                    .map(|s| (s, example))
            })
            .collect::<Vec<_>>();
        scored.sort_by(|left, right| right.0.total_cmp(&left.0));

        let side = |positive: bool| -> Option<f64> {
            let chosen = scored
                .iter()
                .filter(|(_, example)| (example.weight() > 0.0) == positive)
                .take(ranking.neighbour_k.max(1))
                .collect::<Vec<_>>();
            let denominator = chosen
                .iter()
                .map(|(_, example)| example.weight().abs())
                .sum::<f64>();
            (denominator > 0.0).then(|| {
                chosen
                    .iter()
                    .map(|(similarity, example)| example.weight().abs() * similarity)
                    .sum::<f64>()
                    / denominator
            })
        };
        let positive = side(true);
        let negative = side(false);
        let knn = (positive.is_some() || negative.is_some()).then(|| {
            positive.unwrap_or(0.0) - ranking.negative_coefficient * negative.unwrap_or(0.0)
        });
        let neighbours = scored
            .iter()
            .take(RECORDED_TOP)
            .map(|(cos, example)| Neighbour {
                article_id: example.article_id,
                label: example.label.clone(),
                cos: *cos,
                title: example.title.clone(),
            })
            .collect();
        (knn, neighbours)
    }

    /// Mean Beta-smoothed rate over the article's rated direct feeds and author
    /// (§9.3).
    pub fn feed(&self, article: &Article) -> Option<f64> {
        if self.feed_gate <= 0.0 {
            return None;
        }
        let mut rates = direct_feeds(article)
            .into_iter()
            .filter_map(|feed| self.feed_rates.get(&feed))
            .map(|rate| rate.rate())
            .collect::<Vec<_>>();
        if let Some(rate) = normalize_author(article.author.as_deref())
            .as_ref()
            .and_then(|author| self.author_rates.get(author))
        {
            rates.push(rate.rate());
        }
        (!rates.is_empty()).then(|| rates.iter().sum::<f64>() / rates.len() as f64)
    }

    /// Per-feed `(up, down)` credit, exposed for tests of §9.3.
    pub fn feed_credit(&self, feed: FeedId) -> Option<(f64, f64)> {
        self.feed_rates.get(&feed).map(|rate| (rate.up, rate.down))
    }

    /// Per-author `(up, down)` credit, exposed for tests of §9.3.
    pub fn author_credit(&self, author: &str) -> Option<(f64, f64)> {
        normalize_author(Some(author))
            .and_then(|author| self.author_rates.get(&author))
            .map(|rate| (rate.up, rate.down))
    }
}

/// `0.5 ^ (age_days / half_life_days)` (§9.2).
pub fn decay(age_days: f64, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return 1.0;
    }
    0.5f64.powf(age_days.max(0.0) / half_life_days)
}

/// `clamp((n − floor) / (full − floor), 0, 1)` (§9.2).
pub fn gate(n: usize, floor: usize, full: usize) -> f64 {
    if n <= floor {
        0.0
    } else if n >= full || full <= floor {
        1.0
    } else {
        (n - floor) as f64 / (full - floor) as f64
    }
}

/// Distinct `SourceKind::Feed` feeds that carried the article; the best entry's
/// feed when there are none (§9.3).
pub fn direct_feeds(article: &Article) -> Vec<FeedId> {
    let mut feeds = article
        .sources
        .iter()
        .filter(|source| source.kind == SourceKind::Feed)
        .map(|source| source.feed_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if feeds.is_empty() && article.feed_id != 0 {
        feeds.push(article.feed_id);
    }
    feeds.sort_unstable();
    feeds
}

fn normalize_author(author: Option<&str>) -> Option<String> {
    let normalized = author?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    (!normalized.is_empty()).then_some(normalized)
}

fn feed_rates(
    examples: &[RatedExample],
) -> (HashMap<FeedId, FeedRate>, HashMap<String, FeedRate>, usize) {
    let mut feed_rates: HashMap<FeedId, FeedRate> = HashMap::new();
    let mut author_rates: HashMap<String, FeedRate> = HashMap::new();
    let mut attributable = 0;
    for example in examples {
        let weight = example.weight();
        if !example.feeds.is_empty() {
            let feed_weight = if example.aggregator_only {
                weight * AGGREGATOR_FEED_SHARE
            } else {
                weight
            };
            let credit = feed_weight / example.feeds.len() as f64;
            for feed in &example.feeds {
                let rate = feed_rates.entry(*feed).or_default();
                rate.up += credit.max(0.0);
                rate.down += (-credit).max(0.0);
            }
        }
        if let Some(author) = &example.author {
            let rate = author_rates.entry(author.clone()).or_default();
            rate.up += weight.max(0.0);
            rate.down += (-weight).max(0.0);
        }
        if !example.feeds.is_empty() || example.author.is_some() {
            attributable += 1;
        }
    }
    (feed_rates, author_rates, attributable)
}

/// The interest match of §9.1 for one article.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InterestMatch {
    pub score: f64,
    pub top1_cos: f64,
    pub top_interests: Vec<TopInterest>,
}

/// Z-scored standing-interest match for every embedded article (§9.1).
///
/// Below [`INTEREST_ZSCORE_MIN_ARTICLES`] embedded articles the score is the raw
/// top-1 cosine instead, and that is logged.
pub fn interest_matches(
    articles: &HashMap<ArticleId, Vec<f32>>,
    interests: &HashMap<String, Vec<f32>>,
) -> HashMap<ArticleId, InterestMatch> {
    if articles.is_empty() || interests.is_empty() {
        return HashMap::new();
    }
    let fallback = articles.len() < INTEREST_ZSCORE_MIN_ARTICLES;
    if fallback {
        tracing::info!(
            embedded = articles.len(),
            "fewer than {INTEREST_ZSCORE_MIN_ARTICLES} embedded articles; interest uses the raw top-1 cosine"
        );
    }

    let mut matches: HashMap<ArticleId, Vec<TopInterest>> = HashMap::new();
    for (name, interest) in interests {
        let similarities = articles
            .iter()
            .filter_map(|(article_id, article)| {
                dot(interest, article).ok().map(|cos| (*article_id, cos))
            })
            .collect::<Vec<_>>();
        if similarities.is_empty() {
            continue;
        }
        let n = similarities.len() as f64;
        let mean = similarities.iter().map(|(_, cos)| cos).sum::<f64>() / n;
        let variance = similarities
            .iter()
            .map(|(_, cos)| (cos - mean).powi(2))
            .sum::<f64>()
            / n;
        let std = variance.sqrt().max(ZSCORE_STD_FLOOR);
        for (article_id, cos) in similarities {
            matches.entry(article_id).or_default().push(TopInterest {
                name: name.clone(),
                z: (cos - mean) / std,
                cos,
            });
        }
    }

    matches
        .into_iter()
        .map(|(article_id, mut all)| {
            all.sort_by(|left, right| {
                right
                    .z
                    .total_cmp(&left.z)
                    .then_with(|| left.name.cmp(&right.name))
            });
            let top1_cos = all
                .iter()
                .map(|item| item.cos)
                .fold(f64::NEG_INFINITY, f64::max);
            all.truncate(RECORDED_TOP);
            let score = if fallback {
                top1_cos
            } else {
                let top_mean = all.iter().map(|item| item.z).sum::<f64>() / all.len() as f64;
                0.7 * all[0].z + 0.3 * top_mean
            };
            (
                article_id,
                InterestMatch {
                    score,
                    top1_cos,
                    top_interests: all,
                },
            )
        })
        .collect()
}

/// Every cheap signal for the eligible set, normalized and blended (§9, §12.2,
/// §12.4). Pure: the preference state is already loaded.
pub fn compute(
    articles: &[Article],
    article_embeddings: &HashMap<ArticleId, Vec<f32>>,
    interest_embeddings: &HashMap<String, Vec<f32>>,
    preference: &PreferenceState,
    ranking: &RankingConfig,
) -> HashMap<ArticleId, Signals> {
    let interests = interest_matches(article_embeddings, interest_embeddings);
    let mut all = articles
        .iter()
        .map(|article| {
            let mut signals = Signals::baseline(article);
            signals.knn_gate = preference.knn_gate;
            signals.feed_gate = preference.feed_gate;
            if let Some(matched) = interests.get(&article.id) {
                signals.interest = Some(matched.score);
                signals.interest_top1_cos = Some(matched.top1_cos);
                signals.top_interests = matched.top_interests.clone();
            }
            if let Some(embedding) = article_embeddings.get(&article.id) {
                let (knn, neighbours) = preference.knn(embedding, ranking);
                signals.knn = knn;
                signals.neighbours = neighbours;
            }
            signals.feed = preference.feed(article);
            if preference.is_slop_author(article) {
                signals.slop_author = true;
                signals.slop_penalty = preference.slop_author_penalty;
                signals.notes.push(format!(
                    "author reported as AI slop: blend and utility × {:.2}",
                    signals.slop_factor()
                ));
            }
            if preference.knn_gate > 0.0 {
                signals.notes.push(format!(
                    "knn gate {:.2} (n={} rated with embeddings)",
                    preference.knn_gate,
                    preference.examples.len()
                ));
            }
            signals
        })
        .collect::<Vec<_>>();

    normalize(&mut all.iter_mut().collect::<Vec<_>>());
    for signals in &mut all {
        preliminary_blend(signals, &ranking.weights.preliminary);
    }
    articles.iter().map(|article| article.id).zip(all).collect()
}

/// [`compute`] with the preference state loaded from the database.
pub async fn compute_all(
    db: &Db,
    articles: &[Article],
    article_embeddings: &HashMap<ArticleId, Vec<f32>>,
    interest_embeddings: &HashMap<String, Vec<f32>>,
    voyage: &VoyageConfig,
    ranking: &RankingConfig,
    now: Timestamp,
) -> anyhow::Result<(HashMap<ArticleId, Signals>, PreferenceSummary)> {
    let preference = PreferenceState::load(db, voyage, ranking, now).await?;
    preference.log(ranking);
    Ok((
        compute(
            articles,
            article_embeddings,
            interest_embeddings,
            &preference,
            ranking,
        ),
        preference.summary(),
    ))
}

/// Mid-rank percentiles over the present values of each signal (§12.2).
///
/// `p(x) = (count_below + (count_equal + 1) / 2) / n_present`; fewer than two
/// present values or all-equal values give 0.5. Article id never breaks ties.
pub fn normalize(signals: &mut [&mut Signals]) {
    for name in PERCENTILE_SIGNALS {
        let mut values = signals
            .iter()
            .filter_map(|signal| signal.raw(name))
            .collect::<Vec<_>>();
        if values.is_empty() {
            continue;
        }
        values.sort_by(f64::total_cmp);
        let n = values.len() as f64;
        let constant = values.len() < 2 || values.first() == values.last();
        for signal in signals.iter_mut() {
            let Some(value) = signal.raw(name) else {
                continue;
            };
            let percentile = if constant {
                0.5
            } else {
                let below = values.partition_point(|other| *other < value);
                let equal = values.partition_point(|other| *other <= value) - below;
                (below as f64 + (equal as f64 + 1.0) / 2.0) / n
            };
            signal.norm.insert(name.to_string(), percentile);
        }
    }
}

/// The preliminary blend of §12.4 on a 0–100 scale: present-and-active
/// signals only, learned weights multiplied by their gate, renormalized to 1,
/// then scaled by the slop-author factor (§9.3).
pub fn preliminary_blend(signals: &mut Signals, configured: &PreliminaryWeights) -> Option<f64> {
    let candidates = [
        ("interest", configured.interest, 1.0),
        ("knn", configured.knn, signals.knn_gate),
        ("heuristic", configured.heuristic, 1.0),
        ("feed", configured.feed, signals.feed_gate),
        ("social", configured.social, 1.0),
    ];
    let active = candidates
        .into_iter()
        .filter_map(|(name, weight, gate)| {
            let norm = *signals.norm.get(name)?;
            let effective = weight * gate;
            (effective > 0.0).then_some((name, effective, norm))
        })
        .collect::<Vec<_>>();
    let total = active.iter().map(|(_, weight, _)| weight).sum::<f64>();
    if total <= 0.0 {
        signals.weights.clear();
        signals.preliminary = None;
        return None;
    }
    signals.weights = active
        .iter()
        .map(|(name, weight, _)| ((*name).to_string(), weight / total))
        .collect();
    let blend = active
        .iter()
        .map(|(_, weight, norm)| weight / total * norm)
        .sum::<f64>()
        * 100.0
        * signals.slop_factor();
    signals.preliminary = Some(blend);
    Some(blend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ExtractMethod, SourceRef};

    fn ranking() -> RankingConfig {
        RankingConfig::default()
    }

    fn unit(values: &[f32]) -> Vec<f32> {
        let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt();
        values.iter().map(|v| v / norm).collect()
    }

    fn example(id: ArticleId, label: &str, value: f64, embedding: &[f32]) -> RatedExample {
        RatedExample {
            article_id: id,
            label: label.into(),
            title: format!("rated {id}"),
            value,
            decay: 1.0,
            embedding: unit(embedding),
            feeds: vec![id],
            author: None,
            aggregator_only: false,
        }
    }

    fn article(id: ArticleId, feeds: &[FeedId]) -> Article {
        Article {
            id,
            canonical_url: format!("https://example.com/{id}"),
            title: format!("Article {id}"),
            best_entry_id: id,
            content_html: String::new(),
            word_count: 1000,
            excerpt_only: false,
            image_count: 0,
            sources: feeds
                .iter()
                .map(|feed| SourceRef {
                    entry_id: id,
                    feed_id: *feed,
                    feed_title: format!("feed {feed}"),
                    category: None,
                    kind: SourceKind::Feed,
                })
                .collect(),
            first_seen: "2026-08-15T00:00:00Z".parse().unwrap(),
            url: format!("https://example.com/{id}"),
            author: None,
            publication: None,
            feed_id: feeds.first().copied().unwrap_or(0),
            feed_title: String::new(),
            category: None,
            published_at: None,
            comments_url: None,
            image_urls: vec![],
            social: vec![],
            extract_method: ExtractMethod::Miniflux,
        }
    }

    fn with_heuristic(value: Option<f64>) -> Signals {
        Signals {
            heuristic: value,
            ..Signals::default()
        }
    }

    // --- §9.1 interest z-scores ---

    fn interest_fixture(n: usize) -> (HashMap<ArticleId, Vec<f32>>, HashMap<String, Vec<f32>>) {
        // Article 1 sits on axis x; the rest sit near axis y with a tiny spread.
        let mut articles = HashMap::new();
        articles.insert(1, unit(&[1.0, 0.0, 0.0]));
        for id in 2..=n as ArticleId {
            articles.insert(id, unit(&[0.0, 1.0, 0.001 * id as f32]));
        }
        // "Broad" is about equally close to everything; "Specific" matches only article 1.
        let mut interests = HashMap::new();
        interests.insert("Broad".to_string(), unit(&[1.0, 1.0, 0.0]));
        interests.insert("Specific".to_string(), unit(&[1.0, 0.0, 0.0]));
        (articles, interests)
    }

    #[test]
    fn specific_interest_with_one_strong_match_beats_a_broad_one() {
        let (articles, interests) = interest_fixture(40);
        let matched = interest_matches(&articles, &interests);
        let strong = &matched[&1];
        assert_eq!(strong.top_interests[0].name, "Specific");
        assert!(
            strong.top_interests[0].z > 3.0,
            "z = {}",
            strong.top_interests[0].z
        );
        let others = (2..=40)
            .map(|id| matched[&id].score)
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(strong.score > others + 2.0, "{} vs {others}", strong.score);
        // Raw cosine would have called Broad a near-tie everywhere (≈0.707).
        assert!((matched[&2].top1_cos - 0.707).abs() < 0.01);
    }

    #[test]
    fn interest_falls_back_to_raw_cosine_under_thirty_articles() {
        let (articles, interests) = interest_fixture(10);
        let matched = interest_matches(&articles, &interests);
        for (id, m) in &matched {
            assert!(
                (m.score - m.top1_cos).abs() < 1e-9,
                "article {id} should use raw top-1"
            );
        }
        assert!((matched[&2].score - 0.707).abs() < 0.01);
    }

    // --- §9.2 preference ---

    #[test]
    fn one_loved_article_gives_a_positive_knn_to_a_near_neighbour() {
        let mut ranking = ranking();
        ranking.knn_floor = 0;
        ranking.knn_full = 1;
        let state = PreferenceState::build(vec![example(1, "loved", 1.0, &[1.0, 0.0])], &ranking);
        let (knn, neighbours) = state.knn(&unit(&[0.9, 0.1]), &ranking);
        assert!(knn.unwrap() > 0.9);
        assert_eq!(neighbours.len(), 1);
        assert_eq!(neighbours[0].label, "loved");
        let (far, _) = state.knn(&unit(&[0.0, 1.0]), &ranking);
        assert!(far.unwrap().abs() < 1e-6);
    }

    #[test]
    fn two_unrelated_loved_clusters_both_score_high() {
        let mut ranking = ranking();
        ranking.knn_floor = 0;
        ranking.knn_full = 1;
        ranking.neighbour_k = 2;
        let state = PreferenceState::build(
            vec![
                example(1, "loved", 1.0, &[1.0, 0.0, 0.0]),
                example(2, "loved", 1.0, &[0.98, 0.02, 0.0]),
                example(3, "loved", 1.0, &[0.0, 1.0, 0.0]),
                example(4, "loved", 1.0, &[0.0, 0.98, 0.02]),
            ],
            &ranking,
        );
        let (near_a, _) = state.knn(&unit(&[1.0, 0.0, 0.0]), &ranking);
        let (near_b, _) = state.knn(&unit(&[0.0, 1.0, 0.0]), &ranking);
        assert!(near_a.unwrap() > 0.95, "{near_a:?}");
        assert!(near_b.unwrap() > 0.95, "{near_b:?}");
        // A centroid would have put both at ~0.7.
    }

    #[test]
    fn good_carries_a_third_of_loved() {
        let loved = example(1, "loved", 1.0, &[1.0, 0.0]);
        let good = example(2, "good", 0.35, &[1.0, 0.0]);
        assert!((good.weight() / loved.weight() - 0.35).abs() < 1e-9);

        // A mixed neighbourhood: the far example pulls the mean down by 0.35× as
        // much weight when it is merely "good" as when it is "loved".
        let mut ranking = ranking();
        ranking.knn_floor = 0;
        ranking.knn_full = 1;
        let near = example(1, "loved", 1.0, &[1.0, 0.0]);
        let candidate = unit(&[1.0, 0.0]);
        let both_loved = PreferenceState::build(
            vec![near.clone(), example(2, "loved", 1.0, &[0.0, 1.0])],
            &ranking,
        );
        let one_good =
            PreferenceState::build(vec![near, example(2, "good", 0.35, &[0.0, 1.0])], &ranking);
        let pull_loved = 1.0 - both_loved.knn(&candidate, &ranking).0.unwrap();
        let pull_good = 1.0 - one_good.knn(&candidate, &ranking).0.unwrap();
        assert!(pull_good < pull_loved);
        // Weighted means: 0.5 vs 1/1.35 → pulls 0.5 vs 0.35/1.35.
        assert!((pull_good / pull_loved - 0.35 / 1.35 / 0.5).abs() < 1e-9);
    }

    #[test]
    fn negatives_subtract_with_the_negative_coefficient() {
        let mut ranking = ranking();
        ranking.knn_floor = 0;
        ranking.knn_full = 1;
        let state =
            PreferenceState::build(vec![example(1, "not_for_me", -1.0, &[1.0, 0.0])], &ranking);
        let (knn, _) = state.knn(&unit(&[1.0, 0.0]), &ranking);
        assert!((knn.unwrap() + ranking.negative_coefficient).abs() < 1e-9);
    }

    #[test]
    fn decay_halves_at_the_half_life() {
        assert!((decay(60.0, 60.0) - 0.5).abs() < 1e-12);
        assert!((decay(0.0, 60.0) - 1.0).abs() < 1e-12);
        assert!((decay(120.0, 60.0) - 0.25).abs() < 1e-12);
    }

    #[test]
    fn gate_is_zero_below_floor_one_at_full_and_linear_between() {
        assert_eq!(gate(0, 8, 25), 0.0);
        assert_eq!(gate(8, 8, 25), 0.0);
        assert_eq!(gate(25, 8, 25), 1.0);
        assert_eq!(gate(100, 8, 25), 1.0);
        assert!((gate(16, 8, 24) - 0.5).abs() < 1e-9);
        assert!((gate(9, 8, 25) - 1.0 / 17.0).abs() < 1e-9);
    }

    #[test]
    fn knn_is_absent_when_the_gate_is_closed() {
        let ranking = ranking(); // knn_floor 8
        let state = PreferenceState::build(vec![example(1, "loved", 1.0, &[1.0, 0.0])], &ranking);
        assert_eq!(state.knn_gate, 0.0);
        assert_eq!(state.knn(&unit(&[1.0, 0.0]), &ranking), (None, Vec::new()));
    }

    // --- §9.3 feed affinity ---

    #[test]
    fn feed_credit_sums_to_one_across_direct_feeds() {
        let mut rated = example(1, "loved", 1.0, &[1.0, 0.0]);
        rated.feeds = vec![10, 20, 30];
        let state = PreferenceState::build(vec![rated], &ranking());
        let total: f64 = [10, 20, 30]
            .iter()
            .map(|feed| state.feed_credit(*feed).unwrap().0)
            .sum();
        assert!((total - 1.0).abs() < 1e-9);
        assert!((state.feed_credit(10).unwrap().0 - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn feed_affinity_is_the_mean_over_rated_feeds() {
        let mut ranking = ranking();
        ranking.feed_floor = 0;
        ranking.feed_full = 1;
        let mut loved = example(1, "loved", 1.0, &[1.0, 0.0]);
        loved.feeds = vec![10];
        let mut down = example(2, "not_for_me", -1.0, &[1.0, 0.0]);
        down.feeds = vec![20];
        let state = PreferenceState::build(vec![loved, down], &ranking);
        // feed 10: (1+1)/(1+0+2) = 2/3; feed 20: (0+1)/(0+1+2) = 1/3; unrated 99 ignored.
        let both = state.feed(&article(7, &[10, 20, 99])).unwrap();
        assert!((both - 0.5).abs() < 1e-9, "{both}");
        let best = state.feed(&article(8, &[10])).unwrap();
        assert!((best - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(state.feed(&article(9, &[99])), None);
    }

    #[test]
    fn aggregator_only_rating_splits_credit_between_feed_and_author() {
        let mut rated = example(1, "loved", 1.0, &[1.0, 0.0]);
        rated.decay = 0.4;
        rated.feeds = vec![10];
        rated.author = Some("example author".into());
        rated.aggregator_only = true;
        let state = PreferenceState::build(vec![rated], &ranking());

        assert!((state.feed_credit(10).unwrap().0 - 0.25 * 0.4).abs() < 1e-9);
        assert!((state.author_credit("example author").unwrap().0 - 0.4).abs() < 1e-9);
    }

    #[test]
    fn author_affinity_applies_across_feeds_with_normalized_keys() {
        let mut ranking = ranking();
        ranking.feed_floor = 0;
        ranking.feed_full = 1;
        let mut rated = example(1, "loved", 1.0, &[1.0, 0.0]);
        rated.feeds = vec![10];
        rated.author = Some("Ada Lovelace".into());
        let state = PreferenceState::build(vec![rated], &ranking);
        let mut candidate = article(2, &[99]);
        candidate.author = Some("  ADA   lovelace ".into());

        assert_eq!(state.examples[0].author.as_deref(), Some("ada lovelace"));
        assert_eq!(state.feed_credit(99), None);
        assert!((state.feed(&candidate).unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(state.author_credit(" ADA   Lovelace "), Some((1.0, 0.0)));
    }

    #[test]
    fn feed_affinity_means_rated_feed_and_rated_author() {
        let mut ranking = ranking();
        ranking.feed_floor = 0;
        ranking.feed_full = 1;
        let mut feed_loved = example(1, "loved", 1.0, &[1.0, 0.0]);
        feed_loved.feeds = vec![10];
        let mut author_down = example(2, "not_for_me", -1.0, &[1.0, 0.0]);
        author_down.feeds.clear();
        author_down.author = Some("writer".into());
        let state = PreferenceState::build(vec![feed_loved, author_down], &ranking);
        let mut candidate = article(3, &[10]);
        candidate.author = Some("Writer".into());

        assert!((state.feed(&candidate).unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn aggregator_only_without_author_keeps_feed_behavior_at_reduced_credit() {
        let mut ranking = ranking();
        ranking.feed_floor = 0;
        ranking.feed_full = 1;
        let mut rated = example(1, "not_for_me", -1.0, &[1.0, 0.0]);
        rated.feeds = vec![10];
        rated.aggregator_only = true;
        let state = PreferenceState::build(vec![rated], &ranking);

        assert_eq!(state.feed_credit(10), Some((0.0, 0.25)));
        assert_eq!(state.author_credit(""), None);
        assert!((state.feed(&article(2, &[10])).unwrap() - 1.0 / 2.25).abs() < 1e-9);
    }

    // --- §9.3 slop authors ---

    #[test]
    fn slop_authors_are_normalized_and_scale_the_preliminary_blend() {
        let state = PreferenceState::build(Vec::new(), &ranking()).with_slop_authors([
            "  Content   FARM ",
            "",
            "   ",
        ]);
        assert_eq!(state.slop_author_count(), 1);
        let mut reported = article(1, &[10]);
        reported.author = Some("content farm".into());
        let mut other = article(2, &[10]);
        other.author = Some("real writer".into());
        let anonymous = article(3, &[10]);
        assert!(state.is_slop_author(&reported));
        assert!(!state.is_slop_author(&other));
        assert!(!state.is_slop_author(&anonymous));

        let articles = vec![reported, other, anonymous];
        let computed = compute(
            &articles,
            &HashMap::new(),
            &HashMap::new(),
            &state,
            &ranking(),
        );
        let reported = &computed[&1];
        let other = &computed[&2];
        assert!(reported.slop_author);
        assert!(!other.slop_author);
        // Identical heuristic/social inputs: the only difference is the factor.
        assert!((reported.preliminary.unwrap() - other.preliminary.unwrap() * 0.25).abs() < 1e-9);
        assert!(
            reported
                .notes
                .iter()
                .any(|note| note.contains("author reported as AI slop"))
        );
        assert!(!other.notes.iter().any(|note| note.contains("AI slop")));
    }

    #[test]
    fn slop_factor_is_neutral_without_a_report_and_clamped_with_one() {
        let mut signals = Signals {
            slop_penalty: 0.75,
            ..Signals::default()
        };
        assert_eq!(signals.slop_factor(), 1.0);
        signals.slop_author = true;
        assert!((signals.slop_factor() - 0.25).abs() < 1e-9);
        signals.slop_penalty = 1.0;
        assert_eq!(signals.slop_factor(), 0.0);
        signals.slop_penalty = 0.0;
        assert_eq!(signals.slop_factor(), 1.0);
    }

    #[test]
    fn feed_is_absent_when_the_gate_is_closed() {
        let ranking = ranking(); // feed_floor 15
        let mut loved = example(1, "loved", 1.0, &[1.0, 0.0]);
        loved.feeds = vec![10];
        let state = PreferenceState::build(vec![loved], &ranking);
        assert_eq!(state.feed_gate, 0.0);
        assert_eq!(state.feed(&article(7, &[10])), None);
    }

    #[test]
    fn direct_feeds_fall_back_to_the_best_entry_feed() {
        let mut a = article(1, &[]);
        a.feed_id = 42;
        assert_eq!(direct_feeds(&a), vec![42]);
        assert_eq!(direct_feeds(&article(2, &[5, 3, 5])), vec![3, 5]);
    }

    // --- §12.2 normalization, §12.4 blend ---

    #[test]
    fn constant_signal_normalizes_to_half_for_everyone() {
        let mut values = [with_heuristic(Some(7.0)), with_heuristic(Some(7.0))];
        let mut refs = values.iter_mut().collect::<Vec<_>>();
        normalize(&mut refs);
        assert!(values.iter().all(|v| v.norm["heuristic"] == 0.5));
        let mut single = [with_heuristic(Some(3.0))];
        let mut refs = single.iter_mut().collect::<Vec<_>>();
        normalize(&mut refs);
        assert_eq!(single[0].norm["heuristic"], 0.5);
    }

    #[test]
    fn ties_get_equal_percentiles_without_an_id_ramp() {
        let mut values = (0..400)
            .map(|_| with_heuristic(Some(0.0)))
            .collect::<Vec<_>>();
        let mut refs = values.iter_mut().collect::<Vec<_>>();
        normalize(&mut refs);
        assert!(values.iter().all(|v| v.norm["heuristic"] == 0.5));

        let mut mixed = [
            with_heuristic(Some(1.0)),
            with_heuristic(Some(2.0)),
            with_heuristic(Some(2.0)),
            with_heuristic(Some(3.0)),
        ];
        let mut refs = mixed.iter_mut().collect::<Vec<_>>();
        normalize(&mut refs);
        assert_eq!(mixed[0].norm["heuristic"], 0.25);
        assert_eq!(mixed[1].norm["heuristic"], 0.625);
        assert_eq!(mixed[2].norm["heuristic"], 0.625);
        assert_eq!(mixed[3].norm["heuristic"], 1.0);
    }

    #[test]
    fn absent_values_do_not_shift_present_values() {
        let mut values = [
            with_heuristic(Some(1.0)),
            with_heuristic(Some(2.0)),
            with_heuristic(None),
        ];
        let mut refs = values.iter_mut().collect::<Vec<_>>();
        normalize(&mut refs);
        assert!(!values[2].norm.contains_key("heuristic"));
        // n_present = 2: the absent third value does not widen the scale.
        assert_eq!(values[0].norm["heuristic"], 0.5);
        assert_eq!(values[1].norm["heuristic"], 1.0);
    }

    #[test]
    fn effective_weights_sum_to_one_and_missing_signals_are_skipped() {
        let mut signals = Signals {
            interest: Some(1.0),
            heuristic: Some(2.0),
            norm: BTreeMap::from([("interest".into(), 0.8), ("heuristic".into(), 0.4)]),
            ..Signals::default()
        };
        let blend = preliminary_blend(&mut signals, &PreliminaryWeights::default()).unwrap();
        assert!((signals.weights.values().sum::<f64>() - 1.0).abs() < 1e-9);
        assert!(!signals.weights.contains_key("knn"));
        assert!(!signals.weights.contains_key("social"));
        // 0.35/0.55 × 0.8 + 0.20/0.55 × 0.4 = 0.6545…
        assert!((blend - 65.4545).abs() < 0.01, "{blend}");

        let mut only_heuristic = Signals {
            heuristic: Some(2.0),
            norm: BTreeMap::from([("heuristic".into(), 0.4)]),
            ..Signals::default()
        };
        let blend = preliminary_blend(&mut only_heuristic, &PreliminaryWeights::default());
        assert!((blend.unwrap() - 40.0).abs() < 1e-9);
        assert_eq!(only_heuristic.weights["heuristic"], 1.0);
    }

    #[test]
    fn learned_weights_are_multiplied_by_their_gate() {
        let mut signals = Signals {
            knn: Some(0.5),
            heuristic: Some(2.0),
            knn_gate: 0.5,
            norm: BTreeMap::from([("knn".into(), 1.0), ("heuristic".into(), 0.0)]),
            ..Signals::default()
        };
        preliminary_blend(&mut signals, &PreliminaryWeights::default());
        // knn 0.25 × 0.5 = 0.125 against heuristic 0.20.
        assert!((signals.weights["knn"] - 0.125 / 0.325).abs() < 1e-9);
    }

    #[test]
    fn compute_scores_every_article_and_leaves_ungated_signals_absent() {
        let articles = vec![article(1, &[10]), article(2, &[20]), article(3, &[30])];
        let mut embeddings = HashMap::new();
        embeddings.insert(1, unit(&[1.0, 0.0]));
        embeddings.insert(2, unit(&[0.0, 1.0]));
        let mut interests = HashMap::new();
        interests.insert("Axis".to_string(), unit(&[1.0, 0.0]));
        let state = PreferenceState::build(vec![example(9, "loved", 1.0, &[1.0, 0.0])], &ranking());
        let signals = compute(&articles, &embeddings, &interests, &state, &ranking());
        assert_eq!(signals.len(), 3);
        assert!(signals[&1].interest.is_some());
        assert!(signals[&3].interest.is_none(), "no embedding → absent");
        assert!(
            signals
                .values()
                .all(|s| s.knn.is_none() && s.feed.is_none())
        );
        assert!(
            signals
                .values()
                .all(|s| s.heuristic.is_some() && s.preliminary.is_some())
        );
    }
}
