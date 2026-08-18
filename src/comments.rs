//! Comment chapters: fetch trees for selected articles and render them (spec §3.7).
//!
//! Heuristic only, no LLM. Rendered as nested border-left indentation that reads
//! well on e-ink (no color). Every fetch is best-effort: a platform that errors
//! out is simply left out of the chapter (notes §3).

use std::collections::HashSet;

use futures::StreamExt;
use serde_json::Value;

use crate::html::{text_escape, to_xhtml};
use crate::types::{Comment, CommentThread, Discussion, Pick, SocialSource};

/// Top-level threads kept per source (§3.7).
pub const MAX_TOP_LEVEL: usize = 8;
/// Maximum nesting depth rendered: depths 0, 1 and 2 (§3.7).
pub const MAX_DEPTH: usize = 3;
/// Maximum children rendered per node (§3.7).
pub const MAX_CHILDREN: usize = 4;
/// Per-comment character cap before ellipsizing (§3.7).
pub const MAX_COMMENT_CHARS: usize = 1200;
/// Whole-chapter word cap (§3.7).
pub const MAX_CHAPTER_WORDS: usize = 4000;
/// Concurrent discussion fetches.
pub const CONCURRENCY: usize = 4;

/// Platform order inside a discussion chapter: HN → Lobsters → Reddit (§3.7).
pub const SOURCE_ORDER: &[SocialSource] = &[
    SocialSource::Hn,
    SocialSource::Lobsters,
    SocialSource::Reddit,
];

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

/// `https://hn.algolia.com/api/v1/items/{objectID}` (§3.7).
pub fn hn_items_url(item_id: &str) -> String {
    format!("https://hn.algolia.com/api/v1/items/{item_id}")
}

/// `https://lobste.rs/s/{id}.json` (§3.7).
pub fn lobsters_url(item_id: &str) -> String {
    format!("https://lobste.rs/s/{item_id}.json")
}

/// `https://www.reddit.com{permalink}.json?limit=100&depth=3&sort=top` (§3.7).
pub fn reddit_url(permalink_or_url: &str) -> String {
    let base = permalink_or_url.trim_end_matches('/');
    let base = if base.starts_with("http://") || base.starts_with("https://") {
        base.to_string()
    } else if base.starts_with('/') {
        format!("https://www.reddit.com{base}")
    } else {
        format!("https://www.reddit.com/{base}")
    };
    let base = base.trim_end_matches(".json").to_string();
    format!("{base}.json?limit=100&depth=3&sort=top")
}

async fn get_json(http: &reqwest::Client, url: &str) -> Option<Value> {
    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| tracing::debug!(url, "comment fetch failed: {e}"))
        .ok()?;
    if !resp.status().is_success() {
        tracing::debug!(url, status = %resp.status(), "comment fetch rejected");
        return None;
    }
    resp.json::<Value>()
        .await
        .map_err(|e| tracing::debug!(url, "comment payload was not json: {e}"))
        .ok()
}

/// Fetch and assemble the discussion for one selected article, ordered
/// HN → Lobsters → Reddit (§3.7). Best-effort: returns `None` on failure.
pub async fn fetch_discussion(http: &reqwest::Client, pick: &Pick) -> Option<Discussion> {
    let mut threads: Vec<CommentThread> = Vec::new();
    for source in SOURCE_ORDER {
        let Some(social) = pick.article.social.iter().find(|s| s.source == *source) else {
            continue;
        };
        let thread = match source {
            SocialSource::Hn => match social.item_id.as_deref() {
                Some(id) => get_json(http, &hn_items_url(id))
                    .await
                    .and_then(|v| parse_hn(&v)),
                None => None,
            },
            SocialSource::Lobsters => match social.item_id.as_deref() {
                Some(id) => get_json(http, &lobsters_url(id))
                    .await
                    .and_then(|v| parse_lobsters(&v)),
                None => None,
            },
            SocialSource::Reddit => {
                let target = social
                    .item_url
                    .as_deref()
                    .or(social.item_id.as_deref())
                    .map(reddit_url);
                match target {
                    Some(url) => get_json(http, &url)
                        .await
                        .and_then(|v| parse_reddit(&v, "")),
                    None => None,
                }
            }
            SocialSource::X => None,
        };
        match thread {
            Some(mut t) => {
                t.comments = truncate(t.comments);
                if !t.comments.is_empty() {
                    threads.push(t);
                }
            }
            None => tracing::debug!(
                source = %source,
                article = pick.article.id,
                "no comment tree for this source"
            ),
        }
    }
    if threads.is_empty() {
        return None;
    }
    enforce_chapter_budget(&mut threads);
    Some(Discussion {
        article_id: pick.article.id,
        chapter_id: format!("disc-{}", pick.article.best_entry_id),
        threads,
    })
}

/// Fetch discussions for every pick in parallel, filling [`Pick::discussion`] (§3.7).
pub async fn fetch_all(http: &reqwest::Client, picks: &mut [Pick]) -> usize {
    let fetched: Vec<Option<Discussion>> = futures::stream::iter(picks.iter().map(|pick| {
        let http = http.clone();
        async move { fetch_discussion(&http, pick).await }
    }))
    .buffered(CONCURRENCY)
    .collect()
    .await;

    let mut count = 0;
    for (pick, discussion) in picks.iter_mut().zip(fetched) {
        if discussion.is_some() {
            count += 1;
        }
        pick.discussion = discussion;
    }
    tracing::info!(count, of = picks.len(), "fetched discussion chapters");
    count
}

// ---------------------------------------------------------------------------
// Parsing (pure — fixtures cover these, no network in tests)
// ---------------------------------------------------------------------------

/// Parse the Algolia `items/{id}` tree (§3.7).
pub fn parse_hn(v: &Value) -> Option<CommentThread> {
    let id = v.get("id")?.as_i64()?;
    let mut comments = Vec::new();
    let mut total = 0i64;
    for child in v.get("children")?.as_array()?.iter() {
        if let Some(c) = hn_node(child, 0, &mut total) {
            comments.push(c);
        }
    }
    sort_by_points(&mut comments);
    Some(CommentThread {
        source: SocialSource::Hn,
        item_url: format!("https://news.ycombinator.com/item?id={id}"),
        total_comments: total,
        comments,
    })
}

fn hn_node(v: &Value, depth: usize, total: &mut i64) -> Option<Comment> {
    let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let author = v.get("author").and_then(|a| a.as_str()).unwrap_or("");
    let mut children = Vec::new();
    if let Some(kids) = v.get("children").and_then(|c| c.as_array()) {
        for kid in kids {
            if let Some(c) = hn_node(kid, depth + 1, total) {
                children.push(c);
            }
        }
    }
    if text.is_empty() || author.is_empty() {
        // Dead/deleted node: keep its (live) replies by lifting them up.
        return children.into_iter().next();
    }
    *total += 1;
    sort_by_points(&mut children);
    Some(Comment {
        author: author.to_string(),
        points: v.get("points").and_then(|p| p.as_i64()),
        text_html: sanitize_comment(text),
        depth,
        children,
    })
}

/// Parse `https://lobste.rs/s/{id}.json` — a flat list keyed by `indent_level` (§3.7).
pub fn parse_lobsters(v: &Value) -> Option<CommentThread> {
    let short_id = v.get("short_id").and_then(|s| s.as_str()).unwrap_or("");
    let item_url = v
        .get("short_id_url")
        .and_then(|s| s.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://lobste.rs/s/{short_id}"));
    let raw = v.get("comments").and_then(|c| c.as_array())?;

    // Rebuild the tree from indent_level (1 = top level).
    let mut roots: Vec<Comment> = Vec::new();
    // Path of indices into the tree for the current branch.
    let mut path: Vec<usize> = Vec::new();
    let mut total = 0i64;
    for item in raw {
        let text = item
            .get("comment")
            .and_then(|c| c.as_str())
            .or_else(|| item.get("comment_plain").and_then(|c| c.as_str()))
            .unwrap_or("");
        if text.is_empty() {
            continue;
        }
        let author = match item.get("commenting_user") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Object(o)) => o
                .get("username")
                .and_then(|u| u.as_str())
                .unwrap_or("someone")
                .to_string(),
            _ => "someone".to_string(),
        };
        let indent = item
            .get("indent_level")
            .and_then(|i| i.as_i64())
            .unwrap_or(1)
            .max(1) as usize;
        let depth = indent - 1;
        total += 1;
        let comment = Comment {
            author,
            points: item.get("score").and_then(|s| s.as_i64()),
            text_html: sanitize_comment(text),
            depth,
            children: Vec::new(),
        };
        path.truncate(depth);
        if depth == 0 || path.len() < depth {
            path.clear();
            roots.push(comment);
            path.push(roots.len() - 1);
        } else {
            let mut node = &mut roots[path[0]];
            for idx in &path[1..] {
                node = &mut node.children[*idx];
            }
            node.children.push(comment);
            let child_idx = node.children.len() - 1;
            path.push(child_idx);
        }
    }
    sort_by_points(&mut roots);
    let total_comments = v
        .get("comment_count")
        .and_then(|c| c.as_i64())
        .unwrap_or(total);
    Some(CommentThread {
        source: SocialSource::Lobsters,
        item_url,
        total_comments,
        comments: roots,
    })
}

/// Parse `{permalink}.json` — `[post listing, comment listing]` (§3.7).
pub fn parse_reddit(v: &Value, fallback_url: &str) -> Option<CommentThread> {
    let listings = v.as_array()?;
    let post = listings.first();
    let permalink = post
        .and_then(|l| l.pointer("/data/children/0/data/permalink"))
        .and_then(|p| p.as_str())
        .map(|p| format!("https://www.reddit.com{p}"))
        .unwrap_or_else(|| fallback_url.to_string());
    let declared = post
        .and_then(|l| l.pointer("/data/children/0/data/num_comments"))
        .and_then(|n| n.as_i64());

    let children = listings
        .get(1)
        .and_then(|l| l.pointer("/data/children"))
        .and_then(|c| c.as_array())?;

    let mut comments = Vec::new();
    let mut total = 0i64;
    for child in children {
        if let Some(c) = reddit_node(child, 0, &mut total) {
            comments.push(c);
        }
    }
    sort_by_points(&mut comments);
    Some(CommentThread {
        source: SocialSource::Reddit,
        item_url: permalink,
        total_comments: declared.unwrap_or(total),
        comments,
    })
}

fn reddit_node(child: &Value, depth: usize, total: &mut i64) -> Option<Comment> {
    if child.get("kind").and_then(|k| k.as_str()) != Some("t1") {
        return None; // "more" placeholders and the post itself
    }
    let data = child.get("data")?;
    let author = data.get("author").and_then(|a| a.as_str()).unwrap_or("");
    let body = data
        .get("body_html")
        .and_then(|b| b.as_str())
        .map(unescape_entities)
        .or_else(|| {
            data.get("body")
                .and_then(|b| b.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    if author.is_empty() || author == "[deleted]" || body.is_empty() {
        return None;
    }
    *total += 1;
    let mut children = Vec::new();
    if let Some(replies) = data
        .get("replies")
        .and_then(|r| r.pointer("/data/children"))
        && let Some(list) = replies.as_array()
    {
        for reply in list {
            if let Some(c) = reddit_node(reply, depth + 1, total) {
                children.push(c);
            }
        }
    }
    sort_by_points(&mut children);
    Some(Comment {
        author: author.to_string(),
        points: data.get("score").and_then(|s| s.as_i64()),
        text_html: sanitize_comment(&body),
        depth,
        children,
    })
}

fn sort_by_points(comments: &mut [Comment]) {
    // Stable: platform ordering survives when scores are missing or equal.
    comments.sort_by_key(|c| std::cmp::Reverse(c.points.unwrap_or(0)));
}

// ---------------------------------------------------------------------------
// Sanitization and pruning
// ---------------------------------------------------------------------------

/// Sanitize a comment body down to the small tag set the EPUB CSS styles (§3.7).
pub fn sanitize_comment(html: &str) -> String {
    let tags: HashSet<&str> = [
        "p",
        "a",
        "em",
        "i",
        "strong",
        "b",
        "code",
        "pre",
        "blockquote",
        "ul",
        "ol",
        "li",
        "br",
        "del",
        "sup",
        "sub",
    ]
    .into_iter()
    .collect();
    let cleaned = ammonia::Builder::new()
        .tags(tags)
        .link_rel(None)
        .clean(html)
        .to_string();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with('<') {
        trimmed.to_string()
    } else {
        // HN comment bodies start with a bare text run.
        format!("<p>{trimmed}</p>")
    }
}

/// Minimal HTML entity decode — Reddit double-escapes `body_html`.
pub fn unescape_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x200B;", "")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// Plain text of a markup fragment, used for length and word budgeting.
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    unescape_entities(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Ellipsize a comment body to `max_chars` of visible text (§3.7).
pub fn ellipsize_html(html: &str, max_chars: usize) -> String {
    let text = strip_tags(html);
    if text.chars().count() <= max_chars {
        return html.to_string();
    }
    let mut kept: String = text.chars().take(max_chars).collect();
    // Prefer cutting on a word boundary.
    if let Some(idx) = kept.rfind(' ')
        && idx > max_chars * 3 / 4
    {
        kept.truncate(idx);
    }
    format!("<p>{}…</p>", text_escape(kept.trim_end()))
}

fn word_count(html: &str) -> usize {
    strip_tags(html).split_whitespace().count()
}

/// Truncate a tree to the §3.7 limits: top threads by score, depth, children,
/// per-comment length and whole-chapter word budget.
pub fn truncate(comments: Vec<Comment>) -> Vec<Comment> {
    let mut roots: Vec<Comment> = comments;
    sort_by_points(&mut roots);
    roots.truncate(MAX_TOP_LEVEL);
    let mut pruned: Vec<Comment> = roots
        .into_iter()
        .map(|c| prune_node(c, 0))
        .filter(|c| !c.text_html.is_empty())
        .collect();
    let mut budget = MAX_CHAPTER_WORDS;
    trim_to_budget(&mut pruned, &mut budget);
    pruned
}

fn prune_node(mut comment: Comment, depth: usize) -> Comment {
    comment.depth = depth;
    comment.text_html = ellipsize_html(&comment.text_html, MAX_COMMENT_CHARS);
    if depth + 1 >= MAX_DEPTH {
        comment.children = Vec::new();
        return comment;
    }
    let mut children = std::mem::take(&mut comment.children);
    sort_by_points(&mut children);
    children.truncate(MAX_CHILDREN);
    comment.children = children
        .into_iter()
        .map(|c| prune_node(c, depth + 1))
        .filter(|c| !c.text_html.is_empty())
        .collect();
    comment
}

/// Drop comments (depth-first, in render order) once the word budget runs out.
fn trim_to_budget(comments: &mut Vec<Comment>, budget: &mut usize) {
    let mut kept = Vec::with_capacity(comments.len());
    for mut comment in std::mem::take(comments) {
        let cost = word_count(&comment.text_html);
        if cost > *budget {
            break;
        }
        *budget -= cost;
        trim_to_budget(&mut comment.children, budget);
        kept.push(comment);
    }
    *comments = kept;
}

/// Apply the whole-chapter word cap across every source in the chapter (§3.7).
pub fn enforce_chapter_budget(threads: &mut Vec<CommentThread>) {
    let mut budget = MAX_CHAPTER_WORDS;
    for thread in threads.iter_mut() {
        trim_to_budget(&mut thread.comments, &mut budget);
    }
    threads.retain(|t| !t.comments.is_empty());
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Chapter title: "💬 Discussion: {title} ({N} comments on {source})" (§3.7).
pub fn chapter_title(article_title: &str, discussion: &Discussion) -> String {
    let sources: Vec<&str> = discussion
        .threads
        .iter()
        .map(|t| t.source.display_name())
        .collect();
    let sources = if sources.is_empty() {
        "the web".to_string()
    } else {
        sources.join(", ")
    };
    let n = discussion.total_comments();
    let noun = if n == 1 { "comment" } else { "comments" };
    format!("\u{1f4ac} Discussion: {article_title} ({n} {noun} on {sources})")
}

/// Render a discussion to sanitized XHTML for the EPUB (§3.7).
pub fn render_xhtml(discussion: &Discussion, article_title: &str) -> String {
    let mut out = String::new();
    for thread in &discussion.threads {
        out.push_str(&format!(
            "      <h2 class=\"discussion-source\">{}</h2>\n",
            text_escape(&thread_heading(thread))
        ));
        out.push_str(&format!(
            "      <p class=\"discussion-link\"><a href=\"{}\">View the thread \u{2197}</a></p>\n",
            text_escape(&thread.item_url)
        ));
        for comment in &thread.comments {
            render_comment(comment, 3, 0, &mut out);
        }
    }
    if out.is_empty() {
        out.push_str(&format!(
            "      <p>No comments were available for {}.</p>\n",
            text_escape(article_title)
        ));
    }
    out
}

fn thread_heading(thread: &CommentThread) -> String {
    let noun = if thread.total_comments == 1 {
        "comment"
    } else {
        "comments"
    };
    format!(
        "{} \u{00b7} {} {}",
        thread.source.display_name(),
        thread.total_comments,
        noun
    )
}

/// Tag a comment's paragraphs so the X4 can style them without a descendant
/// selector (§3.10). [`sanitize_comment`] allows no attributes on `p`, so every
/// paragraph in a comment body is exactly `<p>`.
fn class_comment_paragraphs(html: &str) -> String {
    html.replace("<p>", "<p class=\"comment-line\">")
}

/// `indent` is cosmetic whitespace; `depth` is the reply nesting level, 0 for a
/// thread's top-level comments.
fn render_comment(comment: &Comment, indent: usize, depth: usize, out: &mut String) {
    let pad = " ".repeat(indent * 2);
    // Nesting is carried as a class rather than left to a descendant selector:
    // the X4's CSS engine only understands `tag`, `.class` and `tag.class`
    // (§3.10), so `blockquote.comment blockquote.comment` never matches there.
    let class = if depth > 0 {
        "comment reply"
    } else {
        "comment"
    };
    out.push_str(&format!("{pad}<blockquote class=\"{class}\">\n"));
    let points = match comment.points {
        Some(p) => format!(" \u{00b7} {p} points"),
        None => String::new(),
    };
    out.push_str(&format!(
        "{pad}  <p class=\"comment-meta\">{}{}</p>\n",
        text_escape(&comment.author),
        text_escape(&points)
    ));
    out.push_str(&format!(
        "{pad}  <div class=\"comment-body\">{}</div>\n",
        class_comment_paragraphs(&to_xhtml(&comment.text_html))
    ));
    for child in &comment.children {
        render_comment(child, indent + 1, depth + 1, out);
    }
    out.push_str(&format!("{pad}</blockquote>\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(author: &str, points: i64, text: &str) -> Comment {
        Comment {
            author: author.into(),
            points: Some(points),
            text_html: format!("<p>{text}</p>"),
            depth: 0,
            children: Vec::new(),
        }
    }

    fn fixture(name: &str) -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let raw = std::fs::read_to_string(&path).expect("fixture must exist");
        serde_json::from_str(&raw).expect("fixture must be json")
    }

    #[test]
    fn parses_the_hn_item_tree() {
        let thread = parse_hn(&fixture("hn_item.json")).expect("hn tree");
        assert_eq!(thread.source, SocialSource::Hn);
        assert_eq!(
            thread.item_url,
            "https://news.ycombinator.com/item?id=40100000"
        );
        assert_eq!(thread.total_comments, 4);
        // Highest-scoring root first.
        assert_eq!(thread.comments[0].author, "alice");
        assert_eq!(thread.comments[0].children.len(), 1);
        assert_eq!(thread.comments[0].children[0].author, "bob");
        assert!(thread.comments[0].text_html.contains("<p>"));
        // The deleted node is dropped but its live reply survives.
        assert!(thread.comments.iter().all(|c| !c.author.is_empty()));
        assert!(thread.comments.iter().any(|c| c.author == "dana"));
    }

    #[test]
    fn parses_lobsters_indent_levels_into_a_tree() {
        let thread = parse_lobsters(&fixture("lobsters_story.json")).expect("lobsters tree");
        assert_eq!(thread.source, SocialSource::Lobsters);
        assert_eq!(thread.item_url, "https://lobste.rs/s/abcdef");
        assert_eq!(thread.total_comments, 4);
        assert_eq!(thread.comments.len(), 2);
        let top = &thread.comments[0];
        assert_eq!(top.author, "pushcx");
        assert_eq!(top.children.len(), 1);
        assert_eq!(top.children[0].children.len(), 1);
        assert_eq!(top.children[0].children[0].author, "third");
    }

    #[test]
    fn parses_reddit_listings_and_skips_more_stubs() {
        let thread = parse_reddit(&fixture("reddit_comments.json"), "").expect("reddit tree");
        assert_eq!(thread.source, SocialSource::Reddit);
        assert_eq!(
            thread.item_url,
            "https://www.reddit.com/r/rust/comments/abc/title/"
        );
        assert_eq!(thread.total_comments, 87);
        assert_eq!(thread.comments.len(), 2);
        assert_eq!(thread.comments[0].author, "ferris");
        assert_eq!(thread.comments[0].children.len(), 1);
        // body_html arrives entity-escaped and must decode into real markup.
        assert!(thread.comments[0].text_html.contains("<p>"));
        assert!(thread.comments[0].text_html.contains("borrow checker"));
        assert!(!thread.comments[0].text_html.contains("&lt;p&gt;"));
        assert!(thread.comments.iter().all(|c| c.author != "[deleted]"));
    }

    #[test]
    fn reddit_url_normalizes_permalinks() {
        assert_eq!(
            reddit_url("/r/rust/comments/abc/title/"),
            "https://www.reddit.com/r/rust/comments/abc/title.json?limit=100&depth=3&sort=top"
        );
        assert_eq!(
            reddit_url("https://www.reddit.com/r/rust/comments/abc/title"),
            "https://www.reddit.com/r/rust/comments/abc/title.json?limit=100&depth=3&sort=top"
        );
    }

    #[test]
    fn sanitizer_strips_scripts_and_wraps_bare_text() {
        let out = sanitize_comment("hello <script>alert(1)</script><b>world</b>");
        assert!(out.starts_with("<p>"));
        assert!(!out.contains("script"));
        assert!(out.contains("<b>world</b>"));
        assert_eq!(sanitize_comment("<p>kept</p>"), "<p>kept</p>");
    }

    #[test]
    fn truncation_applies_every_spec_limit() {
        let mut roots: Vec<Comment> = (0..12)
            .map(|i| leaf(&format!("u{i}"), i as i64, "word ".repeat(10).trim()))
            .collect();
        // Give the top root six children, each with children of their own.
        let mut deep = leaf("deep0", 100, "one");
        for i in 0..6 {
            let mut child = leaf(&format!("c{i}"), i as i64, "two");
            child.children.push(leaf("grandchild", 1, "three"));
            child.children[0].children.push(leaf("too-deep", 1, "four"));
            deep.children.push(child);
        }
        roots.push(deep);

        let out = truncate(roots);
        assert_eq!(out.len(), MAX_TOP_LEVEL, "top-level threads capped");
        assert_eq!(out[0].author, "deep0", "sorted by score, best first");
        assert_eq!(out[0].children.len(), MAX_CHILDREN, "children capped");
        assert_eq!(out[0].children[0].depth, 1);
        assert_eq!(out[0].children[0].children.len(), 1);
        assert_eq!(out[0].children[0].children[0].depth, 2);
        assert!(
            out[0].children[0].children[0].children.is_empty(),
            "rendering stops at depth {MAX_DEPTH}"
        );
    }

    #[test]
    fn per_comment_text_is_ellipsized() {
        let long = "lorem ipsum ".repeat(200);
        let comment = leaf("verbose", 5, &long);
        let out = truncate(vec![comment]);
        let text = strip_tags(&out[0].text_html);
        assert!(text.chars().count() <= MAX_COMMENT_CHARS + 1);
        assert!(out[0].text_html.ends_with("…</p>"));
        // Short comments are left untouched.
        assert_eq!(ellipsize_html("<p>short</p>", 100), "<p>short</p>");
    }

    fn tree_words(comments: &[Comment]) -> usize {
        comments
            .iter()
            .map(|c| word_count(&c.text_html) + tree_words(&c.children))
            .sum()
    }

    fn tree_len(comments: &[Comment]) -> usize {
        comments.iter().map(|c| 1 + tree_len(&c.children)).sum()
    }

    #[test]
    fn chapter_word_budget_is_enforced() {
        // Every comment ellipsizes to ~240 words, so a full 8×4×4 tree is far
        // over the 4,000-word chapter budget.
        let long = "word ".repeat(400);
        let roots: Vec<Comment> = (0..MAX_TOP_LEVEL)
            .map(|i| {
                let mut root = leaf(&format!("u{i}"), 100 - i as i64, &long);
                for j in 0..MAX_CHILDREN {
                    let mut child = leaf(&format!("c{i}{j}"), 10, &long);
                    child.children.push(leaf("grandchild", 1, &long));
                    root.children.push(child);
                }
                root
            })
            .collect();
        let full = tree_len(&roots);

        let out = truncate(roots);
        let total = tree_words(&out);
        assert!(total <= MAX_CHAPTER_WORDS, "{total} words is over budget");
        assert!(!out.is_empty());
        assert!(
            tree_len(&out) < full,
            "comments past the budget are dropped"
        );
    }

    #[test]
    fn renders_nested_blockquotes_and_a_title() {
        let mut root = leaf("alice", 42, "top level");
        root.children.push(leaf("bob", 3, "reply"));
        let discussion = Discussion {
            article_id: 7,
            chapter_id: "disc-1001".into(),
            threads: vec![CommentThread {
                source: SocialSource::Hn,
                item_url: "https://news.ycombinator.com/item?id=1".into(),
                total_comments: 210,
                comments: vec![root],
            }],
        };
        let xhtml = render_xhtml(&discussion, "A Title");
        assert!(xhtml.contains("HN \u{00b7} 210 comments"));
        assert!(xhtml.contains("alice \u{00b7} 42 points"));
        // Top-level comments and replies are distinguishable by class alone, so
        // the X4 needs no descendant selector to indent them (§3.10).
        assert_eq!(xhtml.matches("<blockquote class=\"comment\">").count(), 1);
        assert_eq!(
            xhtml
                .matches("<blockquote class=\"comment reply\">")
                .count(),
            1
        );
        assert_eq!(
            xhtml.matches("</blockquote>").count(),
            2,
            "every blockquote is closed"
        );
        // Comment paragraphs carry their own class for the same reason.
        assert!(
            xhtml.contains("<p class=\"comment-line\">top level</p>"),
            "{xhtml}"
        );
        assert!(!xhtml.contains("<p>"), "an unclassed paragraph survived");
        assert_eq!(
            chapter_title("A Title", &discussion),
            "\u{1f4ac} Discussion: A Title (210 comments on HN)"
        );
    }
}
