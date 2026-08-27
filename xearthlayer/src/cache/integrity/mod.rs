//! Canonical cache integrity model.
//!
//! A cache is never a source of truth. Every read has two outcomes: a value we
//! trust, or a miss. Malformed input is a miss — never an error, never an
//! abort, never degraded output served in place of regeneration.
//!
//! This module gives every cache in the tree one shared model:
//!
//! 1. A read has two outcomes: a trusted value, or a miss.
//! 2. Every length taken from file content is bounded by evidence from that
//!    file ([`io::length_ceiling`]).
//! 3. A rejected entry is deleted, not left in place ([`CacheLoad::or_discard`]).
//! 4. A write is durable or it never happened ([`io::write_atomic`]).

mod io;
mod validators;

pub use io::{discard, length_ceiling, write_atomic};
pub use validators::{dds_tile_validator, raw_chunk_validator, MagicAndSize, EXPECTED_DDS_SIZE};

use std::path::Path;

/// Why a cache entry was rejected. Carried for logs; callers branch on the
/// variant, never on the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrityError {
    /// The entry held no bytes.
    Empty,
    /// The entry did not begin with the format's magic bytes.
    BadMagic { expected: &'static [u8] },
    /// The entry's length disagrees with what the format requires.
    WrongSize { actual: usize, expected: usize },
    /// The entry claimed more bytes than the file could possibly hold.
    ImplausibleLength { claimed: u64, ceiling: u64 },
    /// The entry could not be parsed.
    Malformed(String),
}

/// The outcome of loading a cache entry.
///
/// There is deliberately no error variant. A cache read cannot fail; it can
/// only miss. Anything a caller would have handled as an error is a `Rejected`
/// that resolves to a miss once the bad entry is discarded.
#[derive(Debug)]
pub enum CacheLoad<T> {
    Hit(T),
    Miss,
    Rejected(IntegrityError),
}

impl<T> CacheLoad<T> {
    /// Collapse to an `Option`, deleting a rejected entry from disk first.
    ///
    /// This is the only sanctioned way to turn a `CacheLoad` into a value:
    /// it guarantees invariant 3 cannot be forgotten at a call site.
    pub fn or_discard(self, path: &Path) -> Option<T> {
        match self {
            CacheLoad::Hit(value) => Some(value),
            CacheLoad::Miss => None,
            CacheLoad::Rejected(reason) => {
                discard(path, &reason);
                None
            }
        }
    }
}

/// Validates the raw bytes of one cache entry before they are trusted.
pub trait CacheEntryValidator: Send + Sync {
    /// Stable name for log fields.
    fn name(&self) -> &'static str;
    fn validate(&self, bytes: &[u8]) -> Result<(), IntegrityError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn or_discard_deletes_a_rejected_entry() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("entry.cache");
        std::fs::write(&path, b"garbage").unwrap();

        let load: CacheLoad<()> = CacheLoad::Rejected(IntegrityError::Empty);
        assert!(load.or_discard(&path).is_none());
        assert!(!path.exists(), "a rejected entry must be removed from disk");
    }

    #[test]
    fn or_discard_leaves_a_hit_in_place() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("entry.cache");
        std::fs::write(&path, b"good").unwrap();

        assert_eq!(CacheLoad::Hit(7).or_discard(&path), Some(7));
        assert!(path.exists());
    }

    #[test]
    fn or_discard_on_a_miss_touches_nothing() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("absent.cache");

        let load: CacheLoad<()> = CacheLoad::Miss;
        assert!(load.or_discard(&path).is_none());
        assert!(!path.exists());
    }
}
