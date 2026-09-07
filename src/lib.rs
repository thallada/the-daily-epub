//! `daily-epub` — a personalized daily newspaper as an EPUB (spec §1, §2).
//!
//! The crate ships both a library and a thin `daily-epub` binary. Everything the
//! pipeline does lives here so that integration tests can drive the stages
//! directly (see `tests/e2e_pipeline.rs`) instead of shelling out to the binary.
//!
//! Pipeline order (spec §2), all of it wired in [`crate::pipeline`]:
//!
//! ```text
//! Miniflux ingest → dedupe → extraction → persist → social enrichment
//!   → pre-filter → LLM scoring → selection → comments → editorial
//!   → world briefing → EPUB build (standard + X4) → XTC → publish → report
//! ```

pub mod auth;
pub mod bookorbit;
pub mod comments;
pub mod config;
pub mod curate;
pub mod db;
pub mod dedupe;
pub mod discovery;
pub mod epub;
pub mod extract;
pub mod html;
pub mod http;
pub mod images;
pub mod imports;
pub mod jobs;
pub mod lock;
pub mod mail;
pub mod miniflux;
pub mod pipeline;
pub mod publish;
pub mod rate;
pub mod report;
pub mod server;
pub mod social;
pub mod types;
pub mod web;
pub mod world;

/// `CARGO_PKG_VERSION`, printed in the colophon and the OPDS generator tag.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
