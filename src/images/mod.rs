//! Article images, end to end (spec §3.3 extraction, §3.10 "Images").
//!
//! The stages run in this order, and each submodule owns one of them:
//!
//! | stage | module | when |
//! |---|---|---|
//! | make a page's `<img>` elements usable | [`normalize`] | extraction, before readability |
//! | find what an article references | [`refs`] | extraction and issue build |
//! | download and re-encode per edition | [`fetch`], [`encode`] | issue build |
//! | point the markup at the embedded files | [`embed`] | chapter rendering |
//!
//! Nothing here ever fails a run: an image that cannot be fetched, decoded or
//! resolved is dropped, and the article is rendered without it (notes §3).

pub mod embed;
pub mod encode;
pub mod fetch;
pub mod normalize;
pub mod refs;

pub use embed::rewrite_img_srcs;
pub use encode::{ImageProfile, MIN_DIMENSION_PX, reencode};
pub use fetch::{ISSUE_ASSET_BUDGET_BYTES, collect_for_issue, download};
pub use normalize::{normalize_img_tags, prepare_for_readability, unwrap_image_wrappers};
pub use refs::{ImgRef, collect_image_urls, extract_img_refs};
