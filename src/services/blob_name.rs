//! The blob-name grammar: `<hash>[_<expiration>][.<extension>]` plus the
//! two-level `<h0>/<h1>` fan-out directory prefix.
//!
//! This module is the single owner of that grammar. It is pure: it touches no
//! [`crate::models::AppState`] and performs no I/O. The native (`file_storage`)
//! and S3 (`native_storage`) backends previously each encoded the grammar and
//! the fan-out prefix in their own duplicated, loosely-agreeing match arms and
//! prefix builders; they now delegate here so the grammar lives exactly once.
//!
//! ## Hex-case policy
//!
//! A blob hash must be exactly 64 **lowercase** hexadecimal characters. That is
//! the policy enforced by [`is_valid_hash`] (and therefore by [`name`],
//! [`fan_out`], and [`parse`]) and reused by
//! [`crate::services::file_storage::validate_sha256_format`]. Lowercase is
//! chosen because blob responses and index keys are content-addressed by the
//! lowercase digest; accepting uppercase would let two spellings of the same
//! blob diverge across the storage backends.

use crate::error::{AppError, AppResult};

/// Length of a SHA-256 hash in hexadecimal characters.
const HASH_LEN: usize = 64;

/// The three parts of a blob name, read back by [`parse`].
///
/// This is the inverse of [`name`]: anything `name` produces, `parse` reads
/// back identically, so a round-trip is total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedName {
    /// Lowercase SHA-256 hash.
    pub hash: String,
    /// Expiration timestamp (Unix seconds), if the name carried one.
    pub expiration: Option<u64>,
    /// Extension (everything after the first `.` following the hash), if any.
    pub extension: Option<String>,
}

/// Returns `true` when `hash` matches the single blob-hash policy: exactly 64
/// lowercase hexadecimal characters.
pub fn is_valid_hash(hash: &str) -> bool {
    hash.len() == HASH_LEN
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Fail with `BadRequest` unless `hash` satisfies the hash policy.
fn require_hash(hash: &str) -> AppResult<()> {
    if !is_valid_hash(hash) {
        return Err(AppError::BadRequest("Invalid SHA-256 hash".to_owned()));
    }
    Ok(())
}

/// Build the `<hash>[_<expiration>][.<extension>]` blob filename.
///
/// The `extension`, when present, is appended verbatim after a `.`; it may
/// itself contain dots (e.g. `tar.gz`). Returns an error only for a hash that
/// violates the policy.
pub fn name(hash: &str, expiration: Option<u64>, extension: Option<&str>) -> AppResult<String> {
    require_hash(hash)?;
    Ok(match (expiration, extension) {
        (Some(expiration), Some(extension)) => format!("{hash}_{expiration}.{extension}"),
        (Some(expiration), None) => format!("{hash}_{expiration}"),
        (None, Some(extension)) => format!("{hash}.{extension}"),
        (None, None) => hash.to_owned(),
    })
}

/// Parse a blob name back into the three parts of [`ParsedName`].
///
/// The grammar is decoded from the fixed-width, 64-hex hash prefix, so an
/// expiration (`_<digits>`) and an extension (everything after the first `.`)
/// are located unambiguously:
///
/// - `hash` → `(hash, None, None)`
/// - `hash_42` → `(hash, Some(42), None)`
/// - `hash.tar.gz` → `(hash, None, Some("tar.gz"))`
/// - `hash_42.tar.gz` → `(hash, Some(42), Some("tar.gz"))`
///
/// Returns `None` for anything that violates the grammar: a hash that is
/// missing, wrong-length, non-hex, or uppercase; an empty or non-numeric
/// expiration; or trailing characters that are neither `_` nor `.`. A trailing
/// `.` with nothing after it is read back as `Some("")`, the exact inverse of
/// [`name`] given `Some("")`.
pub fn parse(name: &str) -> Option<ParsedName> {
    let hash = name.get(..HASH_LEN)?;
    if !is_valid_hash(hash) {
        return None;
    }
    let rest = &name[HASH_LEN..];

    let (expiration, extension) = if rest.is_empty() {
        (None, None)
    } else if let Some(after_underscore) = rest.strip_prefix('_') {
        // `<expiration>` is the digit run up to the following dot or end.
        let (digits, extension) = after_underscore
            .split_once('.')
            .map_or((after_underscore, None), |(digits, ext)| {
                (digits, Some(ext))
            });
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        (Some(digits.parse().ok()?), extension)
    } else if let Some(extension) = rest.strip_prefix('.') {
        (None, Some(extension))
    } else {
        return None;
    };

    Some(ParsedName {
        hash: hash.to_owned(),
        expiration,
        extension: extension.map(str::to_owned),
    })
}

/// The two-level `<h0>/<h1>` fan-out directory prefix implied by a hash's first
/// two hex characters.
///
/// File storage joins these under a storage root (`uploads/` or
/// `upstream_cache/`); S3 joins them with `/` to form the object-key prefix.
pub fn fan_out(hash: &str) -> AppResult<(&str, &str)> {
    require_hash(hash)?;
    Ok((&hash[..1], &hash[1..2]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn hash() -> String {
        "aabb".repeat(16)
    }

    #[test]
    fn round_trips_all_four_shapes() {
        let cases: [(Option<u64>, Option<&str>); 4] = [
            (None, None),
            (Some(42), None),
            (None, Some("jpg")),
            (Some(42), Some("jpg")),
        ];
        for (expiration, extension) in cases {
            let name = name(&hash(), expiration, extension).unwrap();
            let parsed = parse(&name).expect("constructed name must parse");
            assert_eq!(parsed.hash, hash());
            assert_eq!(parsed.expiration, expiration);
            assert_eq!(
                parsed.extension,
                extension.map(str::to_owned),
                "for {name:?}"
            );
        }
    }

    #[test]
    fn fan_out_prefix_for_known_hash() {
        let h = hash();
        let (h0, h1) = fan_out(&h).unwrap();
        assert_eq!(h0, "a");
        assert_eq!(h1, "a");
        // A hash with distinct leading chars fans out into two single-char dirs.
        let other = format!("1234{}", "a".repeat(60));
        let (h0, h1) = fan_out(&other).unwrap();
        assert_eq!((h0, h1), ("1", "2"));
    }

    #[test]
    fn rejects_non_hex_hash() {
        let bad = format!("zz{}", "a".repeat(62));
        assert!(!is_valid_hash(&bad));
        assert!(name(&bad, None, None).is_err());
        assert!(fan_out(&bad).is_err());
        assert!(parse(&bad).is_none());
    }

    #[test]
    fn rejects_wrong_length_hash() {
        assert!(!is_valid_hash("abcd"));
        assert!(!is_valid_hash(&"a".repeat(63)));
        assert!(!is_valid_hash(&"a".repeat(65)));
        assert!(name("abcd", None, None).is_err());
        assert!(parse("abcd").is_none());
    }

    #[test]
    fn rejects_uppercase_hash() {
        let upper = "AABB".repeat(16);
        assert!(!is_valid_hash(&upper));
        assert!(name(&upper, None, None).is_err());
        assert!(fan_out(&upper).is_err());
        assert!(parse(&upper).is_none());
        // The lowercase spelling is accepted.
        assert!(is_valid_hash(&upper.to_lowercase()));
    }

    #[test]
    fn extension_may_contain_dots_and_may_be_absent() {
        let dotted = name(&hash(), None, Some("tar.gz")).unwrap();
        assert_eq!(dotted, format!("{}.tar.gz", hash()));
        let parsed = parse(&dotted).unwrap();
        assert_eq!(parsed.extension.as_deref(), Some("tar.gz"));
        assert_eq!(parsed.expiration, None);

        let with_exp = name(&hash(), Some(7), Some("wasm.gz")).unwrap();
        let parsed = parse(&with_exp).unwrap();
        assert_eq!(parsed.extension.as_deref(), Some("wasm.gz"));
        assert_eq!(parsed.expiration, Some(7));

        // Extension absent entirely.
        let bare = name(&hash(), None, None).unwrap();
        assert_eq!(parse(&bare).unwrap().extension, None);
    }

    #[test]
    fn rejects_invalid_expiration_and_trailing_garbage() {
        let h = hash();
        // Empty / non-numeric expiration is not part of the grammar.
        assert!(parse(&format!("{h}_")).is_none());
        assert!(parse(&format!("{h}_xyz")).is_none());
        assert!(parse(&format!("{h}_1_2")).is_none()); // digit run split by '_'
                                                       // Trailing character that is neither '.' nor '_'.
        assert!(parse(&format!("{h}x")).is_none());
    }

    #[test]
    fn pins_emitted_key_and_path_layout() {
        let h = hash();
        // S3 key layout: <h0>/<h1>/<name> (kept byte-for-byte).
        let (h0, h1) = fan_out(&h).unwrap();
        let filename = name(&h, Some(42), Some("jpg")).unwrap();
        let key = format!("{h0}/{h1}/{filename}");
        assert_eq!(key, format!("a/a/{h}_42.jpg"));

        // Filesystem path layout: <root>/<h0>/<h1>/<name>.
        let path = PathBuf::from("/data/uploads")
            .join(h0)
            .join(h1)
            .join(&filename);
        assert_eq!(path, PathBuf::from(format!("/data/uploads/a/a/{h}_42.jpg")));

        // Bare hash and hash+expiration layouts stay aligned with the grammar.
        assert_eq!(name(&h, None, None).unwrap(), h);
        assert_eq!(name(&h, Some(42), None).unwrap(), format!("{h}_42"));
    }
}
