//! Xteink X4 edition transforms and the XTC converter invocation
//! (spec §3.10 X4 edition, §3.11).
//!
//! The converter has no global npm bin: it is run as
//! `node <repo>/cli/index.js convert <in.epub> -o <out.xtch> -f xtch [-c settings.json]`
//! (implementation notes, verified facts). A missing or failing converter is
//! non-fatal — XTC is a bonus artifact.

use std::path::{Path, PathBuf};

use crate::config::XtcConfig;

use super::images::tag_end;

/// Native X4 screen size, used for the cover and image fitting (§3.10).
pub const X4_SCREEN: (u32, u32) = (480, 800);

/// Attributes that let a document lay itself out — dropped for the X4 (§3.10).
pub const DROPPED_ATTRIBUTES: &[&str] = &[
    "style", "align", "width", "height", "srcset", "sizes", "loading", "hspace", "vspace", "border",
];

/// Longest unbroken run of non-whitespace the X4 firmware will lay out; past
/// this it stops wrapping and the line runs off the 480px screen (§3.10).
///
/// Real text never gets near 200 characters — this is for minified source in a
/// code block and for bare URLs pasted into comment threads.
pub const MAX_WORD_CHARS: usize = 200;

/// U+00AD, invisible unless the renderer actually needs to break there.
const SOFT_HYPHEN: char = '\u{00ad}';

/// Elements whose content is code, not prose, and must be copied through
/// untouched — a soft hyphen inside a stylesheet would corrupt it.
const RAW_TEXT_ELEMENTS: &[&str] = &["script", "style"];

/// Declarations the X4 renderer cannot honor (§3.10).
const DROPPED_PROPERTIES: &[&str] = &[
    "float",
    "clear",
    "position",
    "z-index",
    "box-shadow",
    "text-shadow",
    "transform",
    "columns",
    "column-count",
    "column-gap",
    "letter-spacing",
];

#[derive(Debug, thiserror::Error)]
pub enum XtcError {
    #[error("could not run `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("converter exited with status {status}: {stderr}")]
    Failed { status: i32, stderr: String },
    #[error("converter produced no output at {0}")]
    NoOutput(PathBuf),
}

/// Simplify CSS for the X4: no floats/flex/grid, no embedded fonts, larger base
/// font, generous line-height, hyphenation on (§3.10).
pub fn simplify_css(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(open) = rest.find('{') {
        let selector = &rest[..open];
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        let body = &rest[open + 1..open + close];
        rest = &rest[open + close + 1..];

        // `@font-face` (and any other embedded-font rule) is dropped wholesale.
        if selector.to_ascii_lowercase().contains("@font-face") {
            continue;
        }
        let kept: Vec<&str> = body
            .split(';')
            .filter(|decl| !decl.trim().is_empty())
            .filter(|decl| !is_dropped_declaration(decl))
            .collect();
        if kept.is_empty() {
            continue;
        }
        out.push_str(selector.trim_start_matches('\n'));
        out.push('{');
        for decl in kept {
            out.push_str(decl);
            out.push(';');
        }
        out.push('}');
        out.push('\n');
    }
    out
}

fn is_dropped_declaration(decl: &str) -> bool {
    let Some((property, value)) = decl.split_once(':') else {
        return true;
    };
    let property = property.trim().to_ascii_lowercase();
    let value = value.trim().to_ascii_lowercase();
    if DROPPED_PROPERTIES.contains(&property.as_str()) {
        return true;
    }
    if property == "display" && (value.contains("flex") || value.contains("grid")) {
        return true;
    }
    if property.starts_with("flex") || property.starts_with("grid") {
        return true;
    }
    if property == "font-family" && value.contains("url(") {
        return true;
    }
    false
}

/// Strip layout constructs the X4 renderer handles poorly from chapter markup,
/// then soft-hyphenate anything too long for it to wrap (§3.10).
pub fn simplify_xhtml(xhtml: &str) -> String {
    break_long_words(&strip_attributes(xhtml, DROPPED_ATTRIBUTES))
}

/// Insert soft hyphens into words longer than [`MAX_WORD_CHARS`], in text
/// content only (§3.10).
fn break_long_words(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    while let Some(rel) = html[cursor..].find('<') {
        let start = cursor + rel;
        soften_text(&html[cursor..start], &mut out);
        let Some(end) = tag_end(html, start) else {
            out.push_str(&html[start..]);
            return out;
        };
        let tag = &html[start..end];
        out.push_str(tag);
        cursor = end;
        // `<style>`/`<script>` bodies are not prose: copy to the closing tag verbatim.
        if let Some(name) = raw_text_name(tag)
            && let Some(close) = find_close_tag(html, cursor, name)
        {
            out.push_str(&html[cursor..close]);
            cursor = close;
        }
    }
    soften_text(&html[cursor..], &mut out);
    out
}

/// The element name when `tag` opens a raw-text element, else `None`.
fn raw_text_name(tag: &str) -> Option<&'static str> {
    let rest = tag.strip_prefix('<')?;
    if rest.starts_with('/') {
        return None;
    }
    RAW_TEXT_ELEMENTS.iter().copied().find(|name| {
        rest.len() >= name.len()
            && rest[..name.len()].eq_ignore_ascii_case(name)
            // Only `<style>` and `<style type=…>`, never `<styled-thing>`.
            && rest[name.len()..]
                .starts_with([' ', '\t', '\n', '\r', '>', '/'])
    })
}

/// Byte offset of `</name` at or after `from`, else `None`.
fn find_close_tag(html: &str, from: usize, name: &str) -> Option<usize> {
    let needle = format!("</{name}");
    let hay = html.get(from..)?.to_ascii_lowercase();
    hay.find(&needle).map(|i| from + i)
}

/// Copy `text` into `out`, soft-hyphenating any over-long word.
fn soften_text(text: &str, out: &mut String) {
    // Byte length bounds character count, so a short run holds no long word.
    if text.len() <= MAX_WORD_CHARS {
        out.push_str(text);
        return;
    }
    let mut word_start = 0usize;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            push_soft_hyphenated(&text[word_start..i], out);
            out.push(c);
            word_start = i + c.len_utf8();
        }
    }
    push_soft_hyphenated(&text[word_start..], out);
}

fn push_soft_hyphenated(word: &str, out: &mut String) {
    if word.len() <= MAX_WORD_CHARS {
        out.push_str(word);
        return;
    }
    let mut units = 0usize;
    let mut rest = word;
    while !rest.is_empty() {
        if units == MAX_WORD_CHARS {
            out.push(SOFT_HYPHEN);
            units = 0;
        }
        let take =
            entity_len(rest).unwrap_or_else(|| rest.chars().next().map_or(1, char::len_utf8));
        out.push_str(&rest[..take]);
        rest = &rest[take..];
        units += 1;
    }
}

/// Byte length of the `&…;` reference starting `s`, if there is one.
///
/// A character reference is one unit: splitting `&amp;` down the middle would
/// turn it into literal text and break the XHTML.
fn entity_len(s: &str) -> Option<usize> {
    /// `&thickapprox;` is 13 bytes; nothing we emit is longer.
    const MAX_ENTITY_BYTES: usize = 16;
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'&') {
        return None;
    }
    bytes
        .iter()
        .take(MAX_ENTITY_BYTES)
        .position(|&b| b == b';')
        .map(|p| p + 1)
}

/// Remove the named attributes from every tag, leaving the rest verbatim.
fn strip_attributes(html: &str, drop: &[&str]) -> String {
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    while let Some(rel) = html[cursor..].find('<') {
        let start = cursor + rel;
        out.push_str(&html[cursor..start]);
        let Some(end) = tag_end(html, start) else {
            out.push_str(&html[start..]);
            return out;
        };
        out.push_str(&filter_tag(&html[start..end], drop));
        cursor = end;
    }
    out.push_str(&html[cursor..]);
    out
}

/// `<p style="x" class="y">` → `<p class="y">`.
fn filter_tag(tag: &str, drop: &[&str]) -> String {
    if tag.starts_with("<!") || tag.starts_with("<?") || tag.starts_with("</") {
        return tag.to_string();
    }
    let bytes = tag.as_bytes();
    let mut out = String::with_capacity(tag.len());
    let mut i = 1; // past '<'
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
        i += 1;
    }
    out.push_str(&tag[..i]);

    while i < bytes.len() {
        let ws_start = i;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] == b'>' || bytes[i] == b'/' {
            out.push_str(&tag[ws_start..]);
            return out;
        }
        let name_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'>'
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let name = tag[name_start..i].to_ascii_lowercase();
        let mut after_name = i;
        while after_name < bytes.len() && bytes[after_name].is_ascii_whitespace() {
            after_name += 1;
        }
        if after_name < bytes.len() && bytes[after_name] == b'=' {
            i = after_name + 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
            } else {
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                    i += 1;
                }
            }
        }
        if !drop.contains(&name.as_str()) {
            out.push_str(&tag[ws_start..i]);
        }
    }
    out
}

/// Full argv for the converter: `command` + `args` + `<input> -o <output> -f <format>`
/// (+ `-c <settings>` when configured) (§3.11).
pub fn build_command(cfg: &XtcConfig, input: &Path, output: &Path) -> (String, Vec<String>) {
    let mut args = cfg.args.clone();
    args.push(input.display().to_string());
    args.push("-o".to_string());
    args.push(output.display().to_string());
    args.push("-f".to_string());
    args.push(cfg.format.as_str().to_string());
    if let Some(settings) = &cfg.settings {
        args.push("-c".to_string());
        args.push(settings.display().to_string());
    }
    (cfg.command.clone(), args)
}

/// Output path for an input EPUB: `{out_dir}/{stem}.{xtc|xtch}` (§3.11).
pub fn output_path(cfg: &XtcConfig, input: &Path, out_dir: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "issue".to_string());
    out_dir.join(format!("{stem}.{}", cfg.format.extension()))
}

/// Convert the X4 EPUB to `.xtc`/`.xtch` via `tokio::process::Command` (§3.11).
///
/// Callers treat every error as a warning and continue — XTC is a bonus
/// artifact, the X4 can always fall back to the X4 EPUB from BookOrbit.
pub async fn convert(cfg: &XtcConfig, input: &Path, out_dir: &Path) -> Result<PathBuf, XtcError> {
    if cfg.settings.is_none() {
        // The converter refuses to start without `font.path`, which can only be
        // supplied through the settings JSON: `-c` is mandatory in practice even
        // though the flag is optional.
        tracing::warn!(
            "xtc.settings is unset; epub-to-xtc-converter requires a settings \
             file with a font.path and will refuse to run without one"
        );
    }
    let output = output_path(cfg, input, out_dir);
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| XtcError::Spawn {
            command: parent.display().to_string(),
            source: e,
        })?;
    }
    let (command, args) = build_command(cfg, input, &output);
    tracing::info!(command, ?args, "running the xtc converter");

    let result = tokio::process::Command::new(&command)
        .args(&args)
        .output()
        .await
        .map_err(|e| XtcError::Spawn {
            command: command.clone(),
            source: e,
        })?;
    if !result.status.success() {
        return Err(XtcError::Failed {
            status: result.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&result.stderr)
                .lines()
                .take(5)
                .collect::<Vec<_>>()
                .join(" | "),
        });
    }
    if !output.exists() {
        return Err(XtcError::NoOutput(output));
    }
    tracing::info!(path = %output.display(), "xtc conversion complete");
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::XtcFormat;

    fn cfg() -> XtcConfig {
        XtcConfig {
            enabled: true,
            command: "node".into(),
            args: vec![
                "/opt/epub-to-xtc-converter/cli/index.js".into(),
                "convert".into(),
            ],
            format: XtcFormat::Xtch,
            settings: None,
        }
    }

    #[test]
    fn builds_the_documented_converter_argv() {
        let (command, args) = build_command(
            &cfg(),
            Path::new("/out/The Daily EPUB - 2026-08-15 (X4).epub"),
            Path::new("/xtc/The Daily EPUB - 2026-08-15 (X4).xtch"),
        );
        assert_eq!(command, "node");
        assert_eq!(
            args,
            vec![
                "/opt/epub-to-xtc-converter/cli/index.js",
                "convert",
                "/out/The Daily EPUB - 2026-08-15 (X4).epub",
                "-o",
                "/xtc/The Daily EPUB - 2026-08-15 (X4).xtch",
                "-f",
                "xtch",
            ]
        );
    }

    #[test]
    fn settings_file_is_passed_with_dash_c() {
        let mut cfg = cfg();
        cfg.settings = Some(PathBuf::from("/etc/xtc.json"));
        cfg.format = XtcFormat::Xtc;
        let (_, args) = build_command(&cfg, Path::new("in.epub"), Path::new("out.xtc"));
        assert_eq!(args[args.len() - 4..], ["-f", "xtc", "-c", "/etc/xtc.json"]);
    }

    #[test]
    fn output_path_follows_the_format_extension() {
        let out = output_path(&cfg(), Path::new("/out/Issue (X4).epub"), Path::new("/xtc"));
        assert_eq!(out, PathBuf::from("/xtc/Issue (X4).xtch"));
    }

    /// The pipeline turns every one of these into a report warning, so the error
    /// has to say which of them happened (§3.11).
    #[tokio::test]
    async fn converter_failures_are_distinguishable() {
        let dir = tempfile::tempdir().unwrap();

        let mut missing = cfg();
        missing.command = "definitely-not-a-real-binary-9f3b".into();
        missing.args.clear();
        match convert(&missing, Path::new("in.epub"), dir.path()).await {
            Err(XtcError::Spawn { command, .. }) => {
                assert_eq!(command, "definitely-not-a-real-binary-9f3b")
            }
            other => panic!("expected a spawn failure, got {other:?}"),
        }

        let mut failing = cfg();
        failing.command = "false".into();
        failing.args.clear();
        match convert(&failing, Path::new("in.epub"), dir.path()).await {
            Err(XtcError::Failed { status, .. }) => assert_ne!(status, 0),
            other => panic!("expected a nonzero exit, got {other:?}"),
        }

        // Exit 0 but nothing written is its own error, not a silent success.
        let mut silent = cfg();
        silent.command = "true".into();
        silent.args.clear();
        match convert(&silent, Path::new("in.epub"), dir.path()).await {
            Err(XtcError::NoOutput(path)) => assert_eq!(path, dir.path().join("in.xtch")),
            other => panic!("expected NoOutput, got {other:?}"),
        }
    }

    #[test]
    fn css_simplification_drops_layout_and_fonts() {
        let css = r#"
@font-face { font-family: "Serif"; src: url(serif.woff2); }
.a { float: left; color: #000; }
.b { display: flex; flex-direction: row; }
.c { position: absolute; margin: 1em; }
.d { float: right; }
"#;
        let out = simplify_css(css);
        assert!(!out.contains("@font-face"));
        assert!(!out.contains("float"));
        assert!(!out.contains("flex"));
        assert!(!out.contains("position"));
        assert!(out.contains("color: #000"));
        assert!(out.contains("margin: 1em"));
        // A rule left with no declarations disappears entirely.
        assert!(!out.contains(".d"));
    }

    #[test]
    fn xhtml_simplification_drops_layout_attributes_only() {
        let input = r#"<p class="meta" style="float:left" align="center">a &amp; b</p><img src="x.jpg" alt="An x" width="900"/>"#;
        let out = simplify_xhtml(input);
        assert_eq!(
            out,
            r#"<p class="meta">a &amp; b</p><img src="x.jpg" alt="An x"/>"#
        );
    }

    /// The shipped X4 stylesheet must already satisfy the X4 rules, so the
    /// simplifier is a no-op over it (§3.10).
    #[test]
    fn the_shipped_x4_stylesheet_is_already_simplified() {
        let css = super::super::build::stylesheet(crate::types::Edition::X4);
        let simplified = simplify_css(css);
        assert_eq!(
            css.matches(';').count(),
            simplified.matches(';').count(),
            "the simplifier dropped a declaration from style-x4.css"
        );
        // Declarations only — the file's header comment mentions what it avoids.
        for banned in [
            "float:",
            "clear:",
            "display: flex",
            "display: grid",
            "position:",
            "@font-face",
        ] {
            assert!(!css.contains(banned), "style-x4.css must not use {banned}");
        }
    }

    /// The firmware stops wrapping past 200 characters and the line runs off
    /// the screen, so long tokens get soft hyphens (§3.10).
    #[test]
    fn over_long_words_are_soft_hyphenated() {
        let long = "a".repeat(450);
        let out = simplify_xhtml(&format!("<p>short {long} tail</p>"));
        assert_eq!(out.matches(SOFT_HYPHEN).count(), 2);
        // Only the long token is touched; the rest of the line is byte-identical.
        assert!(out.starts_with("<p>short "));
        assert!(out.ends_with(" tail</p>"));
        assert!(!out.contains(&format!("short{SOFT_HYPHEN}")));
        // Removing the hyphens gets the original word back — nothing was lost.
        assert!(out.replace(SOFT_HYPHEN, "").contains(&long));
        // Every run between hyphens is within the limit.
        for run in out.replace(['<', '>'], " ").split_whitespace() {
            for piece in run.split(SOFT_HYPHEN) {
                assert!(piece.chars().count() <= MAX_WORD_CHARS, "{}", piece.len());
            }
        }
        // Words at the limit are left alone.
        let exact = "b".repeat(MAX_WORD_CHARS);
        assert_eq!(
            simplify_xhtml(&format!("<p>{exact} {exact}</p>")),
            format!("<p>{exact} {exact}</p>")
        );
    }

    /// A soft hyphen dropped into `&amp;` would turn it into literal text and
    /// break the XHTML, so character references are indivisible (§3.10).
    #[test]
    fn entities_and_markup_survive_word_breaking() {
        // 120 entities: the raw string is far past the limit, but it is only
        // 120 units, so no break is due — and the entities stay intact.
        let entities = "&amp;".repeat(120);
        let out = simplify_xhtml(&format!("<p>{entities}</p>"));
        assert!(!out.contains(SOFT_HYPHEN));
        assert_eq!(out.matches("&amp;").count(), 120);

        // Past the limit the breaks land between entities, never inside one.
        let out = simplify_xhtml(&format!("<p>{}</p>", "&amp;".repeat(260)));
        assert_eq!(out.matches("&amp;").count(), 260);
        assert_eq!(out.matches(SOFT_HYPHEN).count(), 1);
        assert!(!out.contains(&format!("&amp{SOFT_HYPHEN}")));
        assert!(!out.contains(&format!("&{SOFT_HYPHEN}")));

        // Attribute values are not text content and must not be rewritten.
        let href = "https://example.com/".to_string() + &"z".repeat(300);
        let out = simplify_xhtml(&format!("<p><a href=\"{href}\">link</a></p>"));
        assert!(out.contains(&format!("href=\"{href}\"")), "{out}");
        assert!(!out.contains(SOFT_HYPHEN));
    }

    /// Stylesheets and scripts are code: a soft hyphen inside one corrupts it.
    #[test]
    fn raw_text_elements_are_copied_through_verbatim() {
        let css = format!("p{{content:\"{}\"}}", "x".repeat(400));
        let out = simplify_xhtml(&format!("<style type=\"text/css\">{css}</style>"));
        assert!(out.contains(&css), "{out}");
        assert!(!out.contains(SOFT_HYPHEN));

        // A tag that merely starts with the same letters is ordinary prose.
        let long = "y".repeat(400);
        let out = simplify_xhtml(&format!("<styled-note>{long}</styled-note>"));
        assert_eq!(out.matches(SOFT_HYPHEN).count(), 1);
    }

    /// `/* … */` runs, which are prose and may contain anything.
    fn strip_css_comments(css: &str) -> String {
        let mut out = String::with_capacity(css.len());
        let mut rest = css;
        while let Some(open) = rest.find("/*") {
            out.push_str(&rest[..open]);
            match rest[open + 2..].find("*/") {
                Some(close) => rest = &rest[open + 4 + close..],
                None => return out,
            }
        }
        out.push_str(rest);
        out
    }

    /// The X4's CSS engine understands `tag`, `.class` and `tag.class` only —
    /// a descendant combinator silently drops the whole rule (§3.10).
    #[test]
    fn the_x4_stylesheet_uses_no_descendant_selectors() {
        let css = strip_css_comments(super::super::build::stylesheet(crate::types::Edition::X4));
        for (i, _) in css.match_indices('{') {
            let selector_list = css[..i].rsplit('}').next().unwrap_or_default().trim();
            for selector in selector_list.split(',') {
                let selector = selector.trim();
                if selector.is_empty() || selector.starts_with('@') {
                    continue;
                }
                assert!(
                    !selector.contains(char::is_whitespace),
                    "descendant selector {selector:?} will not match on the X4"
                );
                for combinator in ['>', '+', '~'] {
                    assert!(
                        !selector.contains(combinator),
                        "combinator {combinator:?} in {selector:?} is unsupported on the X4"
                    );
                }
            }
        }
    }

    #[test]
    fn simplification_leaves_prologue_and_text_untouched() {
        let input =
            "<?xml version=\"1.0\"?>\n<!DOCTYPE html>\n<html><body><p>2 &lt; 3</p></body></html>";
        assert_eq!(simplify_xhtml(input), input);
    }
}
