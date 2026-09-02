//! Bisecting batch runner shared by triage and deep assessment.
//!
//! DeepSeek's input content filter rejects a whole request when any one
//! article in the batch trips it (`400 Content Exists Risk`) and does not say
//! which. Sending the batch once and giving up cost every other article its
//! assessment and retried them all the next day. Instead, a batch that fails
//! with a non-transient error is split in half and both halves are sent
//! again, down to single articles. A single article that still fails is
//! "rejected": it is retried once on the editor client when that runs on a
//! different provider, and otherwise reported so the caller can persist a
//! `provider_rejected` row and stop asking (§10, §12.1, §17).
//!
//! Transient errors are retried inside [`LlmClient::complete`] and, once
//! exhausted, leave the batch unassessed as before; a budget trip stops the
//! stage. Concurrency is the caller's `max_concurrent_requests` across the
//! original batches; the bisection inside one batch runs sequentially.

use std::collections::HashSet;

use futures::{StreamExt, stream};

use super::llm::{LlmClient, LlmError};
use crate::types::{ArticleId, Candidate};

/// Longest rejection message kept in `article_assessments.rationale`.
pub const REJECTION_MESSAGE_CHARS: usize = 200;

/// A parsed per-article item the runner can attribute to a candidate.
pub trait Assessed {
    fn article_id(&self) -> ArticleId;
}

/// One stage's prompt builder, parser and clients.
pub struct BatchRunner<'a, T> {
    /// The bulk client every batch goes to first.
    pub llm: &'a LlmClient,
    /// The editor client, tried once per rejected article when it runs on a
    /// different provider than `llm` and its meter is not tripped.
    pub fallback: Option<&'a LlmClient>,
    pub temperature: f32,
    pub build_prompt: &'a (dyn Fn(&[&Candidate]) -> String + Sync),
    pub parse: &'a (dyn Fn(&str) -> Vec<T> + Sync),
}

impl<T> BatchRunner<'_, T> {
    /// The editor client when it is a real alternative to the bulk one.
    pub fn usable_fallback(&self) -> Option<&LlmClient> {
        self.fallback
            .filter(|fallback| fallback.provider() != self.llm.provider())
            .filter(|fallback| fallback.meter.check_budget().is_ok())
    }

    /// The name of the provider rejected articles are retried on, if any.
    pub fn fallback_provider(&self) -> Option<String> {
        self.fallback
            .filter(|fallback| fallback.provider() != self.llm.provider())
            .map(|fallback| fallback.provider().to_string())
    }
}

/// An item together with the model that produced it (bulk or editor).
#[derive(Debug, Clone, PartialEq)]
pub struct Scored<T> {
    pub item: T,
    pub model: String,
}

/// A single article both the bulk provider and the fallback refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub id: ArticleId,
    /// The bulk provider's name.
    pub provider: String,
    /// The bulk provider's message, at most [`REJECTION_MESSAGE_CHARS`] long.
    pub message: String,
}

impl Rejection {
    /// `<provider>: <message>`, the `rationale` of a `provider_rejected` row.
    pub fn rationale(&self) -> String {
        format!("{}: {}", self.provider, self.message)
    }
}

/// What a set of batches produced.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchOutcome<T> {
    pub items: Vec<Scored<T>>,
    /// Articles nobody would assess; the caller persists these.
    pub rejections: Vec<Rejection>,
    /// Single articles the bulk provider rejected, recovered or not.
    pub rejected: usize,
    /// Of `rejected`, those the fallback client assessed.
    pub recovered: usize,
    /// Requests made on the bulk client.
    pub requests: usize,
    /// True when the bulk budget tripped and work was left undone.
    pub budget_stopped: bool,
}

impl<T> Default for BatchOutcome<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            rejections: Vec::new(),
            rejected: 0,
            recovered: 0,
            requests: 0,
            budget_stopped: false,
        }
    }
}

impl<T> BatchOutcome<T> {
    fn absorb(&mut self, other: Self) {
        self.items.extend(other.items);
        self.rejections.extend(other.rejections);
        self.rejected += other.rejected;
        self.recovered += other.recovered;
        self.requests += other.requests;
        self.budget_stopped |= other.budget_stopped;
    }
}

/// The per-stage totals behind the `triage:` / `assess:` log line and the
/// run report's `*_reused` / `*_rejected` counts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StageSummary {
    pub stage: &'static str,
    /// Articles the stage was responsible for this run.
    pub pool: usize,
    /// Served from cached rows within `assessment_reuse_days`.
    pub reused: usize,
    /// Skipped because a `provider_rejected` row is still fresh.
    pub known_rejected: usize,
    /// Sent to the bulk provider.
    pub requested: usize,
    /// Original batches before any bisection.
    pub batches: usize,
    /// Assessments written this run (bulk and editor).
    pub applied: usize,
    /// Single articles the bulk provider rejected this run.
    pub rejected: usize,
    /// Of `rejected`, those the editor client assessed instead.
    pub recovered: usize,
    pub fallback_provider: Option<String>,
}

impl StageSummary {
    /// Articles carrying this stage's assessment after the run.
    pub fn assessed(&self) -> usize {
        self.reused + self.applied
    }

    /// Articles without an assessment because a provider rejected them:
    /// this run's unrecovered rejections plus the cached ones.
    pub fn rejected_total(&self) -> usize {
        self.known_rejected + self.rejected.saturating_sub(self.recovered)
    }

    /// `triage: 398 in pool · 210 reused · 188 requested in 8 batches · 3 rejected (2 recovered on gemini)`
    pub fn info_line(&self) -> String {
        let mut line = format!(
            "{}: {} in pool · {} reused · {} requested in {} batches · {} rejected",
            self.stage, self.pool, self.reused, self.requested, self.batches, self.rejected
        );
        if self.recovered > 0 {
            line.push_str(&format!(
                " ({} recovered on {})",
                self.recovered,
                self.fallback_provider.as_deref().unwrap_or("editor")
            ));
        }
        if self.known_rejected > 0 {
            line.push_str(&format!(" · {} known rejected", self.known_rejected));
        }
        line
    }
}

/// Errors the provider will keep returning for the same input.
fn is_rejection(error: &LlmError) -> bool {
    matches!(
        error,
        LlmError::Api { .. } | LlmError::Refusal { .. } | LlmError::EmptyResponse { .. }
    )
}

/// The provider's own words, without the `<provider> request failed:` prefix.
fn rejection_message(error: &LlmError) -> String {
    match error {
        LlmError::Api { message, .. } => message.clone(),
        LlmError::Refusal { .. } => "returned a refusal".into(),
        LlmError::EmptyResponse { .. } => "returned an empty completion".into(),
        other => other.to_string(),
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    let mut cut = collapsed
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    cut.push('…');
    cut
}

enum Failure {
    Rejected(String),
    NoItems,
}

struct Work<'c> {
    batch: Vec<&'c Candidate>,
    /// A batch that parses to zero items is split once; its halves are not.
    zero_split: bool,
}

/// Run every batch on the bulk client, `max_concurrent` in flight.
pub async fn run_batches<T: Assessed>(
    runner: &BatchRunner<'_, T>,
    batches: Vec<Vec<&Candidate>>,
    max_concurrent: usize,
) -> BatchOutcome<T> {
    let outcomes = stream::iter(batches)
        .map(|batch| run_one(runner, batch))
        .buffer_unordered(max_concurrent.max(1))
        .collect::<Vec<_>>()
        .await;
    let mut merged = BatchOutcome::default();
    for outcome in outcomes {
        merged.absorb(outcome);
    }
    merged
}

/// One original batch, bisected as far as it needs to be.
async fn run_one<T: Assessed>(
    runner: &BatchRunner<'_, T>,
    batch: Vec<&Candidate>,
) -> BatchOutcome<T> {
    let original = batch.len();
    let provider = runner.llm.provider();
    let mut out = BatchOutcome::default();
    let mut warned = false;
    // Explicit LIFO stack instead of recursion: the left half is pushed last
    // so it runs first, which keeps the request order predictable.
    let mut stack = vec![Work {
        batch,
        zero_split: true,
    }];
    while let Some(work) = stack.pop() {
        if let Err(error) = runner.llm.meter.check_budget() {
            tracing::warn!(%error, "bulk budget tripped; skipping the rest of the batch");
            out.budget_stopped = true;
            return out;
        }
        let size = work.batch.len();
        let prompt = (runner.build_prompt)(&work.batch);
        let allowed = work
            .batch
            .iter()
            .map(|candidate| candidate.article.id)
            .collect::<HashSet<_>>();
        out.requests += 1;
        let failure = match runner.llm.complete(&prompt, runner.temperature, true).await {
            Ok(raw) => {
                let items = (runner.parse)(&raw)
                    .into_iter()
                    .filter(|item| allowed.contains(&item.article_id()))
                    .collect::<Vec<_>>();
                if items.is_empty() {
                    Failure::NoItems
                } else {
                    out.items.extend(items.into_iter().map(|item| Scored {
                        item,
                        model: runner.llm.model.clone(),
                    }));
                    continue;
                }
            }
            Err(LlmError::BudgetExceeded { spent, limit }) => {
                tracing::warn!(
                    spent,
                    limit,
                    "bulk budget tripped; skipping the rest of the batch"
                );
                out.budget_stopped = true;
                return out;
            }
            Err(error) if is_rejection(&error) => Failure::Rejected(rejection_message(&error)),
            Err(error) => {
                tracing::warn!(%error, size, "batch failed; its articles remain unassessed");
                continue;
            }
        };
        if size == 1 {
            let candidate = work.batch[0];
            let message = match failure {
                Failure::Rejected(message) => message,
                Failure::NoItems => "returned no assessment for the article".into(),
            };
            resolve_single(runner, candidate, &prompt, message, &mut out).await;
            continue;
        }
        match failure {
            Failure::Rejected(message) => {
                if !warned {
                    tracing::warn!(
                        provider,
                        message = %truncate_chars(&message, REJECTION_MESSAGE_CHARS),
                        "batch of {original} rejected by {provider}; bisecting"
                    );
                    warned = true;
                }
                push_halves(&mut stack, work.batch, work.zero_split);
            }
            Failure::NoItems if work.zero_split => {
                tracing::warn!(provider, size, "batch parsed to zero items; bisecting once");
                push_halves(&mut stack, work.batch, false);
            }
            Failure::NoItems => {
                tracing::warn!(
                    provider,
                    size,
                    "half batch parsed to zero items again; its articles remain unassessed"
                );
            }
        }
    }
    out
}

fn push_halves<'c>(stack: &mut Vec<Work<'c>>, batch: Vec<&'c Candidate>, zero_split: bool) {
    let mut left = batch;
    let right = left.split_off(left.len() / 2);
    stack.push(Work {
        batch: right,
        zero_split,
    });
    stack.push(Work {
        batch: left,
        zero_split,
    });
}

/// A single article the bulk provider would not assess: try the editor
/// client once, else report it as rejected.
async fn resolve_single<T: Assessed>(
    runner: &BatchRunner<'_, T>,
    candidate: &Candidate,
    prompt: &str,
    message: String,
    out: &mut BatchOutcome<T>,
) {
    let id = candidate.article.id;
    let provider = runner.llm.provider();
    out.rejected += 1;
    if let Some(fallback) = runner.usable_fallback() {
        let fallback_provider = fallback.provider();
        match fallback.complete(prompt, runner.temperature, true).await {
            Ok(raw) => {
                if let Some(item) = (runner.parse)(&raw)
                    .into_iter()
                    .find(|item| item.article_id() == id)
                {
                    tracing::info!(
                        article_id = id,
                        provider = fallback_provider,
                        "article rejected by {provider}; assessed on {fallback_provider} instead"
                    );
                    out.items.push(Scored {
                        item,
                        model: fallback.model.clone(),
                    });
                    out.recovered += 1;
                    return;
                }
                tracing::warn!(
                    article_id = id,
                    provider = fallback_provider,
                    "fallback returned no assessment for the rejected article"
                );
            }
            Err(error) => {
                tracing::warn!(
                    article_id = id,
                    provider = fallback_provider,
                    %error,
                    "fallback failed for the rejected article"
                );
            }
        }
    }
    let message = truncate_chars(&message, REJECTION_MESSAGE_CHARS);
    tracing::warn!(
        article_id = id,
        provider,
        %message,
        "article rejected by the provider; recorded so it is not retried"
    );
    out.rejections.push(Rejection {
        id,
        provider: provider.to_string(),
        message,
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::ProviderConfig;
    use crate::curate::llm::{
        ChatBackend, ChatCompletion, ChatRequest, MockBackend, PriceTable, UsageMeter,
    };
    use crate::curate::prefilter::tests::article;
    use crate::curate::triage::{TriageItem, build_batch_prompt, parse_triage_response};
    use crate::http::RetryPolicy;
    use crate::types::TokenUsage;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// A backend that answers for every id it finds in the prompt unless the
    /// prompt mentions a forbidden title, in which case it rejects the whole
    /// request the way DeepSeek's content filter does.
    #[derive(Debug)]
    pub(crate) struct FilterBackend {
        forbidden: Vec<String>,
        /// Prompts answered with `{"articles": []}` (by request index).
        empty_on: Vec<usize>,
        answer: fn(&[i64]) -> String,
        seen: Mutex<Vec<String>>,
    }

    impl FilterBackend {
        /// Answers in the triage shape.
        pub(crate) fn new(forbidden: &[&str], empty_on: &[usize]) -> Arc<Self> {
            Self::with_answer(forbidden, empty_on, answer_for)
        }

        /// Answers in the deep-assessment shape.
        pub(crate) fn deep(forbidden: &[&str]) -> Arc<Self> {
            Self::with_answer(forbidden, &[], deep_answer_for)
        }

        fn with_answer(
            forbidden: &[&str],
            empty_on: &[usize],
            answer: fn(&[i64]) -> String,
        ) -> Arc<Self> {
            Arc::new(Self {
                forbidden: forbidden.iter().map(|s| s.to_string()).collect(),
                empty_on: empty_on.to_vec(),
                answer,
                seen: Mutex::new(Vec::new()),
            })
        }

        pub(crate) fn calls(&self) -> usize {
            self.seen.lock().map(|seen| seen.len()).unwrap_or(0)
        }

        /// The prompt of the first request that carried only `id`.
        pub(crate) fn prompts_for_single(&self, id: i64) -> String {
            self.seen
                .lock()
                .ok()
                .and_then(|seen| {
                    seen.iter()
                        .find(|prompt| ids_in(prompt) == vec![id])
                        .cloned()
                })
                .unwrap_or_default()
        }

        /// The ids in each request, in order.
        pub(crate) fn requests(&self) -> Vec<Vec<i64>> {
            self.seen
                .lock()
                .map(|seen| seen.iter().map(|prompt| ids_in(prompt)).collect())
                .unwrap_or_default()
        }
    }

    fn ids_in(prompt: &str) -> Vec<i64> {
        prompt
            .lines()
            .filter_map(|line| line.strip_prefix("--- id: "))
            .filter_map(|id| id.trim().parse().ok())
            .collect()
    }

    /// A triage answer for `ids`.
    pub(crate) fn answer_for(ids: &[i64]) -> String {
        let items = ids
            .iter()
            .map(|id| format!(r#"{{"id":{id},"interest":6,"kind":"essay","why":"fine"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"articles":[{items}]}}"#)
    }

    /// A deep-assessment answer for `ids`.
    pub(crate) fn deep_answer_for(ids: &[i64]) -> String {
        let items = ids
            .iter()
            .map(|id| {
                format!(
                    r#"{{"id":{id},"quality":7,"fit":6,"category":"Top Stories","rationale":"fine","facets":{{"format":"analysis_essay"}}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"articles":[{items}]}}"#)
    }

    impl ChatBackend for FilterBackend {
        fn complete<'a>(
            &'a self,
            req: ChatRequest,
        ) -> Pin<Box<dyn Future<Output = Result<ChatCompletion, LlmError>> + Send + 'a>> {
            Box::pin(async move {
                let index = self.calls();
                if let Ok(mut seen) = self.seen.lock() {
                    seen.push(req.user.clone());
                }
                if self.forbidden.iter().any(|title| req.user.contains(title)) {
                    return Err(LlmError::api(
                        "deepseek",
                        "400 Bad Request: {\"error\":{\"message\":\"Content Exists Risk\"}}",
                    ));
                }
                let content = if self.empty_on.contains(&index) {
                    r#"{"articles":[]}"#.to_string()
                } else {
                    (self.answer)(&ids_in(&req.user))
                };
                Ok(ChatCompletion {
                    content,
                    usage: TokenUsage::default(),
                })
            })
        }
    }

    fn client(provider: &str, model: &str, backend: Arc<dyn ChatBackend>) -> LlmClient {
        LlmClient::with_backend_options(
            provider,
            model,
            "SYSTEM".into(),
            None,
            UsageMeter::with_prices(PriceTable::from(&ProviderConfig::deepseek()), 10.0),
            backend,
        )
    }

    fn candidates(n: i64) -> Vec<Candidate> {
        (1..=n)
            .map(|id| Candidate::new(article(id, &format!("Title number {id}"), 800), false))
            .collect()
    }

    async fn run(
        llm: &LlmClient,
        fallback: Option<&LlmClient>,
        candidates: &[Candidate],
        batch_size: usize,
    ) -> BatchOutcome<TriageItem> {
        let runner = BatchRunner {
            llm,
            fallback,
            temperature: 0.3,
            build_prompt: &build_batch_prompt,
            parse: &parse_triage_response,
        };
        let all = candidates.iter().collect::<Vec<_>>();
        let batches = all.chunks(batch_size).map(<[&Candidate]>::to_vec).collect();
        run_batches(&runner, batches, 4).await
    }

    fn sorted_ids(outcome: &BatchOutcome<TriageItem>) -> Vec<i64> {
        let mut ids = outcome
            .items
            .iter()
            .map(|scored| scored.item.id)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    #[tokio::test]
    async fn one_bad_article_in_four_costs_five_requests_and_one_rejection() {
        let backend = FilterBackend::new(&["Title number 3"], &[]);
        let llm = client("deepseek", "deepseek-v4-flash", backend.clone());
        let candidates = candidates(4);
        let outcome = run(&llm, None, &candidates, 4).await;

        // [1,2,3,4] is rejected → split. [1,2] succeeds. [3,4] is rejected →
        // split. [3] alone is rejected → recorded. [4] succeeds. 1 + 2 + 2 = 5.
        assert_eq!(
            backend.requests(),
            vec![vec![1, 2, 3, 4], vec![1, 2], vec![3, 4], vec![3], vec![4]]
        );
        assert_eq!(outcome.requests, 5);
        assert_eq!(sorted_ids(&outcome), vec![1, 2, 4]);
        assert!(outcome.items.iter().all(|s| s.model == "deepseek-v4-flash"));
        assert_eq!(outcome.rejected, 1);
        assert_eq!(outcome.recovered, 0);
        assert_eq!(outcome.rejections.len(), 1);
        let rejection = &outcome.rejections[0];
        assert_eq!(rejection.id, 3);
        assert_eq!(rejection.provider, "deepseek");
        assert!(rejection.message.contains("Content Exists Risk"));
        assert!(
            rejection
                .rationale()
                .starts_with("deepseek: 400 Bad Request"),
            "{}",
            rejection.rationale()
        );
        assert!(!outcome.budget_stopped);
    }

    #[tokio::test]
    async fn every_article_rejected_gives_every_article_a_rejection_in_2n_minus_1_requests() {
        let titles = (1..=8)
            .map(|id| format!("Title number {id}"))
            .collect::<Vec<_>>();
        let backend =
            FilterBackend::new(&titles.iter().map(String::as_str).collect::<Vec<_>>(), &[]);
        let llm = client("deepseek", "deepseek-v4-flash", backend.clone());
        let candidates = candidates(8);
        let outcome = run(&llm, None, &candidates, 8).await;
        assert_eq!(backend.calls(), 2 * 8 - 1);
        assert_eq!(outcome.requests, 15);
        assert!(outcome.items.is_empty());
        let mut rejected = outcome
            .rejections
            .iter()
            .map(|rejection| rejection.id)
            .collect::<Vec<_>>();
        rejected.sort_unstable();
        assert_eq!(rejected, (1..=8).collect::<Vec<_>>());
        assert_eq!(outcome.rejected, 8);
    }

    #[tokio::test]
    async fn a_rejected_article_is_recovered_on_an_editor_of_another_provider() {
        let backend = FilterBackend::new(&["Title number 2"], &[]);
        let llm = client("deepseek", "deepseek-v4-flash", backend.clone());
        let editor_backend = Arc::new(MockBackend::new());
        editor_backend.push(answer_for(&[2]), TokenUsage::default());
        let editor = client("gemini", "gemini-3.8-flash", editor_backend.clone());
        let candidates = candidates(2);
        let outcome = run(&llm, Some(&editor), &candidates, 2).await;
        assert_eq!(backend.requests(), vec![vec![1, 2], vec![1], vec![2]]);
        assert_eq!(editor_backend.calls(), 1);
        let prompt = editor_backend.prompts()[0].clone();
        assert_eq!(prompt.system.as_str(), "SYSTEM", "same system prompt");
        assert!(prompt.user.contains("--- id: 2\n"), "same article prompt");
        assert_eq!(sorted_ids(&outcome), vec![1, 2]);
        let recovered = outcome
            .items
            .iter()
            .find(|scored| scored.item.id == 2)
            .expect("recovered");
        assert_eq!(recovered.model, "gemini-3.8-flash");
        assert!(outcome.rejections.is_empty(), "no rejection row");
        assert_eq!((outcome.rejected, outcome.recovered), (1, 1));

        // The same provider name is no alternative: no fallback attempt.
        let backend = FilterBackend::new(&["Title number 2"], &[]);
        let llm = client("deepseek", "deepseek-v4-flash", backend.clone());
        let same_backend = Arc::new(MockBackend::new());
        same_backend.push(answer_for(&[2]), TokenUsage::default());
        let same = client("deepseek", "deepseek-v4-flash", same_backend.clone());
        let outcome = run(&llm, Some(&same), &candidates, 2).await;
        assert_eq!(same_backend.calls(), 0);
        assert_eq!(outcome.rejections.len(), 1);
        assert_eq!(outcome.recovered, 0);

        // A tripped editor meter is skipped too, and a failing editor still
        // yields a rejection with the bulk provider's message.
        let backend = FilterBackend::new(&["Title number 2"], &[]);
        let llm = client("deepseek", "deepseek-v4-flash", backend.clone());
        let tripped_backend = Arc::new(MockBackend::new());
        let tripped = client("gemini", "gemini-3.8-flash", tripped_backend.clone());
        tripped.meter.preload_cost(100.0);
        let outcome = run(&llm, Some(&tripped), &candidates, 2).await;
        assert_eq!(tripped_backend.calls(), 0);
        assert_eq!(outcome.rejections.len(), 1);

        let backend = FilterBackend::new(&["Title number 2"], &[]);
        let llm = client("deepseek", "deepseek-v4-flash", backend.clone());
        let failing_backend = Arc::new(MockBackend::new());
        failing_backend.push_llm_error(LlmError::refusal("gemini"));
        let failing = client("gemini", "gemini-3.8-flash", failing_backend.clone());
        let outcome = run(&llm, Some(&failing), &candidates, 2).await;
        assert_eq!(failing_backend.calls(), 1);
        assert_eq!(outcome.rejections.len(), 1);
        assert_eq!(outcome.rejections[0].provider, "deepseek");
        assert!(
            outcome.rejections[0]
                .message
                .contains("Content Exists Risk")
        );
    }

    #[tokio::test]
    async fn zero_items_bisect_once_and_a_single_zero_is_a_rejection() {
        // Batch of 4 parses to zero items → its two halves are sent; both fine.
        let backend = FilterBackend::new(&[], &[0]);
        let llm = client("deepseek", "deepseek-v4-flash", backend.clone());
        let candidates = candidates(4);
        let outcome = run(&llm, None, &candidates, 4).await;
        assert_eq!(
            backend.requests(),
            vec![vec![1, 2, 3, 4], vec![1, 2], vec![3, 4]]
        );
        assert_eq!(sorted_ids(&outcome), vec![1, 2, 3, 4]);
        assert!(outcome.rejections.is_empty());

        // A half that parses to zero items again is not split further: its
        // articles simply stay unassessed for this run.
        let backend = FilterBackend::new(&[], &[0, 1]);
        let llm = client("deepseek", "deepseek-v4-flash", backend.clone());
        let outcome = run(&llm, None, &candidates, 4).await;
        assert_eq!(backend.calls(), 3);
        assert_eq!(sorted_ids(&outcome), vec![3, 4]);
        assert!(outcome.rejections.is_empty());

        // A single article that parses to zero items is a rejection.
        let backend = FilterBackend::new(&[], &[0]);
        let llm = client("deepseek", "deepseek-v4-flash", backend.clone());
        let outcome = run(&llm, None, &candidates[..1], 1).await;
        assert_eq!(backend.calls(), 1);
        assert!(outcome.items.is_empty());
        assert_eq!(outcome.rejections.len(), 1);
        assert_eq!(
            outcome.rejections[0].rationale(),
            "deepseek: returned no assessment for the article"
        );
    }

    #[tokio::test]
    async fn transient_and_budget_failures_are_not_bisected() {
        let backend = Arc::new(MockBackend::new());
        for _ in 0..3 {
            backend.push_llm_error(LlmError::Transient {
                provider: "deepseek".into(),
                message: "503".into(),
            });
        }
        let llm =
            client("deepseek", "deepseek-v4-flash", backend.clone()).with_retry(RetryPolicy {
                max_attempts: 3,
                base_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(2),
            });
        let candidates = candidates(4);
        // Transient errors are retried inside the client and, once exhausted,
        // leave the batch unassessed without bisecting it.
        let outcome = run(&llm, None, &candidates, 4).await;
        assert_eq!(backend.calls(), 3, "three attempts, one batch");
        assert!(outcome.items.is_empty());
        assert!(
            outcome.rejections.is_empty(),
            "transient is not a rejection"
        );
        assert_eq!(outcome.rejected, 0);

        let backend = Arc::new(MockBackend::new());
        let llm = client("deepseek", "deepseek-v4-flash", backend.clone());
        llm.meter.preload_cost(100.0);
        let outcome = run(&llm, None, &candidates, 2).await;
        assert_eq!(backend.calls(), 0);
        assert!(outcome.budget_stopped);
    }

    #[test]
    fn summary_line_matches_the_documented_shape() {
        let summary = StageSummary {
            stage: "triage",
            pool: 398,
            reused: 210,
            known_rejected: 0,
            requested: 188,
            batches: 8,
            applied: 187,
            rejected: 3,
            recovered: 2,
            fallback_provider: Some("gemini".into()),
        };
        assert_eq!(
            summary.info_line(),
            "triage: 398 in pool · 210 reused · 188 requested in 8 batches · 3 rejected (2 recovered on gemini)"
        );
        assert_eq!(summary.assessed(), 397);
        assert_eq!(summary.rejected_total(), 1);
        let quiet = StageSummary {
            stage: "assess",
            pool: 120,
            reused: 20,
            known_rejected: 2,
            requested: 98,
            batches: 13,
            applied: 98,
            ..StageSummary::default()
        };
        assert_eq!(
            quiet.info_line(),
            "assess: 120 in pool · 20 reused · 98 requested in 13 batches · 0 rejected · 2 known rejected"
        );
        assert_eq!(quiet.rejected_total(), 2);
    }

    #[test]
    fn rejection_messages_are_short_and_single_line() {
        let long = "x".repeat(500);
        let cut = truncate_chars(&long, REJECTION_MESSAGE_CHARS);
        assert_eq!(cut.chars().count(), REJECTION_MESSAGE_CHARS);
        assert!(cut.ends_with('…'));
        assert_eq!(truncate_chars("a\n  b", 10), "a b");
        assert_eq!(
            rejection_message(&LlmError::refusal("anthropic")),
            "returned a refusal"
        );
        assert_eq!(
            rejection_message(&LlmError::api("deepseek", "400: nope")),
            "400: nope"
        );
    }
}
