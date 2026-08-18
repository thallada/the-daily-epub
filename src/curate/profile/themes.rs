//! Grouping the reader's ~220 Scour interests into readable themes (§3.6a).
//!
//! A lookup table and one function over it. It lives in its own file because the
//! table is long and almost never changes, while the profile logic around it
//! does — and a 260-line const in the middle of that logic is a wall to scroll
//! past, not something to read.

/// Theme name → lowercase keywords, in priority order. The first theme whose
/// keyword appears in the interest name wins, so the table is ordered from the
/// most specific bucket to the most general.
const THEMES: &[(&str, &[&str])] = &[
    (
        "Systems & languages",
        &[
            "rust",
            "zig",
            "lua",
            "assembly",
            "compiler",
            "systems programming",
            "concurrency",
            "async",
            "memory",
            "simd",
            "zero-copy",
            "parser",
            "fuzz",
            "static analysis",
            "functional programming",
            "data structures",
            "algorithm",
            "performance",
            "profil",
            "python",
            "typescript",
            "node.js",
            "wasm",
            "webassembly",
            "reactive programming",
            "lsp",
        ],
    ),
    (
        "Databases & data",
        &[
            "database",
            "sql",
            "postgres",
            "query",
            "vector databases",
            "bloom",
            "compression",
            "data engineering",
            "knowledge graph",
            "message queue",
            "distributed systems",
            "crdt",
            "microservices",
            "system design",
        ],
    ),
    (
        "AI & machine learning",
        &[
            "ai",
            "llm",
            "machine learning",
            "nlp",
            "rag",
            "prompt",
            "agent",
            "claude",
            "eval",
        ],
    ),
    (
        "Web, infra & devtools",
        &[
            "web",
            "css",
            "htmx",
            "svelte",
            "react",
            "pwa",
            "api",
            "developer",
            "devops",
            "docker",
            "git",
            "observability",
            "cloud",
            "cdn",
            "edge",
            "networking",
            "mesh",
            "ipfs",
            "activitypub",
            "atproto",
            "search engines",
            "tauri",
            "design systems",
            "cybersecurity",
            "cryptography",
            "monitoring",
            "spatial computing",
            "webrtc",
            "webgpu",
            "webgl",
            "webcodecs",
            "android",
            "mobile",
            "code generation",
        ],
    ),
    (
        "Self-hosting, RSS & the indie web",
        &[
            "self-host",
            "self host",
            "homelab",
            "rss",
            "feed reader",
            "indie web",
            "personal website",
            "personal wiki",
            "digital garden",
            "static site",
            "static sites",
            "personal archiving",
            "offline-first",
            "privacy",
            "open source",
            "licensing",
            "side projects",
            "digital nomad",
            "remote living",
            "off-grid",
            "quantified self",
            "cloudflare workers",
        ],
    ),
    (
        "E-ink, terminals & hardware",
        &[
            "e-ink",
            "eink",
            "writerdeck",
            "tmux",
            "neovim",
            "terminal",
            "text editor",
            "editor",
            "window manager",
            "keyboard",
            "hardware",
            "electronics",
            "calibre",
            "ghostty",
            "retro computing",
            "low-tech",
            "manufacturing",
            "typography",
            "linux",
        ],
    ),
    (
        "Games & interactive fiction",
        &[
            "game",
            "gaming",
            "minecraft",
            "mud",
            "interactive fiction",
            "inform7",
            "bevy",
            "sim racing",
            "modding",
        ],
    ),
    (
        "Creative coding & digital art",
        &[
            "generative",
            "creative coding",
            "creative automation",
            "glitch",
            "procedural",
            "digital art",
            "gaussian splatting",
            "cellular automata",
            "sonification",
            "code visualization",
            "photo",
            "photography",
            "agent-based",
        ],
    ),
    (
        "Writing, books & PKM",
        &[
            "writing",
            "write",
            "reading",
            "book",
            "essay",
            "poetry",
            "literature",
            "note-taking",
            "journaling",
            "pkm",
            "knowledge management",
            "obsidian",
            "markdown",
            "commonplace",
            "longform",
            "long-form",
            "fiction",
            "worldbuilding",
            "sci-fi",
            "science fiction",
            "speculative",
            "podcast",
            "narrative",
            "choice architecture",
        ],
    ),
    (
        "Science, space & nature",
        &[
            "space",
            "aerospace",
            "aviation",
            "science",
            "neuroscience",
            "bioinformatics",
            "nature",
            "cognitive",
            "social science",
        ],
    ),
    (
        "History, policy & culture",
        &[
            "history",
            "policy",
            "culture",
            "lgbt",
            "queer",
            "startups",
            "apple",
            "boston",
            "criticism",
            "internet history",
            "computing history",
        ],
    ),
    (
        "Outdoors, coffee & everyday life",
        &[
            "hiking",
            "camping",
            "running",
            "trail",
            "coffee",
            "cats",
            "films",
            "music",
            "engineering",
        ],
    ),
];

/// Fallback bucket for interests no keyword claims.
const OTHER_THEME: &str = "Other standing interests";

/// Group ~220 interests into readable themes for the prompt (§3.6).
///
/// Deterministic: theme order follows [`THEMES`], members are sorted
/// case-insensitively, and empty themes are omitted.
pub fn group_into_themes(interests: &[String]) -> Vec<(String, Vec<String>)> {
    let mut buckets: Vec<Vec<String>> = vec![Vec::new(); THEMES.len() + 1];
    for interest in interests {
        let lower = interest.to_lowercase();
        let idx = THEMES
            .iter()
            .position(|(_, keywords)| keywords.iter().any(|k| lower.contains(k)))
            .unwrap_or(THEMES.len());
        buckets[idx].push(interest.clone());
    }
    let mut out = Vec::new();
    for (idx, mut members) in buckets.into_iter().enumerate() {
        if members.is_empty() {
            continue;
        }
        members.sort_by_key(|m| (m.to_lowercase(), m.clone()));
        let name = THEMES.get(idx).map(|(n, _)| *n).unwrap_or(OTHER_THEME);
        out.push((name.to_string(), members));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interests() -> Vec<String> {
        [
            "Rust",
            "Zig",
            "Postgres",
            "SQLite",
            "Kubernetes",
            "E-ink",
            "Keyboards",
            "Retrocomputing",
            "Bicycles",
            "Trail running",
            "Coffee",
            "Bread baking",
            "Cartography",
            "Typography",
            "Ambient music",
            "Science fiction",
            "Local politics",
            "Boston",
            "Machine learning",
            "Self-hosting",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn theming_is_deterministic_and_total() {
        let list = interests();
        let a = group_into_themes(&list);
        let b = group_into_themes(&list);
        assert_eq!(a, b);
        let grouped: usize = a.iter().map(|(_, m)| m.len()).sum();
        assert_eq!(
            grouped,
            list.len(),
            "every interest lands in exactly one theme"
        );
        assert!(a.len() >= 6, "expected several themes, got {}", a.len());
        // Members are sorted within a theme.
        for (_, members) in &a {
            let mut sorted = members.clone();
            sorted.sort_by_key(|m| (m.to_lowercase(), m.clone()));
            assert_eq!(members, &sorted);
        }
    }
}
