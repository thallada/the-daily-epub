//! BookOrbit OPDS catalog lookup support.
//!
//! BookOrbit is the companion library and browser-based EPUB reader used by
//! this service. Its OPDS catalog is preferable to its JSON API here because
//! OPDS uses static HTTP Basic credentials and therefore needs no JWT/refresh
//! token lifecycle. Searches use `/api/v1/opds/catalog?q=<date>`, acquisition
//! links expose `/api/v1/opds/<book_id>/download?fileId=<file_id>`, and browser
//! links use `/read/<book_id>/<file_id>`. See
//! `docs/plans/2026-09-05-bookorbit-read-link.md` for the integration design.

use jiff::civil::Date;
use reqwest::header::ACCEPT;

const ACQUISITION_REL: &str = "http://opds-spec.org/acquisition";
const DOWNLOAD_PREFIX: &str = "/api/v1/opds/";

/// The BookOrbit book and file identifiers required by its reader route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookorbitIds {
    /// BookOrbit's identifier for the issue's book record.
    pub book_id: i64,
    /// BookOrbit's identifier for the EPUB file attached to the book.
    pub file_id: i64,
}

/// A failure while querying or parsing BookOrbit's OPDS catalog.
#[derive(Debug, thiserror::Error)]
pub enum BookorbitError {
    /// The OPDS endpoint could not be reached.
    #[error("BookOrbit unreachable: {0}")]
    Unreachable(#[source] reqwest::Error),
    /// BookOrbit rejected the configured OPDS Basic credentials.
    #[error("BookOrbit rejected the OPDS credentials")]
    Unauthorized,
    /// BookOrbit returned an unexpected non-success status.
    #[error("BookOrbit returned HTTP {0}")]
    Status(reqwest::StatusCode),
    /// BookOrbit returned a body that could not be read as the expected feed.
    #[error("BookOrbit returned an unreadable OPDS feed: {0}")]
    Malformed(String),
}

/// Search BookOrbit's OPDS catalog for the Standard edition of the issue dated `date`.
///
/// `api_url` must have no trailing slash. `Ok(None)` means BookOrbit has not
/// indexed the issue yet.
pub async fn find_issue(
    client: &reqwest::Client,
    api_url: &str,
    opds_user: &str,
    opds_pass: &str,
    issue_title: &str,
    date: Date,
) -> Result<Option<BookorbitIds>, BookorbitError> {
    let response = client
        .get(format!("{api_url}/api/v1/opds/catalog"))
        .query(&[("q", date.to_string())])
        .basic_auth(opds_user, Some(opds_pass))
        .header(ACCEPT, "application/atom+xml")
        .send()
        .await
        .map_err(BookorbitError::Unreachable)?;

    let status = response.status();
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return Err(BookorbitError::Unauthorized);
    }
    if !status.is_success() {
        return Err(BookorbitError::Status(status));
    }

    let feed_xml = response.text().await.map_err(|error| {
        if error.is_timeout() || error.is_connect() {
            BookorbitError::Unreachable(error)
        } else {
            BookorbitError::Malformed(error.to_string())
        }
    })?;
    let ids = select_issue_entry(&feed_xml, issue_title, date)?;
    if let Some(ids) = ids {
        tracing::info!(
            book_id = ids.book_id,
            file_id = ids.file_id,
            "resolved BookOrbit issue"
        );
    }
    Ok(ids)
}

/// Select the Standard issue from an Atom feed without performing network I/O.
pub fn select_issue_entry(
    feed_xml: &str,
    issue_title: &str,
    date: Date,
) -> Result<Option<BookorbitIds>, BookorbitError> {
    if find_open_tag(feed_xml, "feed", 0).is_none() {
        return Err(BookorbitError::Malformed(
            "response does not contain an Atom <feed> element".to_string(),
        ));
    }

    let fallback_title = format!("The Daily EPUB - {date}");
    let mut fallback_entry = None;

    for entry in entry_bodies(feed_xml) {
        let Some(title) = element_text(entry, "title") else {
            continue;
        };
        let title = xml_unescape(title);
        let title = title.trim();
        if title.ends_with("(X4)") {
            continue;
        }
        if title == issue_title {
            return acquisition_ids(entry);
        }
        if title == fallback_title && fallback_entry.is_none() {
            fallback_entry = Some(entry);
        }
    }

    fallback_entry.map_or(Ok(None), acquisition_ids)
}

/// Build the public BookOrbit reader URL for `ids`.
///
/// `public_url` must have no trailing slash.
pub fn reader_url(public_url: &str, ids: BookorbitIds) -> String {
    format!("{public_url}/read/{}/{}", ids.book_id, ids.file_id)
}

fn entry_bodies(feed_xml: &str) -> Vec<&str> {
    let mut entries = Vec::new();
    let mut cursor = 0;

    while let Some(start) = find_open_tag(feed_xml, "entry", cursor) {
        let Some(open_end_offset) = feed_xml[start..].find('>') else {
            break;
        };
        let body_start = start + open_end_offset + 1;
        let Some(close_offset) = feed_xml[body_start..].find("</entry>") else {
            break;
        };
        let body_end = body_start + close_offset;
        entries.push(&feed_xml[body_start..body_end]);
        cursor = body_end + "</entry>".len();
    }

    entries
}

fn element_text<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let start = find_open_tag(xml, name, 0)?;
    let open_end = start + xml[start..].find('>')?;
    let text_start = open_end + 1;
    let close = format!("</{name}>");
    let text_end = text_start + xml[text_start..].find(&close)?;
    Some(&xml[text_start..text_end])
}

fn find_open_tag(xml: &str, name: &str, mut cursor: usize) -> Option<usize> {
    let needle = format!("<{name}");
    while let Some(offset) = xml[cursor..].find(&needle) {
        let start = cursor + offset;
        let after_name = start + needle.len();
        if xml
            .as_bytes()
            .get(after_name)
            .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'>' | b'/'))
        {
            return Some(start);
        }
        cursor = after_name;
    }
    None
}

fn acquisition_ids(entry: &str) -> Result<Option<BookorbitIds>, BookorbitError> {
    let mut cursor = 0;
    let mut saw_acquisition = false;

    while let Some(start) = find_open_tag(entry, "link", cursor) {
        let Some(end_offset) = entry[start..].find('>') else {
            break;
        };
        let end = start + end_offset;
        let attributes = &entry[start + "<link".len()..end];
        if attribute_value(attributes, "rel") == Some(ACQUISITION_REL) {
            saw_acquisition = true;
            if let Some(href) = attribute_value(attributes, "href") {
                let href = xml_unescape(href);
                if let Some(ids) = parse_download_href(&href) {
                    return Ok(Some(ids));
                }
            }
        }
        cursor = end + 1;
    }

    if saw_acquisition {
        Err(BookorbitError::Malformed(
            "qualifying entry has an invalid acquisition href".to_string(),
        ))
    } else {
        Ok(None)
    }
}

fn attribute_value<'a>(attributes: &'a str, wanted: &str) -> Option<&'a str> {
    let bytes = attributes.as_bytes();
    let mut cursor = 0;

    while cursor < bytes.len() {
        while cursor < bytes.len() && (bytes[cursor].is_ascii_whitespace() || bytes[cursor] == b'/')
        {
            cursor += 1;
        }
        let name_start = cursor;
        while cursor < bytes.len()
            && !bytes[cursor].is_ascii_whitespace()
            && !matches!(bytes[cursor], b'=' | b'/' | b'>')
        {
            cursor += 1;
        }
        if name_start == cursor {
            cursor += 1;
            continue;
        }
        let name = &attributes[name_start..cursor];

        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'=') {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }

        let quote = *bytes.get(cursor)?;
        if !matches!(quote, b'\'' | b'"') {
            return None;
        }
        cursor += 1;
        let value_start = cursor;
        while cursor < bytes.len() && bytes[cursor] != quote {
            cursor += 1;
        }
        if cursor == bytes.len() {
            return None;
        }
        let value = &attributes[value_start..cursor];
        cursor += 1;
        if name == wanted {
            return Some(value);
        }
    }

    None
}

fn parse_download_href(href: &str) -> Option<BookorbitIds> {
    let path = href.strip_prefix(DOWNLOAD_PREFIX)?;
    let (book_id, query) = path.split_once("/download?fileId=")?;
    if book_id.is_empty() || !book_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }

    let file_id = match query.split_once('&') {
        Some((file_id, extra_params))
            if !extra_params.is_empty() && !extra_params.split('&').any(str::is_empty) =>
        {
            file_id
        }
        Some(_) => return None,
        None => query,
    };
    if file_id.is_empty() || !file_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }

    Some(BookorbitIds {
        book_id: book_id.parse().ok()?,
        file_id: file_id.parse().ok()?,
    })
}

fn xml_unescape(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;

    while let Some(offset) = text[cursor..].find('&') {
        let ampersand = cursor + offset;
        output.push_str(&text[cursor..ampersand]);
        let entity_start = ampersand + 1;
        let Some(end_offset) = text[entity_start..].find(';') else {
            output.push_str(&text[ampersand..]);
            return output;
        };
        let entity_end = entity_start + end_offset;
        let entity = &text[entity_start..entity_end];
        if let Some(character) = decode_entity(entity) {
            output.push(character);
        } else {
            output.push_str(&text[ampersand..=entity_end]);
        }
        cursor = entity_end + 1;
    }

    output.push_str(&text[cursor..]);
    output
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => entity
            .strip_prefix("#x")
            .or_else(|| entity.strip_prefix("#X"))
            .and_then(|digits| u32::from_str_radix(digits, 16).ok())
            .or_else(|| {
                entity
                    .strip_prefix('#')
                    .and_then(|digits| digits.parse().ok())
            })
            .and_then(char::from_u32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_SHAPE_FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <id>urn:bookorbit:catalog</id>
  <title>BookOrbit Catalog</title>
  <entry data-source="watch-folder">
    <title>The Daily EPUB — 2026-09-05 (X4)</title>
    <id>urn:bookorbit:book:410</id>
    <link rel="http://opds-spec.org/acquisition" href="/api/v1/opds/410/download?fileId=901" type="application/epub+zip" title="EPUB"/>
  </entry>
  <entry data-source="watch-folder">
    <title>The Daily EPUB — 2026-09-05</title>
    <id>urn:bookorbit:book:411</id>
    <link rel="http://opds-spec.org/acquisition" href="/api/v1/opds/411/download?fileId=902" type="application/epub+zip" title="EPUB"/>
  </entry>
</feed>"#;

    fn date() -> Date {
        "2026-09-05".parse().expect("date")
    }

    fn feed(entries: &str) -> String {
        format!(r#"<feed xmlns="http://www.w3.org/2005/Atom">{entries}</feed>"#)
    }

    #[test]
    fn exact_title_match_returns_the_right_ids() {
        let xml = feed(
            r#"<entry><title>Another book</title><link rel="http://opds-spec.org/acquisition" href="/api/v1/opds/1/download?fileId=2"/></entry>
               <entry><title>The Daily EPUB — 2026-09-05</title><link rel="http://opds-spec.org/acquisition" href="/api/v1/opds/31/download?fileId=47"/></entry>"#,
        );

        assert_eq!(
            select_issue_entry(&xml, "The Daily EPUB — 2026-09-05", date()).unwrap(),
            Some(BookorbitIds {
                book_id: 31,
                file_id: 47
            })
        );
    }

    #[test]
    fn x4_entry_listed_first_is_skipped_for_standard() {
        assert_eq!(
            select_issue_entry(REAL_SHAPE_FEED, "The Daily EPUB — 2026-09-05", date()).unwrap(),
            Some(BookorbitIds {
                book_id: 411,
                file_id: 902
            })
        );
    }

    #[test]
    fn hyphen_form_fallback_works_without_em_dash_title() {
        let xml = feed(
            r#"<entry><title>The Daily EPUB - 2026-09-05</title><link rel="http://opds-spec.org/acquisition" href="/api/v1/opds/52/download?fileId=81"/></entry>"#,
        );

        assert_eq!(
            select_issue_entry(&xml, "The Daily EPUB — 2026-09-05", date()).unwrap(),
            Some(BookorbitIds {
                book_id: 52,
                file_id: 81
            })
        );
    }

    #[test]
    fn exact_title_outranks_an_earlier_fallback() {
        let xml = feed(
            r#"<entry><title>The Daily EPUB - 2026-09-05</title><link rel="http://opds-spec.org/acquisition" href="/api/v1/opds/1/download?fileId=2"/></entry>
               <entry><title>The Daily EPUB — 2026-09-05</title><link rel="http://opds-spec.org/acquisition" href="/api/v1/opds/3/download?fileId=4"/></entry>"#,
        );

        assert_eq!(
            select_issue_entry(&xml, "The Daily EPUB — 2026-09-05", date()).unwrap(),
            Some(BookorbitIds {
                book_id: 3,
                file_id: 4
            })
        );
    }

    #[test]
    fn no_matching_entry_returns_none() {
        let xml = feed(
            r#"<entry><title>Unrelated</title><link rel="http://opds-spec.org/acquisition" href="/api/v1/opds/7/download?fileId=8"/></entry>"#,
        );

        assert_eq!(
            select_issue_entry(&xml, "The Daily EPUB — 2026-09-05", date()).unwrap(),
            None
        );
        assert_eq!(
            select_issue_entry(&feed(""), "The Daily EPUB — 2026-09-05", date()).unwrap(),
            None
        );
    }

    #[test]
    fn malformed_acquisition_href_is_an_error_for_a_match() {
        let xml = feed(
            r#"<entry><title>The Daily EPUB — 2026-09-05</title><link rel="http://opds-spec.org/acquisition" href="/api/v1/opds/nope/download?fileId=8"/></entry>"#,
        );

        assert!(matches!(
            select_issue_entry(&xml, "The Daily EPUB — 2026-09-05", date()),
            Err(BookorbitError::Malformed(_))
        ));
    }

    #[test]
    fn title_entities_are_unescaped_before_comparison() {
        let xml = feed(
            r#"<entry><title>Books &amp; News — 2026-09-05</title><link rel="http://opds-spec.org/acquisition" href="/api/v1/opds/12/download?fileId=13"/></entry>"#,
        );

        assert_eq!(
            select_issue_entry(&xml, "Books & News — 2026-09-05", date()).unwrap(),
            Some(BookorbitIds {
                book_id: 12,
                file_id: 13
            })
        );
    }

    #[test]
    fn attributed_entry_and_reordered_link_attributes_parse() {
        let xml = feed(
            r#"<entry data-index="1"><title type="text">The Daily EPUB — 2026-09-05</title><link title="EPUB" href="/api/v1/opds/63/download?fileId=64&amp;download=true" type="application/epub+zip" rel="http://opds-spec.org/acquisition"/></entry>"#,
        );

        assert_eq!(
            select_issue_entry(&xml, "The Daily EPUB — 2026-09-05", date()).unwrap(),
            Some(BookorbitIds {
                book_id: 63,
                file_id: 64
            })
        );
    }

    #[test]
    fn reader_url_formats_the_reader_route() {
        assert_eq!(
            reader_url(
                "https://bookorbit.example",
                BookorbitIds {
                    book_id: 14,
                    file_id: 29
                }
            ),
            "https://bookorbit.example/read/14/29"
        );
    }

    #[test]
    fn body_without_feed_is_malformed() {
        assert!(matches!(
            select_issue_entry(
                "<html><body>not an Atom feed</body></html>",
                "The Daily EPUB — 2026-09-05",
                date()
            ),
            Err(BookorbitError::Malformed(_))
        ));
    }
}
