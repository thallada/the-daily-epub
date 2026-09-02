# Personalized Ranking, Embeddings, Facets, and Feedback — Implementation Plan

**Date:** 2026-08-17  
**Repository:** `thallada/the-daily-epub`  
**Status:** implementation plan  
**Scope:** replace the current mostly heuristic candidate funnel with a high-recall, semantically personalized, facet-aware ranking pipeline while preserving the LLM as the final editor.

This plan is intentionally implementation-grade. An implementation agent should be able to execute it without rediscovering the current architecture or making major product decisions. Read these existing documents first:

- `docs/plans/2026-08-15-the-daily-epub.md` — original system design.
- `docs/plans/2026-08-15-implementation-notes.md` — implementation conventions and verified environment facts.

Also read the current curation implementation before changing it:

- `src/pipeline.rs`
- `src/curate/mod.rs`
- `src/curate/prefilter.rs`
- `src/curate/score.rs`
- `src/curate/select.rs`
- `src/curate/profile.rs`
- `src/curate/llm.rs`
- `src/types.rs`
- `src/db.rs`
- `src/config.rs`
- `src/main.rs`
- `migrations/0001_init.sql`

---

## 1. Why this change is needed

The current curation pipeline is:

```text
~400 daily articles
  -> deterministic heuristic prefilter (~120)
  -> DeepSeek Stage A scores those ~120
  -> combined_score() ranks them
  -> top ~40 are shown to DeepSeek Stage B
  -> Stage B chooses ~20 and arranges the issue
```

The last two stages are reasonably personalized, but the first irreversible cut is not. `prefilter.rs` currently decides which articles are allowed to reach the personalized LLM using mostly:

- word count,
- HN/Reddit/Lobsters social proof,
- Scour/HN provenance,
- number of feeds carrying the story,
- a per-feed rating prior,
- excerpt/paywall status,
- title-pattern penalties,
- hard block/always-include rules.

This creates several problems:

1. **Personalization happens too late.** A personally ideal article can be dropped before the reader profile, semantic interests, or learned rating patterns are considered.
2. **Correlated signals are counted repeatedly.** Social proof, long-form bias, and discovery-source provenance influence multiple stages independently.
3. **Ratings are coarse.** The immediate feedback loop mostly learns “this feed tends to be liked,” which cannot distinguish different article types from the same publication.
4. **The LLM sees too little article text.** Stage A currently receives only the first ~200 words, and Stage B only a ~45-word opening plus Stage A's rationale.
5. **The final shortlist can already be homogeneous.** Diversity is mostly delegated to Stage B after a top-40 score cut.
6. **The code contradicts its own editorial philosophy.** The profile says a small issue of excellent pieces is preferable to padding, but `select.rs` currently tops a short lineup back up to the hard minimum.
7. **There is no explicit exploration mechanism.** A source/topic that never survives the current funnel cannot generate the ratings needed to improve its odds.
8. **There is insufficient persisted ranking telemetry to replay historical days and tune the model empirically.**

The desired architecture is:

```text
all daily feed entries
  -> hard hygiene + dedupe + extraction + social
  -> embeddings for all articles
  -> high-recall union based on heuristic + semantic interests + learned taste + exploration
  -> structured facet extraction on the recall pool
  -> personalized pre-Stage-A ranking
  -> Stage A quality/fit scoring using representative article samples
  -> final utility score
  -> embedding-based diversified shortlist (MMR)
  -> Stage B LLM editor chooses the issue
  -> no forced filler
  -> ratings immediately update embedding/facet/source preference models
```

The core principle is: **heuristics may cheaply propose candidates, but they must no longer decide what the personalized system is allowed to see.**

---

## 2. Product goals and non-goals

### Goals

1. Increase recall of articles that closely match the reader's interests or learned taste even when they are short, quiet, or from obscure feeds.
2. Learn preferences at the article-feature level rather than primarily at the feed level.
3. Preserve topic semantic similarity while separately modeling non-topic preferences such as format, depth, technicality, tone, stance, and evidence style.
4. Make the final shortlist diverse before it reaches the LLM editor.
5. Give the LLM better evidence about article quality by sampling the beginning, middle, and end.
6. Keep the service robust: missing Voyage/DeepSeek keys or API failures must degrade to the existing heuristic behavior rather than prevent an issue.
7. Keep all ranking decisions explainable and replayable from persisted per-run features.
8. Keep infrastructure simple. At the expected scale, SQLite plus in-process dot products is sufficient; do not add a vector database.
9. Make the new system tunable through configuration and offline evaluation rather than burying another generation of hard-coded weights in code.

### Non-goals

- Do not train a custom neural recommender in this iteration.
- Do not add collaborative filtering; this is a single-reader system.
- Do not treat the absence of a rating as a downvote.
- Do not infer political ideology or sensitive personal attributes from article content.
- Do not remove the existing weekly natural-language taste-profile mechanism; make it less authoritative and complement it with quantitative learning.
- Do not replace DeepSeek Stage B. The final LLM editorial pass is useful and should remain.
- Do not introduce Qdrant, pgvector, Elasticsearch, or another service solely for a few hundred vectors/day.

---

## 3. Verified Voyage AI facts and chosen defaults

Use **Voyage AI `voyage-4-lite`** for article and interest embeddings.

Verified against Voyage AI's official documentation on 2026-08-17:

- REST endpoint: `POST https://api.voyageai.com/v1/embeddings`
- Authentication: `Authorization: Bearer <API key>`
- Model: `voyage-4-lite`
- Context length: 32,000 tokens per input.
- Supported dimensions: 256, 512, **1024 default**, 2048.
- The embeddings endpoint accepts at most 1,000 inputs/request and, for `voyage-4-lite`, at most 1M input tokens/request.
- `input_type` supports `query` and `document` and should be used for retrieval-style comparisons.
- Voyage embeddings are unit-normalized, so dot product and cosine similarity are equivalent.
- Current published pricing is $0.02 / 1M tokens after the model's free allocation; the first 200M text-embedding tokens are currently free per account. Pricing is operational metadata and must stay configurable rather than assumed forever.

Official references:

- <https://docs.voyageai.com/reference/embeddings-api>
- <https://docs.voyageai.com/docs/embeddings>
- <https://docs.voyageai.com/docs/faq>
- <https://docs.voyageai.com/docs/pricing>

### V1 choices

Use these defaults unless offline evaluation demonstrates a reason to change them:

```toml
[voyage]
enabled = true
base_url = "https://api.voyageai.com/v1"
model = "voyage-4-lite"
output_dimension = 1024
batch_size = 32
max_input_chars_per_article = 60000
price_per_mtok = 0.02
max_daily_usd = 0.25
```

The API key must come only from:

```text
DAILY_EPUB_VOYAGE__API_KEY
```

Never add an API key to `config.toml`, `config.example.toml`, tests, fixtures, logs, run reports, or the database.

Use `output_dtype = "float"` initially. A 1024-dimensional f32 vector is only 4096 bytes before SQLite overhead; brute-force dot products across hundreds or a few thousand vectors are trivial. Do not optimize storage with int8/binary quantization until there is measured pressure. `output_dimension` must still be configurable so 512 can be evaluated later.

Use the REST API directly through `reqwest`; do not add a Python runtime or a Voyage SDK dependency.

---

## 4. New curation pipeline

The target pipeline should become:

```text
1. Miniflux ingest
2. normalize/dedupe
3. content extraction
4. persist articles
5. social enrichment
6. feature preparation
   6a. cache/generate article embeddings for all eligible articles
   6b. load/build interest query embeddings
   6c. build quantitative preference state from ratings
7. high-recall candidate construction (~400 -> configurable ~240)
8. facet extraction on recall pool (~240)
9. personalized pre-Stage-A ranking (~240 -> ~120)
10. DeepSeek Stage A quality + reader-fit scoring (~120)
11. final utility scoring
12. MMR/diversified shortlist (~120 -> ~60)
13. DeepSeek Stage B final editorial selection (soft target ~20, max ~25; no hard minimum)
14. comments/world/editorial/EPUB/publish as today
```

Existing `prefilter.rs` should be refactored rather than deleted. It still owns cheap quality/hygiene signals, but its score becomes one input to recall and utility instead of the sole top-120 gate.

---

## 5. Data model and migrations

Create a new migration, e.g. `migrations/0002_personalized_ranking.sql`. Do not edit `0001_init.sql` for an already deployed database.

### 5.1 `article_embeddings`

Persist embeddings independently of daily runs so they are reused in future preference calculations and historical evaluation.

```sql
CREATE TABLE article_embeddings (
    article_id      INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    model           TEXT NOT NULL,
    dimension       INTEGER NOT NULL,
    input_hash      TEXT NOT NULL,
    embedding       BLOB NOT NULL,
    input_tokens    INTEGER,
    created_at      TEXT NOT NULL,
    PRIMARY KEY (article_id, model, dimension)
);

CREATE INDEX idx_article_embeddings_model
    ON article_embeddings(model, dimension);
```

Requirements:

- Store f32 values as a compact little-endian BLOB. Add explicit encode/decode helpers and test round trips.
- Verify decoded byte length is exactly `dimension * 4`; corrupted rows must be ignored with a warning, not panic.
- `input_hash` is SHA-256 over the exact normalized text sent to Voyage plus an embedding-input-format version. If extraction/content changes, regenerate the vector.
- The configured model and dimension are part of the cache key. Never compare vectors with different model/dimension pairs.
- `input_tokens` comes from Voyage usage when it can be attributed; otherwise nullable is fine.

### 5.2 `interest_embeddings`

Cache query embeddings for the standing Scour interests.

```sql
CREATE TABLE interest_embeddings (
    interest        TEXT NOT NULL,
    model           TEXT NOT NULL,
    dimension       INTEGER NOT NULL,
    input_hash      TEXT NOT NULL,
    embedding       BLOB NOT NULL,
    created_at      TEXT NOT NULL,
    PRIMARY KEY (interest, model, dimension)
);
```

The canonical embedded text should be versioned and initially be:

```text
Articles about: <interest name>
```

Embed interests with `input_type = "query"`; embed articles with `input_type = "document"`.

### 5.3 `article_facets`

Facet extraction is versioned separately from embeddings because the schema/prompt will evolve independently.

```sql
CREATE TABLE article_facets (
    article_id       INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    schema_version   INTEGER NOT NULL,
    model            TEXT NOT NULL,
    prompt_version   INTEGER NOT NULL,
    input_hash       TEXT NOT NULL,
    facets_json      TEXT NOT NULL,
    extracted_at     TEXT NOT NULL,
    PRIMARY KEY (article_id, schema_version)
);
```

Rules:

- `schema_version` changes whenever enum meanings or JSON shape changes incompatibly.
- `prompt_version` changes when instructions change but the output schema remains compatible.
- Reuse a facet row only if schema version and content hash match.
- Do not mix facet statistics from incompatible schema versions.

### 5.4 `candidate_rankings`

This is essential. Persist every post-hygiene candidate and every ranking component, including articles that never reach Stage A. Without this table, future tuning cannot answer why an article disappeared.

```sql
CREATE TABLE candidate_rankings (
    run_date                    TEXT NOT NULL,
    article_id                  INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,

    heuristic_score             REAL,
    social_score                REAL,
    feed_affinity               REAL,
    semantic_interest_score     REAL,
    positive_similarity         REAL,
    negative_similarity         REAL,
    embedding_preference_score  REAL,
    facet_preference_score      REAL,
    preliminary_score           REAL,

    llm_quality_score           REAL,
    llm_reader_fit_score        REAL,
    utility_score               REAL,
    mmr_score                   REAL,

    recall_pool                 BOOLEAN NOT NULL DEFAULT 0,
    facet_scored                BOOLEAN NOT NULL DEFAULT 0,
    stage_a_candidate           BOOLEAN NOT NULL DEFAULT 0,
    shortlist                   BOOLEAN NOT NULL DEFAULT 0,
    exploration_candidate       BOOLEAN NOT NULL DEFAULT 0,
    selected                    BOOLEAN NOT NULL DEFAULT 0,
    rank_before_mmr             INTEGER,
    rank_after_mmr              INTEGER,

    explanation_json            TEXT,
    PRIMARY KEY (run_date, article_id)
);

CREATE INDEX idx_candidate_rankings_date_stage
    ON candidate_rankings(run_date, recall_pool, stage_a_candidate, shortlist, selected);
```

Persist rows incrementally as stages complete. A rerun for the same date should replace/update the day's rows deterministically.

Do not delete the existing `scores` table immediately. Maintain it for compatibility while migrating code/tests; `candidate_rankings` becomes the authoritative feature snapshot for evaluation. Once all consumers are migrated, a later cleanup can collapse duplication.

### 5.5 Feed/source priors v2

The current `feed_priors` logic has asymmetric attribution: a rating is credited only to `best_entry_id`'s feed, but future multi-source candidates take the maximum prior across all feeds. Replace that behavior.

Prefer a new table rather than silently changing the meaning of integer columns:

```sql
CREATE TABLE feed_priors_v2 (
    feed_id       INTEGER PRIMARY KEY,
    up_weight     REAL NOT NULL DEFAULT 0,
    down_weight   REAL NOT NULL DEFAULT 0,
    included      INTEGER NOT NULL DEFAULT 0,
    updated_at    TEXT NOT NULL
);
```

Rating credit allocation:

1. From `article.sources`, collect sources whose `SourceKind` is the ordinary direct `Feed` kind.
2. If there are one or more, split exactly 1.0 vote weight evenly across those feeds.
3. If there are none, fall back to the article's `best_entry_id` feed.
4. Do not give separate full credit to Scour, HN-frontpage, Reddit, or Lobsters discovery feeds merely because they carried the same story.

Future feed affinity is the weighted mean of the relevant direct-feed priors, not the maximum.

Use Beta smoothing on weighted counts:

```text
rate = (up_weight + 1) / (up_weight + down_weight + 2)
```

An unseen feed remains neutral at 0.5.

Do **not** treat an included-but-unrated article as a downvote. `included` is exposure metadata only.

---

## 6. Rust types and module layout

Add focused modules instead of growing `prefilter.rs` into a monolith.

Recommended layout:

```text
src/curate/
├── embedding.rs       # Voyage client, vector serialization, embedding cache/fetch
├── facets.rs          # facet schema, prompt, extraction, parser
├── preference.rs      # rating-derived embedding/facet/feed preference state
├── recall.rs          # high-recall union construction
├── rank.rs            # normalization, utility calculation, MMR shortlist
├── prefilter.rs       # existing cheap heuristic score/hard filters
├── score.rs           # revised Stage A quality + reader-fit
├── select.rs          # revised Stage B, no forced top-up
├── profile.rs         # weekly qualitative adjustments + interest parsing
└── llm.rs             # existing DeepSeek transport
```

Add corresponding exports from `src/curate/mod.rs`.

### 6.1 Core feature types

Add types similar to:

```rust
pub struct ArticleEmbedding {
    pub article_id: ArticleId,
    pub model: String,
    pub dimension: usize,
    pub values: Vec<f32>,
    pub input_hash: String,
}

pub struct PreferenceState {
    pub positive_centroid: Option<Vec<f32>>,
    pub negative_centroid: Option<Vec<f32>>,
    pub upvote_count: usize,
    pub downvote_count: usize,
    pub facet_preferences: FacetPreferences,
    pub feed_priors: HashMap<FeedId, FeedPriorV2>,
}

pub struct RankingSignals {
    pub heuristic: f64,
    pub social: f64,
    pub feed_affinity: f64,
    pub semantic_interest: f64,
    pub positive_similarity: Option<f64>,
    pub negative_similarity: Option<f64>,
    pub embedding_preference: f64,
    pub facet_preference: f64,
    pub llm_quality: Option<f64>,
    pub llm_reader_fit: Option<f64>,
    pub utility: Option<f64>,
}
```

Keep the article and signals together in a new richer candidate type or extend `ScoredArticle`. Prefer a new `Candidate` type if extending `ScoredArticle` would make every downstream field ambiguous. Whichever approach is chosen, avoid nested `HashMap<String, f64>` feature bags for core signals; use typed fields and serialize an explanation object separately.

---

## 7. Voyage client implementation

### 7.1 Configuration

Add `VoyageConfig` to `src/config.rs`:

```rust
pub struct VoyageConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub output_dimension: usize,
    pub batch_size: usize,
    pub max_input_chars_per_article: usize,
    pub price_per_mtok: f64,
    pub max_daily_usd: f64,
}
```

Load the key from `DAILY_EPUB_VOYAGE__API_KEY` through the existing Figment env nesting convention.

Validate:

- dimension is one of 256/512/1024/2048,
- batch size > 0 and <= 1000,
- `max_input_chars_per_article` > 0,
- price and budget are non-negative.

Document the section in `config.example.toml` but leave the key commented/env-only.

### 7.2 Transport

Implement `EmbeddingBackend` analogous to the existing `ChatBackend` seam so tests never touch the network:

```rust
pub trait EmbeddingBackend: Debug + Send + Sync {
    fn embed<'a>(&'a self, req: EmbeddingRequest)
        -> BoxFuture<'a, Result<EmbeddingResponse, EmbeddingError>>;
}
```

Production request body:

```json
{
  "input": ["...", "..."],
  "model": "voyage-4-lite",
  "input_type": "document",
  "truncation": true,
  "output_dimension": 1024,
  "output_dtype": "float"
}
```

Use existing retry conventions: retry network failures, 429, and 5xx with bounded exponential backoff; do not retry ordinary 4xx request errors. Keep Voyage failures non-fatal to issue generation.

### 7.3 Batching

Although Voyage permits much larger batches, use conservative batching:

- configured max inputs (default 32),
- max 60,000 normalized characters/article,
- `truncation = true` as a final server-side safety valve.

The reason for the conservative client-side cap is that the 1M-token request limit is aggregate. A 32-item batch of capped text remains comfortably below it without adding a Voyage tokenizer dependency.

If an individual batch fails, log it and leave those embeddings missing; do not abort other batches.

### 7.4 Usage/cost accounting

Add a small Voyage usage meter, separate from the DeepSeek `UsageMeter`, tracking:

- input tokens,
- estimated cost,
- daily ceiling trip state.

Do not make `max_daily_usd` suddenly mean “all AI providers” without a migration/deprecation story. Keep existing DeepSeek budget semantics and add `voyage.max_daily_usd`.

Record Voyage usage in `RunReport` JSON. Database columns for Voyage tokens/cost are optional in the first migration if the JSON report is sufficient, but the CLI summary should display provider-specific costs clearly.

---

## 8. Article embedding input

### 8.1 Normalize one deterministic embedding document

Add a versioned helper such as `embedding_document(article) -> String`.

V1 format:

```text
Title: <title>
Author: <author if present>
Source: <feed title>

<full extracted article plain text, capped to max_input_chars_per_article>
```

Use `curate::html_to_text()` as the base text normalizer. Collapse whitespace. Do not include social score, ratings, feed prior, LLM rationale, or other ranking metadata in the embedding. The vector should represent the article itself, not today's popularity or the current model's opinion of it.

Use `input_type = "document"`.

Prefer full extracted text up to the configured cap rather than an opening excerpt. Embedding is exactly where using much more of the article is cheap and useful.

### 8.2 Hash/version

Define a constant such as:

```rust
const EMBEDDING_DOCUMENT_VERSION: u32 = 1;
```

Hash:

```text
sha256("v1\n" + exact_embedding_document)
```

A model/dimension change is already represented in the primary key; an input-format change invalidates the content hash.

---

## 9. Standing-interest semantic matching

The OPML already contains ~220 explicit standing interests. Currently they only reach the LLM profile and provide a Scour provenance bonus. Make them a first-class retrieval signal for every article.

### 9.1 Query embedding cache

For each unique interest from `profile::parse_interests()`:

```text
Articles about: Rust
Articles about: Boston Tech
Articles about: E-Ink Displays
...
```

Embed with `input_type = "query"` and cache in `interest_embeddings`.

### 9.2 Per-article interest score

For an article vector `a`, compute dot product against every standing-interest query vector. At only ~220 interests × ~400 articles × 1024 dimensions, brute-force computation is small.

Persist at least:

- maximum similarity,
- mean of top 3 similarities,
- the names/scores of the top 3 matching interests in `explanation_json`.

Initial scalar `semantic_interest_score`:

```text
0.70 * top1_similarity + 0.30 * mean(top3_similarities)
```

Do not map raw Voyage similarity to an arbitrary 0–100 absolute score yet. For ranking mixtures, normalize this signal within the day's candidate distribution (see §14). Persist the raw similarity as well so future calibration remains possible.

The semantic-interest score is a positive recall signal, not a hard filter. An outstanding article outside the standing interests must still be able to survive through quality/heuristic/exploration paths.

---

## 10. Structured article facets

Embeddings are intentionally topic-heavy. Do **not** try to make one article vector represent every preference dimension. Extract structured facets separately so the system can learn preferences such as “likes postmortems and first-hand technical writing” even when topics differ.

### 10.1 Facet schema v1

Use a strict, typed JSON schema. Keep controlled enums small enough that ratings accumulate statistical support.

Recommended V1:

```rust
pub struct ArticleFacetsV1 {
    // Topic is useful for explanation and coarse preference, but overall semantic
    // topic similarity remains primarily the embedding's job.
    pub topic_group: TopicGroup,
    pub specific_topics: Vec<String>, // 0..=4 normalized short noun phrases

    pub format: ArticleFormat,
    pub depth: Depth,
    pub technicality: Technicality,
    pub audience: AudienceLevel,

    pub tones: Vec<Tone>,             // 1..=3 controlled values
    pub stance: Stance,
    pub stance_target: Option<String>,

    pub evidence_modes: Vec<EvidenceMode>, // 1..=3
    pub temporal_orientation: TemporalOrientation,
    pub locality: Locality,
    pub commerciality: Commerciality,
}
```

Suggested enums:

```text
TopicGroup:
  software_engineering | ai_ml | science_space | culture_arts | books_writing |
  games | hardware | internet_web | business_economics | politics_policy |
  boston_new_england | outdoors_lifestyle | history | other

ArticleFormat:
  reported_news | analysis | essay | opinion | explainer | tutorial |
  technical_deep_dive | postmortem | case_study | research |
  interview | review | personal_narrative | announcement |
  release_notes | roundup | reference | other

Depth:
  brief | standard | deep | exhaustive

Technicality:
  nontechnical | light | intermediate | advanced | expert

AudienceLevel:
  general | informed | practitioner | expert

Tone:
  neutral | analytical | conversational | reflective | skeptical |
  enthusiastic | humorous | argumentative | polemical | literary

Stance:
  descriptive | explanatory | supportive | critical | skeptical |
  mixed | advocacy | not_applicable

EvidenceMode:
  first_hand | original_reporting | primary_sources | data_driven |
  experiment | code_or_artifact | secondary_synthesis | anecdotal | speculative

TemporalOrientation:
  breaking | current | durable | evergreen

Locality:
  boston_new_england | us | international | nonlocal_or_not_applicable

Commerciality:
  none | vendor_educational | product_marketing | sponsored_or_promotional
```

The exact enum spellings should be encoded with `serde(rename_all = "snake_case")`.

Do not include “quality” or “good/bad” as a facet. Stage A owns editorial quality. Facets should describe what the article _is_, so ratings can learn which kinds of articles the reader likes.

Do not infer partisan ideology. `stance` refers to the article's rhetorical relation to its stated subject, not left/right politics.

### 10.2 Representative text sampling

Add one deterministic helper used by both facet extraction and revised Stage A:

```rust
representative_excerpt(article, per_segment_words)
```

V1 behavior:

- Convert the full extracted body to plain text.
- If <= ~450 words, use the whole text.
- Otherwise take approximately:
  - first 150 words,
  - 150 words centered near the midpoint,
  - final 150 words.
- Insert visible separators such as `[BEGINNING]`, `[MIDDLE]`, `[END]`.
- Never split UTF-8 unsafely.

This is materially better evidence than only the introduction and keeps prompts bounded.

### 10.3 Facet extraction stage

Add a new batched DeepSeek call between high-recall construction and the final top-120 cut.

The system prompt remains the reader taste profile for prefix-cache efficiency, but the user instruction must explicitly say that facet extraction is **descriptive, not evaluative** and should not be influenced by whether the reader would like the article.

Per candidate send:

- id,
- title,
- feed/source,
- author,
- word count,
- representative beginning/middle/end excerpt.

Output exactly one typed facet object per article. Parsing must be forgiving in the same way as `score.rs`: malformed one-item output must not lose the rest of the batch.

Default facet batch size can reuse `deepseek.score_batch_size` initially or get its own `facet_batch_size` config (recommended default 12–16).

### 10.4 Facet cache

Facet extraction should be cached by `(article_id, schema_version, input_hash)`. A rerun must not re-spend tokens on unchanged articles.

The recall pool is expected to be ~240 articles, not every raw feed entry. This keeps facet-generation cost bounded while embeddings ensure a low-social but semantically excellent article can still reach this stage.

---

## 11. Quantitative preference learning from ratings

Build a `PreferenceState` at the beginning of every generate run after embeddings are available for rated articles.

Use the existing 90-day rating lookback initially for consistency with `profile.rs`, but make it configurable:

```toml
[curation.personalization]
rating_lookback_days = 90
rating_half_life_days = 45
```

### 11.1 Time decay

Preferences can change. Weight each rating by an exponential half-life:

```text
weight(age_days) = 0.5 ^ (age_days / half_life_days)
```

A rating from today has weight 1.0; one half-life old has weight 0.5.

### 11.2 Positive and negative embedding centroids

For every recent rated article with a compatible embedding:

```text
positive_sum += article_embedding * time_weight   // upvotes
negative_sum += article_embedding * time_weight   // downvotes
```

Normalize each non-empty sum back to unit length.

For each current candidate embedding `x`:

```text
positive_similarity = dot(x, positive_centroid)   // nullable if no upvotes
negative_similarity = dot(x, negative_centroid)   // nullable if no downvotes
```

Initial embedding preference signal:

```text
embedding_preference_raw =
    positive_similarity_or_0
    - 0.75 * negative_similarity_or_0
```

The 0.75 negative coefficient is only a starting default and must be configurable/evaluated. Do not assume downvotes are always topic rejection; facets are intended to distinguish “I dislike vendor announcements about AI” from “I dislike AI.”

Also scale the signal toward neutral when evidence is sparse:

```text
confidence = total_decayed_rating_weight / (total_decayed_rating_weight + 6.0)
embedding_preference = embedding_preference_raw * confidence
```

Persist positive/negative raw similarities and the confidence-adjusted score separately.

### 11.3 Facet preference statistics

For each controlled facet value, aggregate decayed explicit up/down weight.

For a value with weighted evidence `u` and `d`:

```text
rate       = (u + 1) / (u + d + 2)       // Beta(1,1) smoothing
support    = (u + d) / (u + d + 4)
effect     = (rate - 0.5) * 2 * support  // roughly -1 .. +1
```

Compute these for:

- topic_group,
- format,
- depth,
- technicality,
- audience,
- each tone,
- stance,
- each evidence_mode,
- temporal_orientation,
- locality,
- commerciality.

For a candidate, compute one effect per facet dimension and average the available dimensions. Multi-value fields such as tones/evidence modes should average their values before contributing one dimension, otherwise an article with three tones receives triple weight.

Topic group should receive a lower default dimension weight than format/depth/evidence because semantic topic similarity is already represented by embeddings. Suggested initial dimension weights:

```text
topic_group          0.50
format               1.00
depth                1.00
technicality         1.00
audience             0.75
tones                0.75
stance               0.50
evidence_modes       1.00
temporal_orientation 0.50
locality              0.75
commerciality         1.00
```

Normalize by the sum of weights actually present. Keep all weights configurable in one struct/constant block and log them into evaluation metadata.

Free-form `specific_topics` are primarily for explanation in V1; do not exact-match them for preference scoring because synonym fragmentation would be severe. Topic affinity should come from the Voyage embedding and controlled `topic_group`.

### 11.4 Missing historical features

A rating is only useful for embedding/facet preference if its article has those features. Therefore implementation must include a rated-article backfill path (§20). During normal generation, missing historical features simply reduce evidence; they must not fail the run.

---

## 12. Weekly natural-language profile learning

Keep `profile::weekly_rebuild_if_due()` because the qualitative summary is valuable to Stage A/B, but change its role.

### Current problem

The existing prompt says learned adjustments must “never contradict the stated preferences — refine them.” This makes the source-code profile a constitution rather than a prior.

### New rule

Rewrite the instruction approximately as:

> Treat stated preferences as a strong initial prior, not an immutable rule. Prefer repeated, recent behavioral evidence when it clearly conflicts with an older stated preference. Do not override a stated preference from one or two anomalous ratings; call out genuine preference drift only when it is supported across multiple articles.

Because immediate quantitative embedding/facet preferences now react to each vote, keep the weekly rebuild cadence for stability. Do not rebuild the prose profile on every click.

Enrich the rebuild prompt with saved facet data when available. Each rated line should include, compactly:

```text
UP | title | feed | topic | format | depth | technicality | tones | evidence modes
```

This gives the profile LLM the same kind of evidence the numeric learner uses and should produce much better rules than title/feed/category alone.

---

## 13. High-recall candidate construction

Replace “sort one heuristic score and truncate at 120” with a **union-of-retrievers** design.

### 13.1 Hard exclusions remain hard

Before recall ranking, keep the current hard behavior for:

- already-published articles,
- explicit blocked domains,
- obvious non-articles removed earlier,
- recently rejected churn rule (LLM < 3 within the configured lookback), except always-includes.

Keep `always_include_feeds` as mandatory candidates.

### 13.2 Compute cheap signals for every remaining article

For all daily articles, compute:

- existing heuristic score,
- social score,
- v2 feed affinity,
- semantic standing-interest similarity,
- rating-centroid positive/negative similarity,
- confidence-adjusted embedding preference score.

No facets or Stage A score are required yet.

### 13.3 Build a recall union

Add configurable defaults:

```toml
[curation.personalization]
recall_pool_keep = 240
recall_heuristic_top = 160
recall_interest_top = 80
recall_embedding_preference_top = 80
recall_feed_top = 30
recall_exploration = 20
stage_a_keep = 120
shortlist_keep = 60
diversity_lambda = 0.82
```

Construct the union of:

- top `recall_heuristic_top` by existing heuristic,
- top `recall_interest_top` by semantic-interest score,
- top `recall_embedding_preference_top` by learned embedding preference,
- top `recall_feed_top` by feed affinity,
- all auto-includes,
- `recall_exploration` deterministic exploration candidates.

This is deliberately a union rather than one weighted sum. A personally exceptional article needs only one strong path to survive the first cut.

### 13.4 If the union exceeds the cap

First protect:

- all auto-includes,
- the top 20 from each major retriever (heuristic, semantic interest, embedding preference),
- exploration candidates up to their configured reservation.

Then fill remaining slots by a preliminary normalized score (see §14). This prevents one retriever from crowding all others out.

If the union is smaller than `recall_pool_keep`, do not pad from hard-excluded articles; simply use the smaller pool.

---

## 14. Signal normalization and initial ranking weights

Raw signals are on incompatible scales. Do not add raw Voyage cosine similarity directly to 0–100 heuristic scores.

### 14.1 Daily percentile normalization

For continuous ranking signals whose absolute calibration is not established, convert the daily candidate values to deterministic percentile ranks in `[0,1]`:

- heuristic score,
- social score,
- semantic-interest score,
- embedding-preference score,
- feed affinity.

Tie breaking must be stable by article ID.

Persist raw values and normalized values (normalized values can live in `explanation_json` if schema size is a concern).

Facet preference is already approximately `[-1,1]`; map to `[0,1]` with `(x + 1) / 2` for mixtures.

LLM scores are naturally `[0,10]`; divide by 10.

### 14.2 Pre-Stage-A score

After facet extraction, rank the recall pool for the expensive Stage A pass using this **initial** 0–1 blend:

```text
0.28 embedding preference
0.22 facet preference
0.18 semantic standing-interest match
0.14 heuristic quality proxy
0.08 feed affinity
0.05 social proof
0.05 exploration/novelty bonus
```

These are starting weights, not product truth. Put them in a typed config/default structure and make `candidate_rankings` capture every component so `evaluate` can tune them later.

Always-includes bypass the top-`stage_a_keep` cut.

Do not let social proof exceed this weak role. Stage A will separately judge quality; HN popularity should no longer be counted three times.

---

## 15. Exploration

Exploration should prevent preference lock-in without making the newspaper noisy.

V1 exploration candidate definition:

- not auto-included,
- not recently rejected,
- not already highly ranked by the personalized retrievers,
- from a feed with low rating evidence **or** semantically outside the dense region of recent positive ratings,
- still above a minimal heuristic-quality floor so the system does not explore obvious junk.

Choose exploration items deterministically using a stable hash of `(run_date, article_id)` after filtering. This makes regeneration of the same date reproducible.

Exploration reserves **candidate-pool/shortlist exposure**, not guaranteed publication. Stage B can still reject an exploration article.

Persist `exploration_candidate = true` for evaluation.

---

## 16. Revise Stage A: separate editorial quality from reader fit

The current single `llm.score` mixes quality and personal preference. Split it so final ranking can reason about them independently.

### 16.1 New response

Change Stage A to return:

```json
{
  "articles": [
    {
      "id": 123,
      "quality_score": 8.5,
      "reader_fit_score": 7.0,
      "category": "Tech & Engineering",
      "rationale": "first-hand failure analysis with concrete measurements",
      "is_paywalled_guess": false
    }
  ]
}
```

Update `LlmScore` accordingly, or create a versioned `LlmArticleAssessment` and migrate call sites. Prefer a new type if changing `LlmScore` would make existing persisted `scores.llm_score` ambiguous.

### 16.2 Quality rubric

`quality_score` should judge:

- substance,
- originality/first-hand evidence,
- clarity and writing quality,
- depth appropriate to the subject,
- whether the article rewards the time spent reading it.

Explicitly tell the model:

- do not award quality merely for length,
- do not award quality merely for social popularity,
- announcements/roundups/vendor marketing are generally low quality unless there is substantial original analysis,
- evaluate from the representative beginning/middle/end sample.

### 16.3 Reader-fit rubric

`reader_fit_score` should judge whether the reader is likely to value the article given the taste profile, including its learned adjustments. It should _not_ be shown the numeric embedding preference/facet/feed/social scores; those are independent model inputs and would cause self-reinforcing double counting.

### 16.4 Prompt evidence

Replace the current first-200-word excerpt with the representative beginning/middle/end excerpt from §10.2.

Continue to provide basic metadata such as title, author, feed, word count, and excerpt-only status. Remove raw social statistics from Stage A unless an experiment demonstrates that they improve quality prediction; social proof is already a separate feature and the current model prompt explicitly lets popularity influence its judgment.

Keep source provenance if useful for extraction context, but stop telling the model that `came via HN` or `came via Scour` should inherently boost the score.

---

## 17. Final personalized utility score

After Stage A, compute a transparent utility score before diversification.

Initial normalized blend:

```text
0.40 LLM editorial quality
0.15 LLM reader fit
0.15 embedding rating preference
0.10 facet preference
0.07 standing-interest semantic match
0.05 feed affinity
0.04 heuristic score
0.04 social proof
--------------------------------
1.00 total
```

Store the resulting value as `utility_score` on a 0–100 scale for readability.

Why quality remains largest: this newspaper should prefer an excellent piece slightly outside known taste over mediocre content that matches a favored topic. Why learned behavior still matters materially: 30% of the score (`embedding + facets`) is direct rating-derived preference, and the LLM reader-fit/profile adds another adaptive signal.

Again, make these defaults configurable and subject to offline evaluation. Do not scatter literals across modules.

Auto-includes remain guaranteed for final Stage B consideration even if their utility is low.

---

## 18. Diversified shortlist with MMR

Do not simply take the top 40/60 by utility. Use article embeddings to reduce redundant topic coverage before Stage B.

### 18.1 Algorithm

Use Maximal Marginal Relevance (MMR):

```text
MMR(candidate) =
    lambda * normalized_utility(candidate)
    - (1 - lambda) * max_similarity(candidate, already_selected)
```

Default:

```text
lambda = 0.82
shortlist_keep = 60
```

Because Voyage vectors are normalized, article-to-article similarity is a dot product.

### 18.2 Shortlist construction rules

1. Seed with the highest-utility non-auto candidate.
2. Repeatedly select the highest MMR score.
3. Guarantee all auto-includes are present even if this exceeds the nominal size.
4. Guarantee a small number of exploration candidates survive to Stage B if any meet the minimum preliminary-quality floor.
5. Preserve at least the top ~20 articles by raw utility regardless of MMR so a cluster of genuinely exceptional same-topic coverage is not entirely erased by diversity pressure.
6. Persist `rank_before_mmr`, `rank_after_mmr`, and the MMR value.

Use embeddings for diversity because topic dominance is desirable in this specific calculation: MMR is supposed to notice that six articles are about essentially the same thing.

### 18.3 Duplicate-story handling

Keep existing URL/title dedupe and Stage B's “do not select two articles that tell the same story.” MMR is not a replacement for duplicate detection; it reduces thematic redundancy among genuinely different articles.

---

## 19. Revise Stage B selection

Stage B remains the final editor and should receive a larger, better, more diverse shortlist (default ~60 instead of 40).

### 19.1 Candidate information

For each candidate show:

- title,
- feed,
- word count / reading time,
- LLM quality score,
- LLM reader-fit score,
- concise Stage A rationale,
- top matching standing interests,
- compact descriptive facets (`format`, `depth`, `technicality`, selected tones/evidence modes),
- whether it is auto-include or exploration,
- a short representative blurb (not just first 45 words).

Do **not** dump every numeric ranking component into the Stage B prompt. The LLM should have enough evidence to edit the issue but not mechanically reproduce the scorer.

### 19.2 Remove the hard minimum

Change the instruction from “choose target; never fewer than target-5” to:

- target approximately `target_article_count` (20 by default),
- never exceed configurable `max_article_count` (25 by default, plus unavoidable auto-includes if necessary),
- choose materially fewer when the shortlist does not justify a full issue,
- never pad with an article the editor would not defend.

In `assemble()`:

- keep the max-size trim,
- **delete the automatic top-up to a minimum**,
- only fall back to heuristic selection when Stage B returns zero usable picks or the call itself fails,
- if the model returns 8 good articles, publish 8.

`--max-articles N` should remain a hard ceiling/override, not a target that forces filling.

### 19.3 Section diversity remains editorial

Keep the existing section palette and Stage B instructions to create a coherent paper. MMR handles topical redundancy before the LLM; section assignment and issue rhythm remain Stage B responsibilities.

---

## 20. Backfill and new CLI commands

The new preference model will initially have historical ratings but no historical embeddings/facets. Provide a supported backfill command rather than relying on daily runs to fill the cache slowly.

Recommended CLI:

```text
daily-epub features backfill [--days N] [--rated-only] [--embeddings-only] [--facets-only]
daily-epub evaluate --from YYYY-MM-DD --to YYYY-MM-DD
daily-epub explain --date YYYY-MM-DD --article ID
```

### 20.1 Backfill order

For first deployment:

1. Embed **all rated articles first**.
2. Extract facets for all rated articles first.
3. Embed recent articles (e.g. last 90 days) for historical replay/exploration if desired.
4. Facet-backfill non-rated historical candidates only when needed for evaluation; do not spend DeepSeek tokens on the entire archive automatically.
5. Build interest query embeddings.

The implementation does not require a Voyage key to compile or pass tests. The operator can supply `DAILY_EPUB_VOYAGE__API_KEY` before live backfill/generation.

### 20.2 `explain`

`explain` should print the persisted ranking row in human-readable form, including:

- raw and normalized signals,
- top semantic interests,
- positive/negative centroid similarities,
- strongest positive/negative facet contributions,
- feed affinity,
- Stage A scores/rationale,
- utility rank,
- MMR penalty/rank,
- which funnel stages it survived,
- whether it was selected.

This is invaluable for tuning and debugging “why did this article show up?” behavior.

---

## 21. Offline evaluation

Do not tune the new ranking solely by reading a few generated issues.

### 21.1 Historical replay

`candidate_rankings` provides a durable feature snapshot for new runs. For historical ratings from before this migration, backfill embeddings/facets for rated articles and replay the available candidate universe as far as the database permits.

The `articles` table stores all deduped persisted articles, not just selected issue articles, so recent windows can be reconstructed from `first_seen`/entry publication timestamps. For exact future replay, `candidate_rankings` becomes authoritative.

### 21.2 Metrics

Because only shown articles can receive ratings, labels are selection-biased. Do not pretend unrated/unshown articles are negative examples.

Useful metrics:

1. **Pairwise preference accuracy:** when an upvoted and downvoted article occur in the same issue/day, how often does the new utility score rank the upvote higher?
2. **Mean utility rank by explicit vote:** compare rank distributions for upvotes vs downvotes.
3. **NDCG / DCG over explicitly rated articles only**, with up=1, down=0, clearly labeled as conditional-on-rated.
4. **Source/facet calibration:** for facet values with enough evidence, compare predicted preference effect with later votes.
5. **Shortlist diversity:** mean/max pairwise article embedding similarity and topic-group concentration.
6. **Recall boundary diagnostics:** count historical upvoted articles that would have been lost at each proposed stage (`recall_pool`, `stage_a`, `shortlist`). This is one of the most important metrics.
7. **Exploration yield:** upvote/downvote rate of selected exploration articles, but only after enough observations.
8. **Issue size and rating rate:** ensure removal of the hard minimum does not collapse issues or reduce engagement unexpectedly.

### 21.3 Weight tuning

The first version may use the weights in this plan. After enough candidate snapshots accumulate, move weight values based on replay results. Keep a small checked-in note/table of evaluation results when defaults change so future agents know why the numbers moved.

Do not introduce an optimizer/ML model until the simple weighted ranker has enough labeled examples to justify it.

---

## 22. Rating-flow changes

The rating HTTP endpoint should remain fast and simple.

On changed 👍/👎:

1. upsert the rating exactly as today,
2. rebuild/update v2 feed priors,
3. do **not** synchronously call Voyage or DeepSeek from the HTTP request,
4. return the confirmation page immediately.

Why: rated articles should already have embedding/facet rows because they appeared in an issue. If one is unexpectedly missing, the next `generate` or explicit backfill can repair it. The rating endpoint must not become dependent on external AI latency.

Flipping an existing vote must be handled correctly by preference rebuilding; do not increment counters blindly. Recompute derived preferences from canonical ratings or update transactionally with old/new vote awareness.

---

## 23. Failure and fallback behavior

The service's current degradation philosophy is good and must be preserved.

### Voyage unavailable/key missing

- Load cached article/interest embeddings when available.
- Do not generate new ones.
- Missing embedding-based signals become neutral, not zero-quality penalties.
- Recall still uses heuristic/feed/social plus cached facets.
- Issue generation continues.

### DeepSeek unavailable / `--skip-llm`

- Do not generate new facets.
- Reuse cached facets if present.
- Skip Stage A/Stage B as today.
- Use the enhanced deterministic ranking (heuristic + Voyage/cached preferences when available) rather than reverting all the way to old prefilter order.
- Editorial summaries continue to fall back to excerpts.

For backward compatibility, `--skip-llm` should continue to mean “no DeepSeek/generative calls.” It may use already-cached Voyage embeddings. It should **not** unexpectedly issue new Voyage requests unless the CLI docs explicitly say so. Recommended behavior: under `--skip-llm`, embedding generation is also disabled and only cached embeddings are used. Add `--skip-embeddings` later only if a real operator need appears.

### Facet extraction partial failure

- Successfully parsed facet rows are stored.
- Missing facet preference is neutral.
- Do not drop an article because the facet model failed.

### Budget ceiling

- DeepSeek and Voyage meters trip independently.
- Once tripped, remaining calls for that provider are skipped for the run.
- Existing cached features remain usable.

---

## 24. Observability and reporting

Extend `RunReport` counts with at least:

```text
embedding_cache_hits
embeddings_generated
embedding_failures
voyage_input_tokens
voyage_cost_usd
interest_embeddings_generated
recall_pool_count
facets_cache_hits
facets_generated
facet_failures
stage_a_candidates
shortlist_candidates
exploration_candidates
selected_exploration
```

Log stage timings separately for:

```text
embedding
preference
recall
facets
personalized_rank
mmr
```

At info level, print one summary such as:

```text
curation: 417 eligible -> 238 recall -> 120 stage A -> 60 diversified -> 17 selected
personalization: 63 recent ratings, 55 embedding-backed, 51 facet-backed
```

At debug level, log top ranking explanations but never full article embeddings or API keys.

---

## 25. Configuration changes

Keep the top-level defaults understandable. Suggested additions:

```toml
[voyage]
enabled = true
base_url = "https://api.voyageai.com/v1"
model = "voyage-4-lite"
# api_key via DAILY_EPUB_VOYAGE__API_KEY
output_dimension = 1024
batch_size = 32
max_input_chars_per_article = 60000
price_per_mtok = 0.02
max_daily_usd = 0.25

[curation.personalization]
enabled = true
rating_lookback_days = 90
rating_half_life_days = 45
recall_pool_keep = 240
recall_heuristic_top = 160
recall_interest_top = 80
recall_embedding_preference_top = 80
recall_feed_top = 30
recall_exploration = 20
stage_a_keep = 120
shortlist_keep = 60
diversity_lambda = 0.82
max_article_count = 25
```

For score weights, either expose a nested `[curation.personalization.weights]` block or keep defaults in a single typed Rust struct for the first release. If exposed, validate that weights are non-negative and normalize them in code rather than requiring exact sum=1 in TOML.

Do not remove existing `prefilter_keep` immediately. Mark it deprecated/alias it to `stage_a_keep` for one release if backward compatibility matters; otherwise update README/config example and migrate directly since this repository currently has a single operator.

---

## 26. Specific current-code changes

### `src/config.rs`

- Add `VoyageConfig`.
- Add nested personalization config to `CurationConfig` or a separate `PersonalizationConfig` field.
- Validate dimensions, counts, lambda `[0,1]`, lookbacks, and budgets.
- Add unit tests for TOML/env overrides including `DAILY_EPUB_VOYAGE__API_KEY` without printing the secret.

### `src/types.rs`

- Add embedding/facet/preference/ranking types or place module-local types where appropriate.
- Split `LlmScore.score` into quality + reader-fit semantics.
- Replace `ScoredArticle::combined_score()` with explicit utility calculation in `rank.rs`; do not leave two competing ranking formulas active.
- Keep old serialization compatibility only if persisted JSON requires it.

### `src/db.rs`

Add runtime-query helpers for:

- get/upsert article embedding,
- batch-load embeddings for article IDs,
- get/upsert interest embeddings,
- get/upsert article facets,
- load recent ratings joined to article/source/facets,
- rebuild/load v2 feed priors,
- upsert/update candidate ranking snapshot,
- list candidates/rankings for evaluation/explain.

Continue the existing implementation convention: runtime `sqlx::query`, no new compile-time DB requirements.

### `src/curate/embedding.rs` (new)

- Voyage request/response structs,
- backend trait + mock,
- retry/error classification,
- batching,
- normalized embedding document,
- SHA-256 cache hash,
- f32 BLOB encode/decode,
- dot product helper with dimension validation,
- article + interest embedding cache orchestration,
- usage meter.

### `src/curate/facets.rs` (new)

- facet enums/schema v1,
- representative excerpt helper (or put shared helper in `curate/mod.rs`),
- facet prompt,
- tolerant JSON parser,
- batch extraction,
- cache orchestration.

### `src/curate/preference.rs` (new)

- load decayed ratings,
- positive/negative centroid construction,
- facet stats,
- v2 feed priors,
- per-candidate preference signal calculation,
- explanation generation for strongest learned preferences.

### `src/curate/recall.rs` (new)

- hard-history filtering should reuse `prefilter` context/functions rather than duplicate SQL,
- union retrievers,
- deterministic exploration,
- cap/protection rules.

### `src/curate/rank.rs` (new)

- percentile normalization,
- pre-Stage-A score,
- final utility score,
- stable deterministic sorting,
- MMR.

### `src/curate/prefilter.rs`

- Keep hard block/history/churn logic.
- Keep cheap heuristic feature functions.
- Stop letting this module own the only top-N cutoff.
- Reduce duplicated popularity/long-form assumptions if needed after Stage A is revised; preserve current values initially for evaluation, but make the heuristic contribution weak in final utility.

### `src/curate/score.rs`

- New quality + reader-fit response shape.
- Representative beginning/middle/end sample.
- Remove/neutralize social-proof instructions and source-provenance boosts from the LLM rubric.
- Continue tolerant parsing and batch failure isolation.

### `src/curate/select.rs`

- Increase shortlist input default to configured ~60.
- Render facets/top matching interests/quality + fit.
- Remove hard minimum and top-up path.
- Keep max trim, section validation, unique lead, auto-include reinsertion, duplicate-ID defense, and malformed-response fallback.

### `src/curate/profile.rs`

- Keep OPML parsing/theme grouping.
- Change learned-adjustment prompt from immutable stated preferences to strong prior + evidence-driven drift.
- Include facet context in rating history.
- Keep weekly cadence.
- Move feed prior v2 logic into `preference.rs` once stable, leaving thin compatibility wrappers if convenient.

### `src/pipeline.rs`

Wire the stages in the order described in §4. Important ordering:

1. social enrichment,
2. Voyage feature cache/generation,
3. preference-state build,
4. recall pool,
5. facets,
6. pre-Stage-A ranking/cut,
7. Stage A,
8. utility + MMR,
9. Stage B.

Every new external stage is degrading/non-fatal.

### `src/main.rs`

- Add `features backfill`.
- Add `evaluate`.
- Add `explain`.
- Update `--skip-llm` help text to describe cached-feature behavior.

### `src/report.rs`

- Add new counts, timings, Voyage usage/cost, and funnel summary.

### `README.md` / `config.example.toml`

- Document Voyage API key/config.
- Document new curation architecture at a high level.
- Document backfill/evaluate/explain commands.
- Document that issue size is now a soft target with no forced filler.

---

## 27. Tests

No test may call Voyage or DeepSeek over the network.

### 27.1 Embedding unit tests

- f32 BLOB round trip preserves values and dimension.
- malformed BLOB length is rejected safely.
- embedding document is deterministic and capped.
- cache hit when input hash/model/dimension match.
- cache miss on changed article content.
- cache miss on dimension/model change.
- mock Voyage response maps embeddings by index correctly.
- partial/failed batches do not abort subsequent batches.
- 429/5xx classified retryable; normal 4xx not retryable.
- dot product dimension mismatch returns an error, never panic.

### 27.2 Interest tests

- OPML interest embeddings are cached/deduped.
- known synthetic article has expected top semantic interest with fixture vectors.
- top-1/top-3 aggregation deterministic.

### 27.3 Facet tests

- strict happy-path facet JSON parses.
- string casing/unknown/malformed individual entries are handled according to parser policy.
- one malformed article does not discard valid siblings.
- representative excerpt includes beginning/middle/end and respects short articles.
- facet cache invalidates on content/schema change.

### 27.4 Preference tests

- no ratings => neutral embedding/facet/feed signals.
- one upvote produces a positive centroid.
- up/down centroids are unit-normalized.
- time decay halves at configured half-life.
- flipping a rating changes derived state correctly.
- facet Beta smoothing stays near neutral with one vote and strengthens with repeated evidence.
- multi-value facets contribute once per dimension, not once per label.
- feed rating credit sums to exactly 1.0 across direct feed sources.
- candidate feed affinity uses mean/weighted mean, never optimistic max.

### 27.5 Recall tests

Critical regression test:

> An article with mediocre heuristic/social score but extremely strong semantic/rating similarity survives the recall pool and can reach Stage A.

Also test:

- high heuristic article survives via heuristic retriever,
- auto-includes always survive,
- blocked/history/recently-rejected articles do not leak through,
- each retriever's protected minimum survives cap pressure,
- exploration selection deterministic for same date,
- different dates can rotate exploration candidates.

### 27.6 Ranking/MMR tests

- percentile normalization stable with ties.
- utility weight calculation exact.
- highest utility seeds MMR.
- near-duplicate embeddings are penalized after one is selected.
- a somewhat lower-utility diverse article can outrank a redundant one under configured lambda.
- top raw-utility preservation rule works.
- auto-includes survive shortlist limit.

### 27.7 Stage A/B tests

- Stage A parses quality + reader-fit.
- Stage A prompt contains representative sections and no social-score calibration instruction.
- Stage B prompt includes compact facets and top interest matches.
- Stage B accepts a deliberately small lineup.
- **Delete/replace tests that require top-up to `target - 5`.**
- zero usable Stage B picks still triggers fallback.
- max count still trims safely.

### 27.8 Pipeline integration tests

Using mocked Voyage + DeepSeek:

- full enhanced funnel produces persisted ranking rows for all eligible candidates,
- Voyage failure still publishes using remaining signals,
- facet failure still publishes,
- DeepSeek failure uses deterministic enhanced ranking,
- `--skip-llm` makes zero DeepSeek calls and zero uncached Voyage calls,
- rerun same date is idempotent and reuses caches.

### 27.9 Migration tests

Open a temp DB, run all migrations, exercise new tables/indexes, and confirm existing rating/issue data remains readable.

---

## 28. Rollout strategy

Implement behind a configuration switch first:

```toml
[curation.personalization]
enabled = false
```

Then roll out in phases.

### Phase A — data collection / shadow mode

- Migrations, Voyage client, embeddings, facets, preference state, candidate ranking snapshots.
- Existing production selection remains authoritative.
- New ranker computes in shadow mode and persists what it _would_ have done.
- Compare old vs new selections for several days.

This is strongly recommended because it creates actual data to validate weights before changing the newspaper.

### Phase B — personalized recall, old final selector

- Enable union recall and Stage-A candidate selection.
- Keep existing final combined score/Stage B behavior temporarily if needed.
- Verify that known desirable low-social articles now survive.

### Phase C — new Stage A + utility + MMR

- Enable separated quality/reader-fit scoring.
- Enable new utility and 60-item diversified shortlist.
- Monitor shortlist diversity and explicit ratings.

### Phase D — remove forced minimum

- Enable no-top-up Stage B behavior.
- Observe issue sizes and ratings for at least several runs.

### Phase E — retire shadow/compatibility code

- Remove old `combined_score()` path and obsolete config only after the new system has demonstrated stable behavior.

---

## 29. Implementation sequence / suggested commits

An agent should implement in small, reviewable commits roughly in this order:

1. **`Add personalization schema and config`**
   - migration,
   - Voyage + personalization config,
   - DB primitives,
   - base types.

2. **`Add Voyage embedding cache and client`**
   - backend seam/mock,
   - article document generation,
   - f32 serialization,
   - article/interest embedding orchestration,
   - tests.

3. **`Add article facet extraction`**
   - schema v1,
   - representative excerpt,
   - prompt/parser/cache,
   - tests.

4. **`Build rating-derived preference state`**
   - decayed centroids,
   - facet preference stats,
   - feed priors v2,
   - tests.

5. **`Add personalized recall pipeline`**
   - union retrievers,
   - exploration,
   - candidate snapshots,
   - tests.

6. **`Separate LLM quality and reader fit`**
   - Stage A response/prompt,
   - persisted fields,
   - compatibility migration/tests.

7. **`Add utility ranking and diversified shortlist`**
   - normalization,
   - weights,
   - MMR,
   - tests.

8. **`Make final selection quality-gated rather than padded`**
   - Stage B prompt/rendering,
   - remove top-up,
   - max-only validation,
   - tests.

9. **`Add personalization backfill and evaluation tools`**
   - CLI,
   - explain output,
   - replay metrics.

10. **`Enable personalized curation and update docs`**
    - config example,
    - README,
    - rollout flag/default after shadow evaluation.

Do not combine all of this into one giant implementation commit.

---

## 30. Acceptance criteria

The feature is complete when all of the following are true:

1. Every eligible new article can receive a cached `voyage-4-lite` embedding before the first personalized candidate cut.
2. An article with low social/word-count heuristic score can reach Stage A solely because it strongly matches standing interests or positive rating history.
3. Article facets are stored in a versioned typed schema and include at minimum topic, format, depth, technicality, tone, stance, and evidence style.
4. Explicit ratings affect the next day's ranking through:
   - embedding similarity,
   - facet preferences,
   - corrected feed affinity,
     without waiting for the weekly profile rebuild.
5. Weekly learned profile adjustments include facet context and may recognize sustained preference drift.
6. The final utility score exposes separate quality, semantic preference, facet preference, feed, social, and heuristic components.
7. The shortlist is diversified with article-embedding MMR and is larger than the current ~40 by default.
8. Stage B can publish fewer than 15 articles without deterministic filler being added.
9. Missing Voyage or DeepSeek service/key does not prevent issue generation.
10. `candidate_rankings` records why every eligible daily article did or did not survive each funnel stage.
11. `features backfill` can populate at least all historical rated articles without network calls in tests.
12. `evaluate` can compare upvoted vs downvoted ranking quality and report recall losses at each funnel boundary.
13. `explain` can answer why a specific article was ranked/selected from persisted data.
14. All current tests continue to pass after intentional expectation updates, and new curation modules have deterministic unit coverage.
15. No API keys or raw full embedding vectors are emitted to ordinary logs/reports.

---

## 31. Decisions intentionally left tunable

The implementation agent should **not** block on perfect values for these. Use the defaults in this plan, persist enough data to evaluate them, and keep them configurable:

- Voyage dimension (default 1024),
- rating lookback and half-life,
- recall sub-pool sizes,
- facet dimension weights,
- negative-centroid coefficient,
- evidence-confidence constants,
- pre-Stage-A ranking weights,
- final utility weights,
- MMR lambda,
- shortlist size,
- exploration reservation,
- Stage B soft target/max.

The architectural decisions are the important part: high-recall union, full-content semantic embeddings, separate structured facets, immediate rating-derived preference state, quality/fit separation, diversified shortlist, and no forced filler.

---

## 32. Final target behavior

A good outcome should feel qualitatively different from the current implementation:

- A quiet 900-word post from an obscure feed about a highly favored niche can beat a viral generic HN story because semantic and learned preference signals rescue it early.
- A user who repeatedly upvotes first-hand postmortems and downvotes vendor announcements should see that preference propagate across unrelated topics and publications through facets, not merely through feed priors.
- A user who changes taste over time should influence the next issue immediately through decayed embeddings/facets rather than waiting for a weekly prose-profile rewrite.
- Six articles about the same AI news cycle should not occupy most of the final shortlist merely because they all scored well individually.
- The LLM editor should receive a broad, high-quality, deliberately diverse set of candidates and be free to publish a short issue when that is what the day deserves.

That is the design goal: **a recommendation system that maximizes candidate recall for this reader first, then uses explicit editorial quality judgment and an LLM editor to turn those candidates into a coherent newspaper.**
