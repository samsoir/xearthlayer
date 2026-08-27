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
//! 3. A rejected entry is deleted, not left in place. Each cache satisfies
//!    this its own way: the disk tiers call [`io::discard`] explicitly on a
//!    validator rejection; the index caches (which have no separate
//!    validation step to reject from) satisfy it by unconditionally
//!    overwriting the same path via [`io::write_atomic`] the next time they
//!    rebuild.
//! 4. A write is durable or it never happened ([`io::write_atomic`]).

mod io;
mod validators;

pub use io::{discard, length_ceiling, write_atomic};
pub use validators::{dds_tile_validator, raw_chunk_validator, MagicAndSize, EXPECTED_DDS_SIZE};

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
}

/// Validates the raw bytes of one cache entry before they are trusted.
pub trait CacheEntryValidator: Send + Sync {
    /// Stable name for log fields.
    fn name(&self) -> &'static str;
    fn validate(&self, bytes: &[u8]) -> Result<(), IntegrityError>;
}
