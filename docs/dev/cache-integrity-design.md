# Cache Integrity Design

**Status**: Implemented
**Created**: 2026-08-26
**Issue**: #253

## Overview

XEarthLayer has several caches backed by files on disk: the ortho union index
cache, the scenery index cache, and the DDS-tile / raw-chunk disk cache tiers.
Every one of them can be corrupted by something outside the program's
control — a killed process mid-write, a full disk, a bad sector, manual
tampering. Before this module existed, each cache handled that possibility
differently, and none of them handled it safely:

- An oversized or misaligned length prefix in a bincode-encoded cache passed
  a garbage value to `Vec::with_capacity`, which **aborts the process**
  (`SIGABRT`) rather than returning an `Err` — no caller could recover from
  this, however carefully it matched on `Result`.
- A corrupt DDS tile on disk was read, failed to decode, and was re-served or
  left in place, so the same file was rejected — or worse, shown to X-Plane
  as a permanently magenta tile — on every subsequent request.
- Writes had no consistent durability guarantee: some went through a
  temp-file-plus-rename, some did not fsync before renaming, so a crash could
  leave a torn write promoted to the live path.

`cache/integrity` gives every one of these caches one shared model instead of
each reinventing (or omitting) its own. The starting premise: **a cache is
never a source of truth**. It exists purely to avoid recomputation; every
value it returns must be either trustworthy or absent, and losing an entry to
corruption must never cost more than a slower rebuild from the real sources
(HTTP downloads, `.ter` file scans, directory walks).

## The Four Invariants

1. **A read has two outcomes: a trusted value, or a miss.** Malformed input
   is a miss — never an error, never an abort, never degraded output served
   in place of regenerating.
2. **Every length taken from file content is bounded by evidence from that
   file.** A file's own size is the natural ceiling: nothing legitimate can
   require reading, or allocating for, more bytes than the file holds. See
   [`length_ceiling`](#io-length_ceiling-write_atomic-discard).
3. **A rejected entry is deleted, not left in place.** Otherwise the next
   read rejects it again, forever — the corruption becomes permanent instead
   of self-healing.
4. **A write is durable or it never happened.** Temp file → flush → `fsync`
   → rename, with the temp file removed on any failure path. See
   [`write_atomic`](#io-length_ceiling-write_atomic-discard).

Each cache satisfies invariant 3 its own way — there is no single shared type
that forces it. The disk tiers (`cache/providers/disk.rs`) call
[`discard`](#io-length_ceiling-write_atomic-discard) explicitly the moment
[`CacheEntryValidator::validate`](#cacheentryvalidator) rejects an entry. The
index caches (`ortho_union/cache.rs`, `prefetch/scenery_cache.rs`) have no
separate validation step to reject from — a bad file there is retired by
being unconditionally overwritten via `write_atomic` the next time the
in-memory index rebuilds and saves. Both routes delete-or-overwrite the bad
file; neither leaves it in place to be rejected again on the next read.

## Module API

`xearthlayer/src/cache/integrity/` has three files:

```
cache/integrity/
├── mod.rs        # IntegrityError, CacheEntryValidator trait
├── io.rs         # length_ceiling, write_atomic, discard
└── validators.rs # MagicAndSize, dds_tile_validator, raw_chunk_validator
```

It imports nothing from `fuse`, `metrics`, or `service` — it is a leaf module
that every cache depends on, not one that depends back on any of them.

### `IntegrityError`

```rust
pub enum IntegrityError {
    Empty,
    BadMagic { expected: &'static [u8] },
    WrongSize { actual: usize, expected: usize },
    ImplausibleLength { claimed: u64, ceiling: u64 },
}
```

Carried for logs; callers branch on the variant, never on the rendered text.
`ImplausibleLength` is constructed at both `Vec::with_capacity` bound sites in
`prefetch/scenery_cache.rs` (`total_tiles` and `package_count`) — the two
places a garbage count from file content would otherwise reach an allocator.

### `CacheEntryValidator`

```rust
pub trait CacheEntryValidator: Send + Sync {
    fn name(&self) -> &'static str;
    fn validate(&self, bytes: &[u8]) -> Result<(), IntegrityError>;
}
```

`name()` identifies which validator rejected an entry; `disk.rs::get()` passes
it through to `discard()` so the resulting `warn` log line says which check
failed, not just what the failure was.

### Validators (`validators.rs`)

`MagicAndSize` is a `CacheEntryValidator` expressed as magic bytes plus an
optional exact length, checked in this order: empty first (always reported
as `Empty`, never `BadMagic`), then magic, then length.

```rust
pub fn dds_tile_validator() -> MagicAndSize   // b"DDS ", exact EXPECTED_DDS_SIZE
pub fn raw_chunk_validator() -> MagicAndSize  // empty magic slice, no length check
```

An empty `magic` slice makes `BadMagic` unreachable — every byte slice
trivially starts with an empty prefix — which is how `raw_chunk_validator`
asserts non-emptiness only, without claiming a magic sequence that hasn't
been verified (see [non-decisions](#deliberate-non-decisions) below).

`EXPECTED_DDS_SIZE` is computed at compile time by walking the same mipmap
chain the encoder does (halving dimensions, sizing each level as
`div_ceil(4) * div_ceil(4) * 8` bytes, plus a 128-byte header) — 11,184,952
bytes for a 4096×4096 BC1 tile with a full 13-level chain.

### `io`: `length_ceiling`, `write_atomic`, `discard`

```rust
pub fn length_ceiling(file: &File) -> io::Result<u64>
pub fn write_atomic<F>(path: &Path, write: F) -> io::Result<()>
    where F: FnOnce(&mut BufWriter<File>) -> io::Result<()>
pub fn discard(path: &Path, reason: &IntegrityError, validator: &str)
```

- `length_ceiling` is invariant 2 in one call: the file's own size, nothing
  more.
- `write_atomic` creates the destination's parent directory (`create_dir_all`)
  if it doesn't already exist, then writes to a sibling `.tmp` path, flushes,
  calls `sync_all()`, and renames over the live path; the temp file is
  removed on any failure in that sequence. Owning the directory creation here
  is load-bearing, not a convenience: the only thing that reliably creates
  `~/.xearthlayer` is `logging.rs`, keyed off the user-settable `logging.file`
  config value, so a caller that assumed the directory already existed could
  fail `ENOENT` on a fresh install with a non-default log path. It does
  **not** `fsync` the parent directory after the rename — that's defensible
  because the worst crash outcome without it is that the rename itself is
  lost and the old file survives untouched at the live path, which is
  invariant 4's "it never happened" branch, not a torn write.
- `discard` logs a `warn` with the path, `IntegrityError`, and the name of
  the validator that rejected the entry, then removes the file; a failure to
  delete (other than `NotFound`) is itself logged, not propagated —
  deletion is a best-effort cleanup on a path that has already decided to
  treat the entry as gone.

## How Each Cache Adopts the Model

The four caches don't all adopt the same amount of the model — the amount
adopted matches what each one actually needed.

### `ortho_union/cache.rs` (`IndexCache`)

`IndexCache::load` bounds the bincode reader with `length_ceiling` before
deserializing:

```rust
let limit = crate::cache::integrity::length_ceiling(&file)?;
bincode::DefaultOptions::new()
    .with_fixint_encoding()
    .allow_trailing_bytes()
    .with_limit(limit)
    .deserialize_from(reader)
```

The three `DefaultOptions` calls reproduce the legacy `bincode::deserialize_from`
exactly — `bincode::options()` alone defaults to varint encoding and would
fail to read every cache written before this change. The limit is what turns
an oversized or misaligned length prefix into an `Err` instead of a
`SIGABRT`.

`IndexCache::save` delegates entirely to `write_atomic`. `try_load_cached_index`
logs a `warn` on any load failure ("Discarding unreadable index cache;
rebuilding from sources") and returns `None` — it does not call `discard()`
to delete the bad file directly. This still satisfies invariant 3 in
practice: the caller (`ortho_union/builder.rs`) always rebuilds the index
from sources on a cache miss and then unconditionally calls
`save_index_cache`, which overwrites the same path via `write_atomic`. The
bad file is retired by being overwritten on the very next successful build,
not by an explicit delete.

This cache does not use `CacheEntryValidator` — the payload is a single
bincode-typed struct, and bincode's own deserialization failure already
distinguishes malformed from well-formed. There is no separate "plausible
bytes but wrong shape" case for a validator to add value against.

### `prefetch/scenery_cache.rs` (`load_cache` / `save_cache`)

The line-based text cache reads a `total_tiles` count and a `package_count`
from its header before allocating `Vec::with_capacity` for each. Both are
now bounded, each turned into an `IntegrityError::ImplausibleLength` and an
`Invalid`/`Err` verdict when they exceed the ceiling:

```rust
let max_tiles = ceiling / MIN_TILE_LINE_BYTES;
if total_tiles as u64 > max_tiles {
    let reason = IntegrityError::ImplausibleLength { claimed: total_tiles as u64, ceiling: max_tiles };
    return CacheLoadResult::Invalid { error: format!("...{:?}", reason) };
}
```

— `total_tiles` is divided by `MIN_TILE_LINE_BYTES` (not compared to the raw
byte ceiling directly) because it sizes a `Vec<SceneryTile>`, not a byte
buffer: an element-for-element `SceneryTile` costs far more than one file
byte, so a raw-byte ceiling still let a corrupt count over-reserve by roughly
`size_of::<SceneryTile>()` per byte of file. `package_count` guards the same
allocation in `cache_status()` — a second, independent read path added after
the original tile-count fix, since a CLI status query never goes through
`load_cache`'s package-count-vs-current-packages cross-check.

`load_cache` is now a thin wrapper over `load_cache_from(path, packages)`,
and `cache_status` over `cache_status_from(path)`, both split out so tests
can point at a temporary file instead of the live
`~/.xearthlayer/scenery_index.cache` (potentially ~180 MB) without ever
reading or mutating it. `save_cache` delegates its write entirely to
`write_atomic`, which creates the parent directory itself.

This cache keeps its own four-variant `CacheLoadResult` (`Loaded` / `Stale` /
`NotFound` / `Invalid`) rather than adopting `CacheEntryValidator` — see
[non-decisions](#deliberate-non-decisions) below for why `Stale` and
`Invalid` are kept distinct. As with the ortho union index, the caller
(`service/orchestrator/core.rs`) rebuilds from sources on `Stale`,
`NotFound`, *or* `Invalid` and then calls `save_cache` **only when
`total_tiles > 0`** — so an invalid cache whose rebuild happens to find zero
tiles is never overwritten (or deleted), unlike the ortho union index, which
saves unconditionally. See [Known Limitations](#known-limitations).

### `cache/providers/disk.rs` (`DiskCacheProvider`)

This is the one caller that uses `CacheEntryValidator` for the reject path.
`get()` calls `validate()` directly and inlines the collapse to `None` itself
— there is no separate wrapper type — since it also needs to touch the LRU
index and the size metric on the way through. `validator_for_tier` is the
single point mapping a `DiskTier` to a validator:

```rust
fn validator_for_tier(tier: DiskTier) -> Arc<dyn CacheEntryValidator> {
    match tier {
        DiskTier::Dds => Arc::new(dds_tile_validator()),
        DiskTier::Chunk => Arc::new(raw_chunk_validator()),
    }
}
```

`DiskCacheProvider` stores the chosen validator at construction, so the
struct itself stays ignorant of what it holds — the same type backs both the
DDS tile tier and the raw chunk tier. In `get()`:

```rust
if let Err(reason) = self.validator.validate(&data) {
    discard(&path, &reason, self.validator.name());
    self.lru_index.remove(&key_owned);
    self.report_size_to_metrics();
    return Ok(None);
}
```

a rejected entry is deleted from disk *and* removed from the in-memory LRU
index in the same branch — the index would otherwise keep pointing at a file
that no longer exists. It also reports the (now smaller) cache size to
metrics immediately, the same as the `set()` and `delete()` paths do; without
that call the size gauge only self-corrects on the *next* `set()` for that
key, which never arrives for a tile that regenerates incomplete (#180), so it
could otherwise hold stale-high indefinitely. `discard()` is passed
`self.validator.name()` so its `warn` log records which validator rejected
the entry, not just why. A non-`NotFound` I/O error on the read (e.g. a
transient disk error) now also degrades to a logged miss rather than
propagating as `Err` to the caller: the tile regenerates from the next tier
up instead of failing the request outright.

`set()` on this provider performs its own async atomic write — a
`tokio::fs::write` to a sibling `.tmp` path followed by `tokio::fs::rename`
— rather than calling `io::write_atomic`, which takes a synchronous
`std::fs::File` and would block the async runtime if called directly. This
write is atomic (a reader never observes a torn file at the live path) but
does **not** call `sync_all()` before the rename, so invariant 4's
durability half is not currently enforced for this tier. This is a
pre-existing asymmetry with the other three caches, not something this
change introduced or closed; it is recorded here rather than glossed over.

## Deliberate Non-Decisions

Two choices in this model were made on purpose and are recorded here so they
aren't mistaken for oversights in a later pass.

### Chunk validation is non-empty-only

`raw_chunk_validator()` checks only that a chunk has bytes at all. Asserting
a JPEG magic (`0xFF 0xD8`) would require proving that all seven current
imagery providers — Bing, Go2, Google, Apple, ArcGIS, Mapbox, USGS — return
JPEG for every tile they serve. That has not been audited. Claiming an
unverified magic and rejecting on mismatch would turn a correctness fix into
an outage: any provider returning a different (but valid) format would have
every one of its chunks discarded and regenerated in a loop. Tighten this
validator only after a provider audit confirms the actual format contract.

### `CacheLoadResult::Stale` stays distinct from `Invalid`

In the scenery index cache, a `Stale` result (packages changed, version
mismatch) is **correct data that no longer matches its current sources** —
not corruption. `Invalid` is reserved for data that fails to parse or fails a
plausibility check (like the `total_tiles` ceiling above). Collapsing both
into one "discard and rebuild" outcome would lose a useful operational
diagnostic: `Stale` tells an operator the cache did its job and simply aged
out; `Invalid` tells them something wrote a broken file. Both currently
result in the same rebuild-and-overwrite behaviour at the call site, but the
distinction is kept in the type so that behaviour isn't forced to stay
identical forever, and so logs say which one actually happened.

## Known Limitations

- **`EXPECTED_DDS_SIZE` is BC1-only.** It's computed from
  `full_chain_bc1_dds_size(4096, 4096)`, but `texture.format` also accepts
  `bc3`, which has a different (larger) per-block size. Whether a BC3-sized
  tile can currently reach the DDS disk cache tier in practice has **not**
  been verified — see the `TODO(#253)` at the constant's definition. Until
  that's checked, a BC3 deployment could see every DDS disk entry rejected
  by `WrongSize`, degrading (safely, but silently) to full re-encoding on
  every read.
- **A scenery cache rebuild that finds zero tiles is neither overwritten nor
  deleted.** `service/orchestrator/core.rs` only calls `save_cache` when
  `total_tiles > 0`, so a corrupt-on-disk scenery cache whose rebuild
  legitimately yields zero tiles (e.g. no packages currently indexed) is left
  exactly as it was — invariant 3 is not met on this one path. The failed
  load re-warns on every subsequent startup until either a rebuild finds at
  least one tile or the file is removed by hand. This is a pre-existing gap,
  not something this change introduced; fixing it is out of scope here (it
  would change `service/orchestrator/core.rs` behaviour, which this pass
  deliberately does not touch).
- **fsync ordering is not unit-testable** without a fault-injecting
  filesystem — there's no way to assert from a test that the `fsync` happens
  before the `rename` short of intercepting syscalls. It is addressed by
  construction (the code only has one order it can execute in) and verified
  by inspection, not by a regression test. Removing the `sync_all()` call
  entirely still leaves the existing test suite green, which is the honest
  bound on what that suite proves.
- **`cache/providers/disk.rs::set()` doesn't fsync before rename** (see
  above) — atomic, but not durable in the crash sense that `write_atomic`
  provides for the other three caches.

## Key Files

| File | Purpose |
|------|---------|
| `xearthlayer/src/cache/integrity/mod.rs` | `IntegrityError`, `CacheEntryValidator` |
| `xearthlayer/src/cache/integrity/io.rs` | `length_ceiling`, `write_atomic`, `discard` |
| `xearthlayer/src/cache/integrity/validators.rs` | `MagicAndSize`, `dds_tile_validator`, `raw_chunk_validator`, `EXPECTED_DDS_SIZE` |
| `xearthlayer/src/ortho_union/cache.rs` | `IndexCache` — bounded bincode load, `write_atomic` save |
| `xearthlayer/src/prefetch/scenery_cache.rs` | `load_cache` / `save_cache` — bounded `total_tiles`, `write_atomic` save |
| `xearthlayer/src/cache/providers/disk.rs` | `DiskCacheProvider` — `validator_for_tier`, discard-on-reject `get()` |
| `docs/dev/index-building-optimization.md` | Ortho union index cache design (corruption handling section links back here) |
