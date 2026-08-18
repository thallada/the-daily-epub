//! Generic HTML markup utilities shared across the pipeline.
//!
//! Everything here is about *markup as text*: scanning tags without building a
//! DOM, escaping for XML output, and reading a fragment as prose. It knows
//! nothing about articles, images or EPUBs — those modules build on top of it.
//!
//! There are two ways to look at HTML in this crate. `scraper` parses a real
//! DOM and is the right tool when structure matters (walking up to an enclosing
//! `<figure>`, say). The scanners here walk the string instead, which is what
//! you want when the job is to rewrite tags in place and hand back markup that
//! is otherwise byte-identical.

use scraper::{Html, Node};

/// HTML void elements: XHTML requires them self-closed (§3.10 "valid XHTML").
pub const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

// ---------------------------------------------------------------------------
// Tag scanning
// ---------------------------------------------------------------------------

/// End index (exclusive) of the tag starting at `start` (`html[start] == '<'`),
/// respecting quoted attribute values and comments.
pub fn tag_end(html: &str, start: usize) -> Option<usize> {
    let rest = &html[start..];
    if rest.starts_with("<!--") {
        return rest.find("-->").map(|i| start + i + 3);
    }
    let mut quote: Option<char> = None;
    for (i, c) in rest.char_indices().skip(1) {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"') | (None, '\'') => quote = Some(c),
            (None, '>') => return Some(start + i + c.len_utf8()),
            (None, _) => {}
        }
    }
    None
}

/// Lowercased element name of a tag body such as `img src="…"`.
pub fn tag_name(inner: &str) -> String {
    inner
        .trim_start_matches('/')
        .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Parse `name="value"` pairs out of a tag body, with entity-decoded values.
///
/// Decoding matters: this scanner reads markup that ammonia has serialized, and
/// ammonia writes `&` in a URL as `&amp;`. A raw comparison against a URL that
/// came out of a real HTML parser would never match (§3.10).
pub fn parse_attrs(inner: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let bytes: Vec<char> = inner.chars().collect();
    let mut i = 0;
    // Skip the element name.
    while i < bytes.len() && !bytes[i].is_whitespace() {
        i += 1;
    }
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_whitespace() || bytes[i] == '/') {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && !bytes[i].is_whitespace() && bytes[i] != '=' && bytes[i] != '/' {
            i += 1;
        }
        if i == name_start {
            break;
        }
        let name: String = bytes[name_start..i]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == '=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == '"' || bytes[i] == '\'') {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    value.push(bytes[i]);
                    i += 1;
                }
                i += 1;
            } else {
                while i < bytes.len() && !bytes[i].is_whitespace() && bytes[i] != '>' {
                    value.push(bytes[i]);
                    i += 1;
                }
            }
        }
        attrs.push((name, decode_entities(&value)));
    }
    attrs
}

/// Decode the handful of entities an HTML serializer emits inside attributes.
///
/// Numeric forms are included because feeds and WordPress write `&#038;` for
/// `&`; anything else is left alone rather than guessed at.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let Some(end) = tail[..tail.len().min(12)].find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| match n.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => n.parse::<u32>().ok(),
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Escape a string for use inside a double-quoted XML attribute.
pub fn attr_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escape a string for XML text content.
pub fn text_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// XHTML output
// ---------------------------------------------------------------------------

/// Self-close HTML void elements and normalize `&nbsp;` so the markup parses as
/// XML — EPUB3 content documents are XHTML (§3.10).
pub fn to_xhtml(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    while let Some(rel) = html[cursor..].find('<') {
        let start = cursor + rel;
        out.push_str(&html[cursor..start]);
        let Some(end) = tag_end(html, start) else {
            out.push_str(&html[start..]);
            cursor = html.len();
            break;
        };
        let raw = &html[start..end];
        let inner = raw.trim_start_matches('<').trim_end_matches('>');
        let name = tag_name(inner);
        if VOID_ELEMENTS.contains(&name.as_str()) && !inner.trim_end().ends_with('/') {
            out.push('<');
            out.push_str(inner.trim_end());
            out.push_str("/>");
        } else {
            out.push_str(raw);
        }
        cursor = end;
    }
    out.push_str(&html[cursor..]);
    // html5ever (via ammonia) emits `&nbsp;`, which is undefined in XML.
    out.replace("&nbsp;", "&#160;")
}

// ---------------------------------------------------------------------------
// Reading markup as text
// ---------------------------------------------------------------------------

/// Visible text of an HTML fragment, entities decoded, `script`/`style` skipped.
pub fn html_to_text(html: &str) -> String {
    let document = Html::parse_fragment(html);
    let mut out = String::with_capacity(html.len() / 2);
    for node in document.tree.nodes() {
        let Node::Text(text) = node.value() else {
            continue;
        };
        let hidden = node.ancestors().any(|a| match a.value() {
            Node::Element(e) => matches!(e.name(), "script" | "style" | "noscript"),
            _ => false,
        });
        if hidden {
            continue;
        }
        out.push_str(text);
        out.push(' ');
    }
    out
}

/// Count words in rendered text (tags stripped) (§3.3).
pub fn word_count(html: &str) -> i64 {
    html_to_text(html)
        .split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .count() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_scanner_finds_the_end_of_awkward_tags() {
        // A `>` inside a quoted attribute is not the end of the tag.
        let html = r#"<a title="a > b">x</a>"#;
        assert_eq!(
            tag_end(html, 0),
            Some(17),
            "past the closing `>`, not the one in the title"
        );
        // Comments end at `-->`, not at the first `>`.
        let comment = "<!-- a > b -->rest";
        assert_eq!(tag_end(comment, 0), Some(14));
        // An unterminated tag has no end.
        assert_eq!(tag_end("<p class=\"x", 0), None);
    }

    #[test]
    fn tag_names_are_lowercased_and_stripped() {
        assert_eq!(tag_name("IMG src=\"x\""), "img");
        assert_eq!(tag_name("/DIV"), "div");
        assert_eq!(tag_name("br/"), "br");
        assert_eq!(tag_name(""), "");
    }

    #[test]
    fn attributes_parse_in_every_spelling() {
        let attrs = parse_attrs(r#"img src="a.png" ALT='an alt' loading=lazy hidden"#);
        let get = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("src"), Some("a.png"));
        assert_eq!(
            get("alt"),
            Some("an alt"),
            "names lowercase, quotes either way"
        );
        assert_eq!(get("loading"), Some("lazy"), "unquoted values work");
        assert_eq!(get("hidden"), Some(""), "valueless attributes are empty");
    }

    #[test]
    fn attribute_values_come_back_entity_decoded() {
        // The reason this matters: a serializer writes `&` in a URL as `&amp;`,
        // and callers compare against URLs a real parser produced.
        let attrs = parse_attrs(r#"img src="a.jpg?id=1&amp;w=9&#038;h=2""#);
        assert_eq!(attrs[0].1, "a.jpg?id=1&w=9&h=2");
    }

    #[test]
    fn entity_decoding_covers_named_decimal_and_hex() {
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("&#038;&#x26;"), "&&");
        assert_eq!(decode_entities("&lt;p&gt;"), "<p>");
        assert_eq!(decode_entities("caf&#233;"), "café");
        // Nothing to do, and nothing invented for what we do not know.
        assert_eq!(decode_entities("plain"), "plain");
        assert_eq!(decode_entities("&unknown; &"), "&unknown; &");
    }

    #[test]
    fn escaping_is_the_inverse_that_output_needs() {
        assert_eq!(
            attr_escape(r#"a & "b" <c>"#),
            "a &amp; &quot;b&quot; &lt;c&gt;"
        );
        // Text content keeps quotes as they are.
        assert_eq!(text_escape(r#"a & "b" <c>"#), r#"a &amp; "b" &lt;c&gt;"#);
    }

    #[test]
    fn to_xhtml_self_closes_voids_and_entities() {
        let html = "<p>a<br>b<hr>c&nbsp;d<img src=\"x.png\" alt=\"y\"></p><p>e<br/></p>";
        let out = to_xhtml(html);
        assert!(out.contains("<br/>"));
        assert!(out.contains("<hr/>"));
        assert!(out.contains("<img src=\"x.png\" alt=\"y\"/>"));
        assert!(out.contains("&#160;"));
        assert!(!out.contains("&nbsp;"));
        assert!(!out.contains("<br/ >"));
        // Already-closed voids are left alone (no double slash).
        assert_eq!(out.matches("<br/>").count(), 2);
    }

    #[test]
    fn to_xhtml_ignores_angle_brackets_in_attributes() {
        let html = r#"<a title="a > b">x</a>"#;
        assert_eq!(to_xhtml(html), html);
    }

    #[test]
    fn word_count_ignores_markup_and_script() {
        assert_eq!(word_count("<p>one two three</p>"), 3);
        assert_eq!(word_count("<p>a</p><script>b c d e</script>"), 1);
        assert_eq!(word_count("<p>&amp; &mdash; ok</p>"), 1);
        assert_eq!(word_count(""), 0);
        assert_eq!(word_count("<p></p>"), 0);
    }
}
