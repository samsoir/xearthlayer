# Publisher Separation Design

## Status

**Draft**. Target release: 0.5.0.

Related: [Service-UI Decoupling Design](service-ui-decoupling-design.md) (the
crate topology this extends), [Package Publisher Design](package-publisher-design.md)
(what the publisher does), [Scenery Packages](scenery-packages.md) (the format
this design promotes to a contract crate).

### Work Items

Issues to be filed. The order is a dependency order, not a preference.

| Item | Work |
|------|------|
| P1 | `xearthlayer-package` contract crate |
| P2 | `xearthlayer-publisher` library crate |
| P3 | `xearthlayer-publish` binary crate, `xearthlayer publish` removed |
| P4 | Packaging and CI for the new binary |

## Problem Statement

Publishing scenery packages and streaming satellite imagery are separate
domains that currently share one library crate and one binary.

They are related only by the package format. A change to how tiles are
deduplicated before release has nothing to say about FUSE, and a change to
prefetch backpressure has nothing to say about archive parts. Yet today they
version together, build together, and ship together, so each domain pays for
the other's dependencies and each release couples the two.

The coupling is measurable and it is almost entirely one way.

**The publishing domain is large.** 9,861 lines of library across 23 files
(8.3% of the 118,903 line library), plus 4,189 lines of CLI across 7 files,
which is **25% of the entire CLI crate**.

**The runtime does not use it.** Production code contains no reference to
`publisher`. The only two references in the whole of `xearthlayer` outside the
module itself are in a `#[cfg(test)]` block:

```
xearthlayer/src/fuse/fuse3/ortho_union_fs.rs:2149   crate::publisher::DsfCompressor::laminar()
xearthlayer/src/fuse/fuse3/ortho_union_fs.rs:2153   crate::publisher::SEVENZ_MAGIC
```

That test builds a realistic 7z DSF container and asserts FUSE passes it
through byte for byte. It cares that the bytes are opaque, not who produced
them.

**The publisher barely uses the runtime.** Its entire reach into the rest of
the library is `crate::package` (the format types, the library index parser,
the spec version constant, the naming helpers) and `crate::config::format_size`,
used twice.

**The publish CLI is equally self contained.** Across 4,189 lines it names
`xearthlayer::publisher` 42 times, `xearthlayer::package` 5 times,
`xearthlayer::config` once, and the CLI's own error type 5 times. Nothing else.

**The cost is paid by the wrong binary.** Cargo's unit of dependency is the
crate, so a library that contains the publisher links the publisher's
dependencies into every binary built from it. Four crates are used only by the
publisher: `staticmap`, `tiny-skia`, `csscolorparser` and `sevenz-rust2`.
Removing those four from the graph removes **21 of 327 crates**:

```
attohttpc, rustls, rustls-webpki, webpki-roots      staticmap's HTTP client
staticmap, tiny-skia, tiny-skia-path, strict-num, png   coverage map rasteriser
sevenz-rust2, lzma-rust2                            DSF 7z containers
csscolorparser, uncased, phf, phf_generator, phf_macros, phf_shared,
siphasher, arrayref, bitflags, fastrand
```

`staticmap` brings **a second HTTP and TLS stack**, independent of `reqwest`,
into a long lived process that holds FUSE mounts and will never draw a map. The
coverage map is a reporting tool for a publisher. It should not be linked into
a flight simulator's filesystem daemon.

That is about to become permanent. `xearthlayer-daemon` (#142) does not
exist yet. Once it does, and once the multi-package release pipeline (#146)
enumerates the binaries, this becomes a property of a shipped service rather
than an artifact of a single crate.

## Goals

1. The publishing domain lives in its own library crate with its own
   dependencies, versioned and tested independently of the runtime.
2. Publishing is driven by its own binary, `xearthlayer-publish`.
3. No XEL runtime binary links a publishing dependency.
4. The package format has exactly one definition, shared by the code that
   writes it and the code that reads it.
5. No crate depends sideways on a peer, preserving the layering the
   service-UI decoupling design establishes.

## Non-Goals

- Changing the package format, the spec version, or any on-disk layout. This
  design moves code and draws boundaries. It changes no bytes.
- Changing publisher behaviour or CLI semantics beyond the binary's name.
- Splitting the publisher further, for example separating coverage and gap
  analysis from build and release. See Open Questions.
- Publishing the crates to crates.io. They stay workspace members.

## Design Decisions

### Three crates, not two

The publishing code cannot simply move out, because it shares the package
format with the runtime. `package` (2,533 lines across `core`, `installed`,
`library`, `metadata`, `naming`, `spec`, `types`) is the on-disk contract: the
two text formats, archive and part naming, and the `spec_version` major gate.

The publisher **writes** it. The runtime **reads** it, from
`manager/{installer,local,updates,client,cache,symlinks,traits}`,
`ortho_union/builder` and the FUSE layer.

| Option | Consequence |
|--------|-------------|
| `package` stays in `xearthlayer`, publisher depends on `xearthlayer` | The publisher then links FUSE, wgpu, moka and the entire runtime. Strictly worse than today, and a sideways dependency the layering forbids. |
| **`package` becomes a contract crate at the bottom of the graph** | Both sides depend inward on it. Chosen. |
| Duplicate the format in both crates | A publisher that writes a format its clients cannot read is a field failure with no compiler to catch it. Rejected. |

The chosen option is the same shape as `xearthlayer-proto` in the service-UI
design, and for the same reason: a pure contract with no XEL dependencies, that
every party depends inward on. The gain beyond decoupling is that the spec
version gate now lives in a crate the writer and the reader must both compile
against, so format compatibility becomes a versioned artifact rather than a
convention.

### One binary, not several

`xearthlayer-publish` keeps the existing subcommand tree (`init`, `scan`, `add`,
`list`, `build`, `urls`, `version`, `release`, `status`, `validate`, `coverage`,
`dedupe`, `gaps`).

Those subcommands operate on shared repository state discovered from one
repository root. Splitting them across executables would duplicate discovery
and argument parsing in every one of them, and a publisher's workflow runs
several in sequence against the same repository.

### `xearthlayer publish` is removed outright

No deprecation shim. Three reasons: publishers are a small and technical
audience who will read a release note, 0.5.0 already restructures the binary
set so a single coordinated change is cheaper than two, and a stub that exists
only to print a redirect is code that must be carried, tested and eventually
removed.

The CHANGELOG entry and the release notes carry the migration: every
`xearthlayer publish X` invocation becomes `xearthlayer-publish X`.

### Coverage map rendering stays in the publisher

`coverage.rs` and `region_colors.rs` are the heaviest dependencies in the group
and the least related to packaging. They stay in `xearthlayer-publisher`
anyway. A fourth crate for two files is churn, and coverage reporting is part
of what a publisher does with a repository. The separation is recorded here so
that a later decision to extract it does not have to rediscover that it is
separable.

### `xearthlayer-publish` is not in the meta-package

The `xearthlayer` meta-package depends on the daemon, the CLI and the TUI. It
does not depend on `xearthlayer-publish`.

`apt install xearthlayer` is a flight simulator user installing a streaming
service. Publishing tools are for the handful of people building regional
scenery packages, who can install one more package deliberately. This is also
what keeps the second TLS stack and the map rasteriser out of a default
install, which is the point of the whole exercise.

### The runtime keeps the read side of archives

`manager/extractor.rs` reads and extracts archives during package
installation. It stays in `xearthlayer`. Only `publisher/archive.rs`, which
writes and splits them, moves.

Both sides shell out to `tar` today, and #274 replaces both with pure Rust.
That work straddles this boundary, so the sequencing matters. See Relationship
to Other 0.5.0 Work.

## Design

### Crate Topology

```
xearthlayer-package                    (contract: formats, naming, spec gate)
   ^                      ^
   |                      |
   |                      |
xearthlayer          xearthlayer-publisher
(service library)    (publishing domain)
   ^                      ^
   |                      |
daemon / tui / cli   xearthlayer-publish
                     (publishing binary)
```

Dependencies point one way only. `xearthlayer` does not know the publisher
exists. `xearthlayer-publisher` does not know the runtime exists. Neither can
reach the other even by accident, because Cargo will refuse the cycle.

#### xearthlayer-package

The on-disk contract for scenery packages. Moved wholesale from
`xearthlayer/src/package/`:

| Module | Responsibility |
|--------|----------------|
| `core` | `Package`: region, type, version |
| `installed` | `InstalledPackage`: adds path and enabled state |
| `types` | `PackageType`, `ArchivePart` |
| `spec` | `CURRENT_SPEC_VERSION`, `SUPPORTED_SPEC_MAJOR`, `is_supported_spec_major` |
| `library` | `xearthlayer_package_library.txt` parse and serialize |
| `metadata` | `xearthlayer_scenery_package.txt` parse, serialize, validate |
| `naming` | `archive_filename`, `archive_part_filename`, `package_mountpoint`, `update_archive_version` |

Dependencies: `semver` (re-exported as `package::Version`), `serde`,
`thiserror`. Nothing else, and in particular nothing from XEL.

Two rules govern what may be added later. It holds **format knowledge only**:
no I/O beyond parsing bytes it is handed, no network, no path discovery. And
every statement the format makes must live here rather than in a caller, so the
documented facts include which checksum algorithm the metadata's checksum
fields use, since the writer and the verifier must agree and neither owns the
answer alone.

`InstalledPackage` is a borderline case worth naming: installation state is
arguably a runtime concern. It stays because it is `Deref`-composed onto
`Package` and the publisher's validation paths read the same shape. If it later
grows runtime concerns such as mount state, it moves to `xearthlayer`.

#### xearthlayer-publisher

The publishing domain, moved wholesale from `xearthlayer/src/publisher/`:
repository management, the Ortho4XP and overlay processors, DSF 7z
compression, archive building, metadata and URL generation, library index
management, release, versioning, coverage mapping, and the dedupe and gap
analysis tools.

Depends on `xearthlayer-package` plus its own third party set, which now
includes the four crates that were library-wide: `staticmap`, `tiny-skia`,
`csscolorparser`, `sevenz-rust2`.

It keeps its own `config.rs`, a repository configuration unrelated to XEL's
`config.ini`. That this already exists is evidence the domain was always
separate.

#### xearthlayer-publish

Binary crate, moved from `xearthlayer-cli/src/commands/publish/`, which is
already structured as a Command Pattern with `args`, `handlers`, `services`,
`traits` and `output` modules. It gains a `main.rs` and its own error type,
replacing the 5 uses of the CLI crate's error.

It runs **no preflight checks**. Today `xearthlayer publish` passes through the
preflight registry, and while the path checks exclude themselves by asking for
`command() == "run"`, the migration check does not: publishing on a machine
with a pre-0.5.0 config can trigger layout migration. A publisher has no
X-Plane installation to validate and no XEL layout to migrate. Separate
binaries make that correct by construction rather than by a growing list of
exclusions.

#### xearthlayer (service library)

Loses `package` and `publisher`, gains a dependency on
`xearthlayer-package`. Every current `crate::package::X` becomes
`xearthlayer_package::X`. Nothing else changes.

### Two Loose Ends

**`config::format_size`.** The publisher calls it twice, and it is the only
non-`package` reach into the library. It is presentation, not format knowledge,
so it does not belong in the contract crate. The publisher gets its own
formatter. Byte formatting is a dozen lines and the two domains have no reason
to agree on it: XEL's parses and formats config values and is bound by
round-trip requirements (#218), while the publisher's only prints sizes in
reports. If ultimately enough common shared functionality for formatting and
other utilities materialize, a common library or similar can be constructed.

**The FUSE test fixture.** The `#[cfg(test)]` reference to `DsfCompressor`
becomes a direct `sevenz-rust2` dev-dependency in `xearthlayer`. Cargo permits
dependency cycles through dev-dependencies, so `xearthlayer` could dev-depend
on `xearthlayer-publisher` instead, but that would reintroduce the coupling in
the one direction this design exists to remove, for a fixture. The test asserts
that FUSE passes an opaque 7z blob through unchanged, so it can build the blob
itself.

### SOLID Analysis

- **Single Responsibility**: each crate has one reason to change. The contract
  crate changes when the package format changes, which is the one event that
  should force both domains to react. The publisher changes when publishing
  changes. The runtime changes when streaming changes.
- **Open/Closed**: a second publishing front end, for example a web service
  that builds packages, is a new crate on `xearthlayer-publisher`. Nothing
  existing is modified.
- **Interface Segregation**: the runtime sees the package format and not the
  publisher. The publisher sees the package format and not FUSE, the cache or
  the executor.
- **Dependency Inversion**: the contract crate is pure abstraction with zero
  internal dependencies, and both domains depend inward on it. There is no
  adapter layer here because there is nothing to adapt: unlike the gRPC
  boundary, both sides speak the format natively.

## Migration Strategy

Four phases. Each leaves the workspace building, all tests passing, and every
CLI command behaving as it does today, with one exception called out in
phase 3.

**Phase 1: extract `xearthlayer-package`.** Create the crate, move the seven
modules and the module root, add the dependency to `xearthlayer`, rewrite `crate::package` to
`xearthlayer_package` across the runtime and the publisher. A pure motion
commit: no logic changes, and the test suite is the proof. This phase is
independently valuable and independently revertible.

**Phase 2: extract `xearthlayer-publisher`.** Move the 23 modules, move the
four dependencies out of `xearthlayer/Cargo.toml`, give the publisher its own
`format_size`, convert the FUSE fixture to `sevenz-rust2`. At the end of this
phase `xearthlayer` no longer compiles the publisher, which is where the 21
crate reduction lands. The publish CLI still lives in `xearthlayer-cli` and
now depends on `xearthlayer-publisher`.

**Phase 3: extract `xearthlayer-publish`.** Move the 7 CLI modules into a new
binary crate with its own `main.rs` and error type. Remove the `Publish`
variant from the CLI's command enum, its arm in `command_name`, and its
dispatch arm. This is the one phase with a user visible change, so it carries
the CHANGELOG entry and the release note.

**Phase 4: packaging and CI.** Add the build job, the package definitions per
format, and the release artifacts. See the service-UI decoupling design's
Packaging and Distribution section, updated by this design.

Phases 1 and 2 are invisible to users. Phase 3 is a one line change to a
publisher's muscle memory. Nothing is staged behind a feature flag because
nothing changes behaviour.

## Testing Strategy

The tests move with the code they cover, and the move is the test. Every
publisher and package test exists today and must pass unchanged in its new
crate, because this design changes no behaviour. A test that needs editing to
compile is expected (import paths), a test that needs editing to pass is a
defect in the move.

Three things need tests that do not exist today:

**The boundary itself.** `xearthlayer-package` must not acquire XEL
dependencies and `xearthlayer` must not acquire publishing ones. Cargo enforces
the cycle but not the taste, so the guard is a check over the manifests rather
than a unit test: the contract crate's dependency list is short enough to
assert exactly.

**The new binary's entry point.** `xearthlayer-publish` needs the smoke
coverage the subcommands inherited from the CLI crate: that it parses, that
`--help` lists every subcommand, and that an unknown subcommand fails with a
useful message.

**The removal.** A test asserting `xearthlayer publish` is gone, so that a
future merge cannot quietly restore the variant.

The publisher's own 4,189 lines of CLI tests, including `tests.rs`, move intact
and continue to cover the command layer.

## Relationship to Other 0.5.0 Work

**Do this before #140.** Three reasons, in descending strength.

1. **#142 should be born clean.** The daemon does not exist yet. Splitting
   first costs the same work and produces no cleanup task. Splitting later
   means removing a dependency from a shipped service binary.
2. **#146 enumerates the binaries.** The multi-package release pipeline, the
   deb, rpm and AUR structures and the meta-package all hard-code the binary
   set. Doing this afterwards means rewriting packaging work that was correct
   when written.
3. **#145 assumes the CLI keeps `publish`.** That assumption is now wrong
   either way, so the service-UI design needs revising regardless. Better to
   revise it once, before the work depending on it starts.

**#175 is disjoint.** CacheLocator SSOT touches cache and prefetch. This
touches `publisher/`, `package/` and `commands/publish/`. They share no file,
so the order between them is a scheduling choice.

**#274 straddles the boundary.** Replacing the `tar` and `split` shell-outs
covers `manager/extractor.rs` (read side, stays) and `publisher/archive.rs`
(write side, moves). If #274 remains the designated slip candidate to 0.6.0,
this design makes it two independent pieces of work in two crates, which is
the better outcome. If #274 is pulled into 0.5.0, do it after the move rather
than across it.

**#124 is merged**, so nothing is in flight inside the publisher. The DSF
compressor moves as it stands.

## Open Questions

**Should coverage and gap analysis be their own crate?** Deferred, not
rejected. They are reporting tools over a repository rather than steps in
building one, and they own the heaviest dependencies in the group. Revisit if
`xearthlayer-publisher` grows a second consumer that wants the packaging
without the rendering.

**Does `xearthlayer-publish` follow the workspace version?** All crates share
`workspace.package.version` today, which means the publishing tool would take a
version bump for a prefetch fix. Since it ships as its own package outside the
meta-package, an independent version is defensible. Left as is for 0.5.0
because a single workspace version is one less thing to get wrong during a
release, and the packaging work in phase 4 is the natural place to revisit it.
