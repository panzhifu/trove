//! The content hash: BLAKE3 hex, the one string three subsystems agree on.
//!
//! A hash is not free-form text. [`crate::media::hash`] always produces 64
//! lowercase hex characters, and three separate things key off that exact
//! shape: the blob is filed under it, the thumbnail cache names files after it,
//! and the `assets.content_hash` column indexes it for deduplication. A string
//! of the wrong length or alphabet is not a "slightly off" hash — it is a key
//! to a file that does not exist, and the dedup lookup that uses it silently
//! misses.
//!
//! So the shape is carried by the type, the way [`crate::model::Rating`]
//! carries the star range: [`ContentHash::parse`] is the one validating door
//! (accepting either hex case and normalizing to lowercase), and
//! [`ContentHash::from_hasher`] is the trusted one for bytes this build just
//! hashed itself. The field that holds it is `Option<ContentHash>`, so an
//! asset with no file yet (a metadata placeholder) says so with `None` rather
//! than an empty string that looks like a hash.
//!
//! [`Deref<Target = str>`](std::ops::Deref) is deliberate: every reader — the
//! blob path, the thumbnail cache, the SQL binding — wants the `&str` inside,
//! and a newtype that made each of them call `.as_str()` would cost more than
//! it taught. Writers still go through `parse`/`from_hasher`, which is where
//! the invariant is enforced.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Hex characters in a content hash: 64, for a 32-byte BLAKE3 digest.
///
/// The one definition for the whole crate — [`crate::media::hash`] re-exports
/// it rather than spelling 64 a second time, because the blob layout, the
/// thumbnail file names and the storage column all read a hash as opaquely
/// this many hex characters.
pub const HEX_LEN: usize = 64;

/// A BLAKE3 content digest: exactly [`HEX_LEN`] lowercase hex characters.
///
/// Compares and serializes as the string it wraps: the export format writes a
/// bare string, and `serde(transparent)` keeps an existing library's JSON
/// unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(String);

impl ContentHash {
    /// The hash `text` names, normalized to lowercase, or `None` when it is not
    /// [`HEX_LEN`] hex characters.
    ///
    /// Either case is accepted on the way in — a hash written by another tool
    /// in uppercase names the same bytes — and the stored spelling is always
    /// lowercase, so two spellings of one digest compare equal.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != HEX_LEN || !bytes.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        Some(Self(text.to_ascii_lowercase()))
    }

    /// The hash of bytes this build hashed itself, which needs no re-checking.
    ///
    /// [`crate::media::hash`] emits 64 lowercase hex characters by
    /// construction, so the boundary between it and the domain is not a place
    /// to re-parse; `debug_assert` catches a future hasher change in tests
    /// without putting a branch on the import path.
    pub fn from_hasher(text: String) -> Self {
        debug_assert!(
            Self::parse(&text).is_some(),
            "the hasher produced a non-canonical digest: {text:?}"
        );
        Self(text)
    }

    /// The hex digest, lowercase.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for ContentHash {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ContentHash {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<ContentHash> for String {
    fn from(hash: ContentHash) -> String {
        hash.0
    }
}

#[cfg(test)]
mod tests {
    use super::{ContentHash, HEX_LEN};

    #[test]
    fn only_64_hex_characters_are_a_hash() {
        assert!(ContentHash::parse(&"a".repeat(HEX_LEN)).is_some());
        assert!(ContentHash::parse(&"0123456789abcdef".repeat(4)).is_some());
        // Wrong length.
        assert!(ContentHash::parse(&"a".repeat(HEX_LEN - 1)).is_none());
        assert!(ContentHash::parse(&"a".repeat(HEX_LEN + 1)).is_none());
        assert!(ContentHash::parse("").is_none());
        // Right length, wrong alphabet.
        assert!(ContentHash::parse(&"z".repeat(HEX_LEN)).is_none());
        assert!(ContentHash::parse(&"g".repeat(HEX_LEN)).is_none());
    }

    #[test]
    fn an_uppercase_hash_is_the_same_hash() {
        let upper = "A".repeat(HEX_LEN);
        let lower = "a".repeat(HEX_LEN);
        assert_eq!(
            ContentHash::parse(&upper),
            ContentHash::parse(&lower),
            "case must not name two different digests"
        );
        assert_eq!(ContentHash::parse(&upper).unwrap().as_str(), lower);
    }

    #[test]
    fn a_hash_is_the_string_it_wraps() {
        let hash = ContentHash::parse(&"b".repeat(HEX_LEN)).unwrap();
        // The readers that want `&str` get it without a conversion call.
        let as_str: &str = &hash;
        assert_eq!(as_str.len(), HEX_LEN);
        assert_eq!(hash.to_string(), as_str);
        assert_eq!(
            serde_json::to_string(&hash).unwrap(),
            serde_json::to_string(as_str).unwrap()
        );
    }
}
