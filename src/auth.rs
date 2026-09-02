//! Rating-link signing — the single source of truth for the HMAC token (spec §3.9).
//!
//! The EPUB article footer ([`crate::epub::build`]) mints the links and the rating
//! endpoint ([`crate::server`]) verifies them, so the formula must be identical on
//! both sides. It lives here and nowhere else:
//!
//! ```text
//! message = "{issue_date}/{article_id}/{loved|good|down}"
//! token   = hex(hmac_sha256(secret, message))[..16]
//! link    = {public_url}/r/{issue_date}/{article_id}/{vote}?t={token}
//! ```
//!
//! Pinned test vector, asserted from three places (here, `epub::build`,
//! `tests/m7_server.rs`): `secret = "test-secret"`, date `2026-08-15`,
//! article `42`, `loved` (with legacy `up` verification).

use hmac::{Hmac, KeyInit, Mac};
use jiff::civil::Date;
use sha2::Sha256;

use crate::types::{ArticleId, Vote};

/// Characters of the hex HMAC kept in rating links (§3.9).
pub const TOKEN_LEN: usize = 16;

/// The exact signed string: `{issue_date}/{article_id}/{loved|good|down}` (§3.9).
pub fn rating_message(issue_date: Date, article_id: ArticleId, vote: Vote) -> String {
    format!("{issue_date}/{article_id}/{}", vote.as_str())
}

/// `hex(hmac_sha256(secret, "{issue_date}/{article_id}/{vote}"))[..16]` (§3.9).
pub fn rating_token(secret: &str, issue_date: Date, article_id: ArticleId, vote: Vote) -> String {
    rating_token_for_segment(secret, issue_date, article_id, vote.as_str())
}

fn rating_token_for_segment(
    secret: &str,
    issue_date: Date,
    article_id: ArticleId,
    segment: &str,
) -> String {
    // `Hmac` derives a fixed-size key from any input length, so this never fails.
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(format!("{issue_date}/{article_id}/{segment}").as_bytes());
    let digest = hex::encode(mac.finalize().into_bytes());
    digest[..TOKEN_LEN].to_string()
}

/// Constant-time comparison of a supplied token against the expected one (§3.9).
pub fn verify_token(
    secret: &str,
    issue_date: Date,
    article_id: ArticleId,
    vote: Vote,
    token: &str,
) -> bool {
    let current = rating_token(secret, issue_date, article_id, vote);
    if constant_time_eq(current.as_bytes(), token.as_bytes()) {
        return true;
    }
    // Already-published `up` links were signed over the literal legacy segment.
    vote == Vote::Loved
        && constant_time_eq(
            rating_token_for_segment(secret, issue_date, article_id, "up").as_bytes(),
            token.as_bytes(),
        )
}

/// Length-independent, data-independent byte comparison.
///
/// A tiny local implementation so the crate does not need `subtle` directly;
/// `black_box` keeps the optimizer from short-circuiting the accumulate.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// Full rating URL embedded in an article footer:
/// `{public_url}/r/{date}/{article_id}/{loved|good|down}?t={token}` (§3.9).
pub fn rating_url(
    public_url: &str,
    secret: &str,
    issue_date: Date,
    article_id: ArticleId,
    vote: Vote,
) -> String {
    let token = rating_token(secret, issue_date, article_id, vote);
    format!(
        "{}/r/{issue_date}/{article_id}/{}?t={token}",
        public_url.trim_end_matches('/'),
        vote.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date() -> Date {
        "2026-08-15".parse().expect("date")
    }

    #[test]
    fn all_three_tokens_verify_and_are_distinct() {
        let votes = [Vote::Loved, Vote::Good, Vote::NotForMe];
        let tokens: Vec<String> = votes
            .iter()
            .map(|vote| rating_token("test-secret", date(), 42, *vote))
            .collect();
        assert_eq!(tokens.len(), 3);
        assert!(tokens.iter().all(|token| token.len() == TOKEN_LEN));
        assert_ne!(tokens[0], tokens[1]);
        assert_ne!(tokens[1], tokens[2]);
        for (vote, token) in votes.into_iter().zip(tokens) {
            assert!(verify_token("test-secret", date(), 42, vote, &token));
        }
    }

    #[test]
    fn legacy_up_token_still_verifies_as_loved() {
        let legacy = rating_token_for_segment("test-secret", date(), 42, "up");
        assert_eq!(legacy, "3b314cf7e6d8f50f");
        assert!(verify_token(
            "test-secret",
            date(),
            42,
            Vote::Loved,
            &legacy
        ));
    }

    #[test]
    fn verification_rejects_tampering() {
        let token = rating_token("s", date(), 42, Vote::Loved);
        assert!(!verify_token("s", date(), 42, Vote::Good, &token));
        assert!(!verify_token("s", date(), 43, Vote::Loved, &token));
        assert!(!verify_token("other", date(), 42, Vote::Loved, &token));
        assert!(!verify_token("s", date(), 42, Vote::Loved, "short"));
    }

    #[test]
    fn url_shape_matches_the_spec() {
        let token = rating_token("test-secret", date(), 42, Vote::Good);
        assert_eq!(
            rating_url(
                "https://daily.hallada.net/",
                "test-secret",
                date(),
                42,
                Vote::Good
            ),
            format!("https://daily.hallada.net/r/2026-08-15/42/good?t={token}")
        );
    }
}
