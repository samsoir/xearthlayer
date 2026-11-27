# Night Lighting Design

**Status**: Phase 1, phase 2a and phase 2b implemented (packaging, black serving, real lit sizes, the `LitGenerator` trait with its CPU implementation, bilinear world fields, warped soft class edges, hex stochastic tiling, and a coalesced service), plus the lit disk cache, LED lighting and road lights; tuning continues by flight
**Created**: 2026-10-17
**Branch**: `blackmarble`, proposed for `develop/0.5.0` so others can test it

## Overview

X-Plane 12 lights its terrain at night through the terrain type: the urban
and industrial `.ter` files carry a lit texture, and the global scenery
assigns those terrain types to city triangles. Ortho4XP replaces that mesh
with its own and writes one `.ter` per ortho texture with no lit texture, so
ortho scenery goes dark beyond the reach of autogen lights.

OXO (the Ortho4 XEarthLayer Orchestrator) now builds night lighting into the
tiles it delivers, in two parts:

- every land `.ter` carries a lit line naming a texture that does not
  exist on disk:
  ```
  TEXTURE_LIT ../textures/<row>_<col>_<map><zoom>_LIT.dds
  LUMINANCE 150
  ```
- one **recipe** per 1° tile, `<tile>.oxn`, describing the lighting the
  tile needs: a class layer naming Laminar terrain types (3 arc-seconds) and
  a brightness layer from NASA's annual VIIRS night lights (15 arc-seconds).

XEL turns the recipe into the lit texture at run time, on the user's
machine, from the lit art already in the user's X-Plane installation. That
art cannot be redistributed, so no package carries it.

This document is XEL's side. It rests on three OXO documents, kept in the
OXO repository:

- *XEarthLayer night lighting: requirements* (R1 to R21, referenced
  throughout this document)
- *Night recipe format*, version 1: the contract between the two projects
- *Night lighting design*: OXO's side and the in-sim evidence

### Facts this design rests on

Established in the simulator (X-Plane 12 beta, 2026-10-16), not inferred:

1. A lit texture renders on Ortho4XP's terrain and uses the ortho texture's
   own coordinates: 0 to 1 across one ortho texture, north up.
2. Lit texture size is independent of the base texture's size. A 4 × 4
   black DDS is valid and draws nothing, because lit is added to the base
   colour.
3. `LIT_FADE_PARAMS` has no effect on this terrain. A distance fade works by
   making the top mip level(s) of the lit DDS black. Re-tested 2026-10-18
   with a control (a build without blank levels), on 111 tiles south of
   KSFO, with `TEXTURE_LIT` and `TEXTURE_LIT_NOWRAP`: no effect. Laminar's
   own `_FAR_LIT` art is equally bright at every mip level, so its fade is
   done by the engine, most likely only for projected terrain.
4. X-Plane's texture quality setting (`renopt_tex_res_04` in
   `Output/preferences/X-Plane.prf`, 4 = full down to 0) drops one top mip
   level per notch at load time. VR has its own setting.
5. Laminar's lit art is nine 1024 × 1024 DXT1 textures under
   `Resources/default scenery/1000 world terrain/textures12/urban_FAR/`,
   reached from terrain names through `library.txt`. Each lit `.ter` gives
   its pattern's ground scale in metres as the first value of
   `LIT_FADE_PARAMS` (3072, 4096 or 8192).
6. Lit ground tints the cockpit's ambient light, so brightness must stay
   restrained.
7. X-Plane pages a lit texture's mip levels in one at a time (seen in the
   FUSE reads, 2026-10-18): the first load reads only the small levels at
   the end of the file, and the first visible level and the top level
   arrive tens of seconds later as the aircraft closes. The mip fade
   therefore happens per texture, as each level is paged in, and the
   level shown is the largest one loaded (fully read for 19 of 1,006
   ZL16 lit textures, 2026-11-14).

## Decisions

| Decision | Choice | Why |
|---|---|---|
| Light model | Laminar pattern × VIIRS gain, with some neighbourhoods recoloured LED white | The project owner's decision. The class picks the Laminar lit texture for street-scale structure; the brightness layer scales its intensity. Nothing lights outside Laminar's urban and industrial classes. The pattern gives the street structure and the sodium hue; the LED stage (see LED lighting) changes the hue of whole neighbourhoods and keeps the structure |
| Where lit work runs | A separate lit path beside the ortho executor, never a job on the demand or prefetch lane | Lit generation must not delay ortho generation (R17) and must not share coalescing or cache keys with it (R8). A separate path guarantees both: lit inodes are routed to the lit source, and the ortho request path returns the black texture for a lit name as a guard |
| Texel density | 1024 px at the first visible level of a ZL16 texture (about 7.5 m per texel), scaling by zoom | The project owner's decision: about 2.8 MB of video memory per urban ZL16 texture, against about 10.7 MB at 2048 px |
| Decoding Laminar's art | `ddsfile` (header and mip layout) and `bcdec_rs` (a pure Rust port of `bcdec`) | No DDS decoder exists in XEL. Decoding happens once per art file per session, so correctness and size matter, not speed |
| Encoding lit textures | `IspcCompressor`, named in `bc1_levels` and called level by level through `dds::compress_level`, whatever `texture.compressor` says | A lit payload is about a tenth of an ortho tile, so the GPU channel worker is not wired into the lit path. Lit bytes are versioned by `LIT_GENERATOR_VERSION` and fingerprinted on disk on the assumption that one kernel makes them, so the kernel is named rather than taken from configuration (#392) |
| Recipe container | `ciborium` for CBOR, `flate2` for the zlib layers | CBOR is the format's choice; `flate2` is already a dependency |
| Caching | Memory, then a disk tier of payloads, keyed by footprint and validated by a fingerprint | Regeneration costs about 130 to 170 ms of CPU per ZL16 texture, which adds up at scene load, and is deterministic. The fingerprint covers every input it can observe, and `LIT_GENERATOR_VERSION` is bumped when generated bytes change, so the disk tier needs no manual invalidation when X-Plane, its settings or the art change |
| Configuration | `night.enabled` and `night.brightness` | `brightness` is a visual preference, not a performance knob. It is a development setting, reviewed before release |
| Recipes in a package | `night/<10° block>/<tile>.oxn` | Hidden from X-Plane by the union index's existing root allowlist, on this and every XEL version since 0.3.0 |
| Publishing night packages | Staging library only until XEL 0.5.0 is the minimum client | Pre-0.5.0 XEL (filename regex without `(_LIT)?`) answers ENOENT for every `_LIT.dds` an OXO night `.ter` names, so a night package must not reach any library a pre-0.5.0 client reads. Whether to also bump the package spec version is open |
| Delivery | Two phases: packaging and black serving first, the generator second | Phase 1 alone makes OXO night packages publishable and safe |

## Architecture

```
 package/                          X-Plane
  night/<block>/<tile>.oxn          │ opens 25712_12960_BI16_LIT.dds
  terrain/*.ter (lit line)          ▼
  textures/                   ┌──────────────┐
                              │ FUSE (union) │── ortho .dds ──▶ executor lanes (unchanged)
                              └──────┬───────┘
                                     │ _LIT.dds
                                     ▼
                           ┌───────────────────┐  has_light  ┌──────────────┐
                           │ LitTextureService │◀───────────▶│ RecipeStore  │
                           │ coalesce, cache,  │             └──────────────┘
                           │ own CPU budget    │  patterns   ┌──────────────┐
                           └─────────┬─────────┘◀───────────▶│ LaminarArt   │
                                     │ generate              └──────────────┘
                                     ▼                       ┌──────────────┐
                            LitGenerator (trait) ◀───────────│ XPlanePrefs  │
                            CPU implementation now           └──────────────┘
                                     ▲                       ┌──────────────┐
                                     └── road glow ──────────│ RoadService  │◀── DSF hints (FUSE)
                                                             └──────────────┘
```

The ortho path is untouched apart from the filename parser learning a
texture kind, and the FUSE `lookup` and `open` handlers telling the lit
source when a DSF is reached (for road lights; with hints off this is one
branch on a bool, see Trigger). Everything else is new code in
`xearthlayer/src/night/`:

| File | Responsibility |
|---|---|
| `night/recipe.rs` | Read and validate one `.oxn` file |
| `night/store.rs` | `RecipeStore`: find, load, cache and evict recipes |
| `night/laminar/library.rs` | Parse `library.txt`, resolve terrain names |
| `night/laminar/ter.rs` | Read a Laminar `.ter`'s lit texture and pattern scale |
| `night/laminar/art.rs` | `LaminarArt`: name to decoded lit pattern, cached |
| `night/footprint.rs` | A lit texture's ground footprint |
| `night/layout.rs` | The lit DDS layout: sizes, the black levels above the payload (those X-Plane drops and the top it uploads), where the payload sits, and whether a read reaches it |
| `night/prefs.rs` | `XPlanePrefs`: the texture quality setting |
| `night/generator/mod.rs` | The `LitGenerator` trait, `LitRequest` and `encode_visible` |
| `night/generator/bc1_repair.rs` | The BC1 endpoint repair of blocks that tint dim texels green or purple |
| `night/generator/gain.rs` | The gain curve |
| `night/generator/noise.rs` | Integer hashing and world-anchored value noise |
| `night/generator/field.rs` | `RecipeField`: recipe layers as continuous functions of world position |
| `night/generator/tiling.rs` | Hex-grid stochastic tiling in Web Mercator metres |
| `night/generator/cpu.rs` | `CpuLitGenerator`, the CPU implementation |
| `night/service.rs` | `LitTextureService`: the FUSE-facing entry point |
| `night/disk.rs` | Lit disk entries and the fingerprint of a payload's inputs |
| `night/roads/` | Road lights: the simulator's road network as chains of lamps (see Road lights) |

### Recipe reader (`night/recipe.rs`)

Implements the recipe format, version 1, and nothing more:

- The file is the four magic bytes `OXN1` followed by one CBOR map. The
  reader rejects a wrong magic, a major version other than 1, a `format`
  that differs from the magic's digit, and trailing bytes after the map.
- It accepts any valid CBOR encoding, not only the deterministic one.
- Unknown keys at every level and unknown layer names are ignored. A
  missing `class` or `brightness` layer reads as all zeros.
- A layer whose `zlib` stream does not inflate to exactly
  `width × height` bytes is rejected, and so is a class cell indexing past
  the palette. Inflation is bounded by `width × height`, so a hostile file
  cannot allocate more than it declares (the cache integrity model's
  bounded-read rule).
- Layer sizes are always taken from the layer's `width` and `height`.
- The result is a `Recipe`: tile, palette, class grid, brightness grid and
  the brightness `scale` (`r0_nw`, `rmax_nw`).

### RecipeStore (`night/store.rs`)

The union index hides `night/` from X-Plane, so the store reads recipes
through each ortho source's own path, not through the mount:

- For a 1° tile, it looks for `night/<block>/<tile>.oxn` in each ortho
  source, in the union's priority order (patches before regional
  packages), and takes the first it finds. The block is Ortho4XP's: each
  coordinate floored to a multiple of 10 (`+37-123` is in `+30-130`).
- Recipes load lazily on first use and live in a cache bounded by bytes,
  evicted least recently used (LRU, for the reason the payload cache is; #387),
  each about 1.5 MB decoded. "No recipe for this tile" is cached too, so
  countryside costs one lookup per tile per session.
- A rejected file is logged once and then treated as a dark tile.

### LaminarArt (`night/laminar/`)

Resolves a recipe palette name to a decoded lit pattern, through the user's
own installation, so it follows whatever the installed X-Plane uses (R9):

1. `library.txt` in `1000 world terrain` maps library paths such as
   `lib/g10/terrain10/urban_tmp_sdry3.ter` to files such as
   `terrain12/urban_tmp_sdry3.ter`. The parser handles the `EXPORT`
   family of directives and conditional `REGION` blocks, taking the
   unconditional (global) definition.
2. The resolved `.ter` is read for its lit texture (`TEXTURE_LIT`, or
   `LIT_TEX_NOWRAP`) and the first value of `LIT_FADE_PARAMS`, the
   pattern's ground scale in metres.
3. The lit texture is decoded with `ddsfile` and `bcdec_rs` to RGB at its
   top level and kept as a `LitPattern { texels, size, ground_scale_m }`.

A name is lit if and only if it resolves to a `.ter` with a lit texture. A
name that resolves to nothing lit is dark. The X-Plane installation comes
from the existing detection (`XPlaneEnvironment::installation_path()`).
All nine patterns together are about 36 MB decoded, loaded on first use.

### Footprint (`night/footprint.rs`)

Ortho4XP names a texture `<til_y_top>_<til_x_left>_<map><zoom>`, covering
Web Mercator tiles x = `til_x_left` to `til_x_left + 15` and y =
`til_y_top` to `til_y_top + 15` at that zoom (R5). From the name alone the
footprint gives:

- its latitude and longitude bounds, and the mapping from a texel to a
  latitude and longitude, north up, row 0 at the north edge;
- the 1° tiles it touches, longitudes wrapped into [-180, 180) at the
  antimeridian (`Footprint::tile_ranges` keeps them unwrapped, so a field
  across the line stays contiguous, #434). A footprint can straddle tile
  boundaries, and each texel is computed from the recipe of the tile
  containing it;
- `has_light` (`RecipeField::has_light`, beside the generator's sampling
  so the two share one model): whether the generator may light any texel
  of the footprint. It reads recipes only, never generates pixels, and
  answers the file size before generation (R7). See the probe below.

### Lit DDS layout (`night/layout.rs`)

With the texture quality setting `t` (0 to 4):

- X-Plane drops `4 − t` top levels at load. XEL puts
  `B = (4 − t) + 1` levels above the first visible one (R15): the ones
  X-Plane drops are black, and so is the one it uploads (the uploaded top,
  twice the first visible level's size): X-Plane's 3D objects light the
  ground there (see Uploaded top below).
- The first visible level `F` is 1024 px for a ZL16 texture, halving per
  zoom level above 16 and doubling per zoom level below it (512 px at ZL17,
  256 px at ZL18), about 7.5 m per texel throughout.
- The top level is `F × 2^B`, with a full mip chain down to 1 × 1 and
  BC1 (DXT1) blocks throughout.

| Setting `t` | Blank levels | Top level (ZL16) | File size | Uploaded by X-Plane |
|---|---|---|---|---|
| 4 | 1 | 2048 | 2.8 MB | 2.8 MB |
| 3 | 2 | 4096 | 11.2 MB | 2.8 MB |
| 2 | 3 | 8192 | 44.7 MB | 2.8 MB |

Where each texture's levels change is set by the texel size of the
level X-Plane uploads first, so the first visible level stays fixed in
metres at every setting. This is why lit VRAM does not fall with the
texture setting. A layout that shrank the visible level with the setting
(the same file at every setting, VRAM quartered per notch) was flown on
2026-10-18 and moved the cut-out (the uploaded top was black then) 2x
farther out per notch.

**Below ZL16 (#431).** Fixed in metres, the first visible level doubles per
zoom level below 16: 4096 px at ZL14 and 8192 px at ZL12 and ZL13 at t = 4,
16 to 64 times a ZL16 generation's work and memory, with an uploaded top of
up to 16384 px. Below ZL16 it is capped at `MAX_FIRST_VISIBLE_BELOW_ZL16`
(2048 px). That changes ZL12 to ZL14 at t = 3 and 4 only (at t <= 2,
`MAX_TOP_LEVEL` already held them to 2048 px or less); ZL15 and every zoom
from 16 up keep their layouts byte for byte. On a capped texture the lit
layer starts farther from the aircraft, by the factor its density fell, and
steps where it meets ZL16. The Laminar pattern is sampled at the capped size,
but the road glow is not: the raster floors a glow's sigma at one texel, so
at 15 to 60 m texels lamps would spread into thick lines. A capped texture's
road field is rastered at the uncapped density, at most twice the linear
size (`LitLayout::road_supersample()`), in bands of 128 output rows (one
band of samples, 4 MiB for a 2048 px texture), and each 2 x 2 block of
samples is averaged into its texel (`RoadField::raster_supersampled`); the
profile enters the light linearly, so the mean is the linear-light mean.
The probe keeps the reach at the output size, which is never narrower, so it
still never answers dark where the generator lights. Capped textures change
length, so their old disk entries fail the length check; their fingerprint
also hashes the supersample (only when above 1, so every other fingerprint
is unchanged), because at ZL12 to 14 t = 2 and t = 3 or 4 give the same
length with different roads. No packaged world has lit textures below ZL16
yet.

**Uploaded top.** X-Plane loads a texture's top level only for the
textures nearest the aircraft, and shows it over the whole texture,
under its 3D scenery. In the flight of 2026-11-14 at KCCR it fully read
the top level of 19 of 1,006 ZL16 lit textures, the first visible level
of 25 more, the next of 338, and only the small levels of the other 624.
The uploaded top is black: the ground under X-Plane's 3D scenery is lit
by its objects alone, and the lit layer switches off right around the
aircraft, after the dimmed levels of the fade ramp. Lighting it was
tried and reverted. Filled from the first visible level (each texel
repeated over 2 x 2 at a quarter of the light), flight 7 showed it too
bright and too coarse; generated on its own at twice the first visible
size (3.75 m texels at ZL16) at an eighth (flight 8) and then a twelfth
of the light, it was dim enough, but its street lamps were too coarse
and placed too inaccurately beside X-Plane's own 3D lights, and broke
the scene (flights after beta.1, 2026-11-19). It cost about 720 ms of
generation per top at 2048 px against about 190 ms for a payload.

**Fade ramp.** Because X-Plane pages levels in one at a time (fact 7), a
texture would jump from full light straight to its top level, black
before the ramp and before the top was lit, and was seen to switch off as
one visible square. The first visible level
gives a quarter of the light and the next half (`FADE_RAMP`,
`(1/4)^(1/2.2)` and `(1/2)^(1/2.2)` in stored value, since X-Plane decodes
sRGB), so each texture steps from full to half to a quarter, one stop per
level, then black at the uploaded top.
The chain is downsampled from the undimmed image, so every coarser level
is at full light, and the ramp costs no bytes. The nearest level is cut
back so the ground under X-Plane's 3D roads and lights is only faintly
lit, and two levels make the fade span twice the mip range: on a wide
view the screen's stretch puts the blank-to-visible change about a level
farther out at the sides than at the centre and squeezes a one-level fade
into a few pixels there (KCCR, 2026-11-14). The nearest level first gave
an eighth of the light, and in flight it did not show at all: over the 20
KCCR textures of the road light measurements, 0.08% of its texels stayed
above a linear 0.002, against 2.9% at full light and 0.4% at a quarter.
BC1 was not the cause; it kept 87% of the eighth's light (92% of a
quarter's). Until then one level was dimmed, to 2/3 of the
light. A two-step ramp flown on 2026-10-18 (2/3 and 1/3) read as black
before the 3D lights were drawn, but it scaled stored values (1/3 stored
is about 9% of the light), and no road lights covered the hand-off yet.

**Mips in linear light.** The chain below the first visible level is
averaged in linear light (`lit_chain`, #363), not by averaging stored
values as the ortho encoder did then. The ortho encoder now averages in
linear light too, but `lit_chain` stays separate (f32, gamma 2.2, bytes
versioned with the generator). Lit art is bright points on black, and a
stored-value average counts a lit texel beside dark ones at a fraction of
its light. Measured on a ZL16 texture over Mexico City, the stored-value
chain kept 82% of the light at the first coarser level, 53% at the third
and 24% at the sixth, and it warmed as it went: blue, the sparsest channel
of warm light, lost the most, so linear B/R fell from 0.45 to 0.33 by the
sixth level (0.17 to 0.04 for sodium alone). Distant towns, which X-Plane
draws from those levels, read dimmer and more orange than the near ground.
Each level is now averaged from the linear values of the level above, not
from its rounded bytes, so every level keeps the light and colour of the
first.
The linear values come from the shared 256-entry table (`linear_table`),
and each averaged level is stored through `led::ByteEncoder`, which gives
exactly the byte the sRGB encode followed by rounding gives, without a
`powf` per channel (#439). Every generated texel is opaque, so an alpha of
exactly 1.0 is stored as 255 without rounding.

**One level at a time** (#440). `lit_chain` takes the first visible level
by value and visits the chain one level at a time, in place. The first
level's bytes are dimmed, floored, compressed into their slice of the
payload (`bc1_level_into`) and repaired. Its linear values (three `f32`
per texel; alpha is not kept, every lit texel being opaque, and ISPC's
BC1 kernels and the repair ignore alpha, which tests pin) are then
halved in place for each next level, whose bytes are stored into one
reused buffer and encoded the same way before the level after is made.
The encode used to hold the whole chain at once, in linear and in
stored form: 24 MiB for a 1024 px texture. It now holds the first
averaged level's linear values (512 x 512 x 12 bytes, 3 MiB) and the
payload (699,064 bytes): 3,844,856 bytes measured, the 64 over being a
padded copy of the smallest level for its kernel. The road field is
dropped before the encode, so a generation no longer holds it while it
encodes. The bytes are unchanged.

**What lit bytes depend on.** A lit payload is a function of its inputs
(recipes, Laminar art, road networks, `night.brightness`, the texture
setting), of the CPU's ISPC kernel class and of the platform's libm. ISPC
compiles one BC1 kernel per instruction set and picks one at run time; on
x86 the kernels with FMA (AVX2, AVX-512) round differently from those
without (SSE2, SSE4, AVX) and give other bytes for most images, though they
agree on the golden test's fixture. `powf`, `exp`, `log`, `tan`, `atan`,
`sinh` and `cos` come from the platform's libm (glibc on Linux), and
`led::ByteEncoder` builds its thresholds at first use by bisection with
`powf`, so the chain's encode still depends on libm although it no longer
calls `powf` per texel. `ByteEncoder` is exact wherever the platform's
`powf(x, 1/2.2)` never decreases, which `the_byte_encoder_exhaustive` has
checked on x86 glibc; it is to be run on Apple Silicon before 0.5.1.
`floor` and `round` are Rust's own routines and exact. The lit fingerprint
covers none of this: a lit cache moved between machines, or kept across a
libm update, can serve bytes another machine would have made slightly
differently, which is harmless. Byte-identity tests and before/after
comparisons must run on one machine.

**BC1 floor.** BC1 stores colours as RGB565, so below about 4 in stored
value red and blue round to 0 while green keeps a 4: very dim texels
decode green or purple, and in flight 7 (KCCR, 2026-11-14) green and
purple texels showed in the lit layer. Over 20 KCCR ZL16 textures at
1024 px the generator itself makes no green or purple dim texels. Every
encoded level is therefore floored before BC1, after its dimming: a texel
whose largest stored R, G or B is below 8 (`BC1_FLOOR`) becomes black,
its alpha untouched. Counted over every decoded texel after BC1 at full
light (green when decoded G > max(R, B) + 2, purple when
min(R, B) > G + 2), two probes found about 150,000 green texels without
the floor and about 3,900 green and 13,500 purple with it, the floor
keeping 0.916 of the decoded light against 0.960 without. Their purple
counts without the floor differ, 61,088 and 22,016, for a reason not
found; with the floor they agree.

At the dimmed levels as served the counts are lower: 1,727 green and
8,740 purple over the 20 first visible levels at a quarter, and 639 green
and 4,619 purple over three 2048 px tops at an eighth. The floor no longer
carries the tint fix alone: the texels left just above it are the ones the
encoder tints, which the endpoint repair below corrects in measurement
(flight is the check on the GPU's decode).

**BC1 endpoint repair.** Flight 9 (KCCR, 2026-11-14) still showed whole
road lamps lavender beside grey-white ones, and pink and green specks in
towns, in the distant levels too. Near black the RGB565 grid has no neutral
colour between 8 and 16 in red and blue (decoded 0, 8, 16, 25), while green
also has 4 and 12 (0, 4, 8, 12, 16), and ISPC quantises each endpoint's
channels on their own. In blocks mixing a dim lamp with black it puts dim
near-white texels on lavender codes: a lamp of `(9, 7, 5)` decodes
`(8, 4, 8)` (where nearest rounding would give the neutral `(8, 8, 8)`),
`(17, 14, 12)` decodes `(16, 12, 16)`. A probe over 20 KCCR ZL16 payloads
and 5 tops at 2048 px, encoded as served, found the tints introduced by
BC1 (decoded tinted at a margin of 2, the pre-encode level not) at 3,009
green and 30,458 purple; 99.6% of the purple had a source largest channel
of 8 to 15, near neutral, in mixed blocks, and 60% sat on road texels; the
generator's own levels had 8 green and no purple. Raising the floor (to
12, 16 or 24) left green and lost 20% to 76% of the light; moving texels
onto the 565 grid before encoding did not survive the encoder's refit and
raised green eightfold.

After ISPC, `generator/bc1_repair.rs` looks at every block of every
encoded level (in `bc1_levels`, level by level) whose decode has a
tint its source does not have, at a margin of 2 (`TINT_MARGIN`), on a
texel whose largest decoded channel is at least `BC1_FLOOR`. It searches
the block's endpoints by coordinate descent from the original endpoints
with their indices chosen anew, each endpoint's R, G and B one 565 step
down or up, keeping every step that lowers the block's error, until none
does or 64 sweeps have run (`MAX_SWEEPS`; the probe's encodes needed at
most 4), re-choosing the indices for every candidate. A texel's
error is `4 x ((dR - dG)^2 + (dB - dG)^2)` (`CHROMA_WEIGHT`) plus the
square of its Rec.709 luminance difference plus 1000 (`TINT_PENALTY`) if
it decodes with a tint its source does not have, in stored values. The
block keeps its mode (four colours while `c0 > c1`, three otherwise,
whose transparent fourth entry is never chosen) and is replaced only
when its error strictly drops. Blocks whose endpoints are both below the
floor are skipped, as no colour of theirs can be lit. The repair works
in place on the encoded bytes and allocates nothing. On the same 25
encodes it leaves no tint introduced at a margin of 2 (from 3,009 green
and 30,458 purple), and at a linear-light ratio of 1.5 (green when G >
1.5 x max(R, B) after an sRGB decode, purple when min(R, B) > 1.5 x G) 1
green and 152 purple against 2,996 and 31,220, keeping 0.999 of the
light (the tops 1.001). It costs about 1.5 ms per 1024 px payload and
3.2 ms per 2048 px top (release, KCCR r1582 c659), against about 190 ms
(185 to 195 measured) and 700 ms to generate them. A margin of 4 is
blind to it: the commonest codes differ from green by exactly 4, so at a
margin of 4 BC1 seems to introduce 2 green and no purple, yet `(8, 4,
8)` carries 4.6 times the green's light in red and blue and `(16, 12,
16)` 1.9 times. The repair is judged against a D3D-exact decode; GPUs
round the interpolated colours by about 1.

A footprint without light is a fixed **minimal black DDS**: 4 × 4, BC1,
with its full chain (4, 2 and 1 px levels), all blocks zero, 152 bytes
(R6).

The levels X-Plane drops are never stored. A lit texture's bytes are
`header ‖ zero region ‖ payload`: a read inside the zero region (the
levels X-Plane drops and the uploaded top) is answered with zeros and
generates nothing; `LitLayout::reaches_payload` says whether a read
reaches the payload, which is generated only then. `LitLayout::read_lit`
composes the bytes from the payload it is given; a payload that is
absent or of the wrong length
reads as zeros, and any split of a read gives the same bytes as one whole
read. A read wholly inside the zero region, up to 1 MiB, is a slice of one
static zero buffer (`Bytes::from_static`), so the most common lit read
allocates nothing; a read with header or payload bytes is composed once
into its own buffer (#434). The payload (the visible levels, about 0.7 MB for an urban ZL16
texture) is cached in memory and on disk, the top (2 MiB at ZL16) in
memory only.

**Risk.** At `t = 0` the top level would be 32768 px. Whether X-Plane
accepts a header that large, even though it only uploads from level 4
onward, is unverified. The fallback is to cap the top level at 16384 px
and accept a 512 px first visible level at the lowest setting. The tuning
flight settles this.

### XPlanePrefs (`night/prefs.rs`)

The texture setting comes from a `TextureSettingSource` trait, so where it
comes from can change without touching the layout, the service or FUSE:

```rust
pub trait TextureSettingSource: Send + Sync {
    /// X-Plane's texture quality, 0 (lowest) to 4 (full).
    fn texture_setting(&self) -> u8;
}
```

- The implementation XEL ships, `StartupTextureSetting`, reads
  `renopt_tex_res_04` from `Output/preferences/X-Plane.prf` once, when XEL
  starts, and holds it for the session.
- If the file is missing or unreadable, it assumes the full setting
  (`t = 4`) and logs once (R16).
- The setting feeds the blank-level count and the file size. Because it
  never changes within a session, a lit file's size never changes either,
  so the kernel's attribute cache can never serve a stale size, on Linux or
  on macOS (which has no direct I/O). A change to the texture setting takes
  effect the next time XEL starts.
- Following the file while running stays possible: a watching
  implementation of the same trait can replace `StartupTextureSetting`.
  It would also have to answer the stale-size question (attribute
  timeouts, macOS page cache), which is why it is not the default.
- VR's `vr_renopt_tex_res_04` is not followed. This is a documented
  limitation.

### LitGenerator (`night/generator/`)

```rust
pub trait LitGenerator: Send + Sync {
    /// `size` x `size` pixels, north up, black wherever nothing is lit.
    fn generate(&self, request: &LitRequest<'_>) -> RgbaImage;
}
```

`LitRequest` carries the texture name (footprint and zoom), the size of the
first visible level, `night.brightness`, the LED model (`led`), the road
glow raster (`roads`, a `RoadField`) with its `RoadModel`, and lookups:
recipes by 1 degree tile, the package region each recipe was read from
(`regions`) and lit patterns by palette name. The trait leaves room for a
GPU generator later (R17); the CPU implementation comes first.

`generate` is infallible. Once recipes and patterns are resolved there is no
failure mode: a missing recipe or pattern is black, not an error. Encoding
can fail, so it lives outside the trait, in `encode_visible`, whose error
the service turns into a black texture.

For each texel of the first visible level, the CPU implementation:

1. **Places the texel in the world.** Its latitude and longitude, and from
   them its Web Mercator position.
2. **Weights the classes.** The class layer is sampled at the four nearest
   cell centres, with the sample position first warped by a smooth,
   world-anchored value noise (`noise.rs`) of 0.5 cell amplitude and 2 cell
   wavelength (about 45 m and 180 m at 3 arc-seconds). Each lit class gets
   a bilinear weight, merged per class; class 0 and cells without a recipe
   carry no weight. The warp breaks the 90 m cell grid into organic edges
   (R12). Cells are indexed globally (row 0 at the north pole, column 0 at
   180 W), so a sample near a 1 degree edge reads the neighbouring tile's
   cells, and `RecipeField` fetches every recipe a footprint plus a small
   margin touches once. Each tile's layers are read on that tile's own
   grid and brightness scale (#434): a point samples on its own tile's
   grid, or, where its tile lacks the layer, on the nearest tile's that
   has it (ties to the smaller tile key), reading cells only from tiles on
   that grid, so the value depends on the ground and never on which
   footprint asks; before, a tile whose grid differed from the first
   recipe's read as missing and every tile used the first recipe's scale,
   so two adjacent textures could disagree along their shared edge. Every
   installed recipe today has the same grids (1,200 class and 240
   brightness cells per degree) and scale, so the change alters no real
   texture and `LIT_GENERATOR_VERSION` stays 10; a recipe on another grid
   is a new recipe identity, so a new fingerprint. Tile longitudes wrap
   across the antimeridian, so a footprint at the line reads the recipe
   and the roads of the tile across it (the pattern tiling and warp noise
   are not periodic, so a pattern seam at the line remains).
3. **Samples each weighted class's pattern** by hex-grid stochastic
   tiling (`tiling.rs`), anchored to world position at the pattern's own
   ground scale (R11):
   - A triangle lattice (the dual of a hex grid) is laid over Web Mercator
     metres, with cells the size of the pattern's ground scale.
   - Each hex vertex hashes its integer grid coordinates to a random
     offset and a rotation in 90° steps.
   - A texel blends the pattern as transformed by its three nearest
     vertices, with barycentric weights raised to `BLEND_SHARPNESS = 4`
     and renormalised so the blend keeps the pattern's contrast. Each
     vertex converts world position to pattern coordinates with the
     ground scale scaled by cos(latitude) at that vertex, so the result
     has no global shear and is continuous everywhere.
   - Because the transforms depend only on world position, two textures
     compute the same light for the same ground, so edges between
     textures and 1° tiles have no seam.
4. **Applies the gain** (`gain.rs`). Brightness is interpolated
   bilinearly in its byte (log) domain, with 0 where a neighbour has no
   recipe, and converted to radiance `R` with the recipe's `scale`. The
   gain is a linear ramp in `ln(R)` from a floor radiance (gain 0) to a
   ceiling radiance (gain 1), raised to a gamma, then multiplied by
   `night.brightness` and clamped to 0 to 1 (R10). The floor, ceiling and
   gamma are constants in code, set by the tuning flight; the provisional
   values are a floor of 1.6 (a gain of about 0.05 at 2), a ceiling of
   150 nW·cm⁻²·sr⁻¹ and a gamma of 1.0.
5. **Recolours LED neighbourhoods** (`led.rs`, see LED lighting): where a
   texel's patch is LED, the pattern sum is replaced by white of the patch's
   colour temperature before the gain applies.
6. **Outputs** the (possibly recoloured) weighted sum of class patterns ×
   gain, as RGB. An LED texel is held to at least the byte luminance of the
   sodium bytes it replaces (see LED lighting).
7. **Adds road light** where the road raster reaches the texel, in linear
   light, including texels the recipe leaves dark (see Road lights).

`encode_visible` returns a zeroed payload of the layout's length when the
first visible level is opaque black (the probe answered lit but the
generator lit nothing; 1 texture in 99 in the #436 sample): ISPC's BC1
kernels encode an opaque black block as eight zero bytes, which the repair
leaves, and a test pins the full path's zeros against the shortcut from
4 to 2048 px, square and not (#439). Otherwise it
averages the lower levels from the first visible
level in linear light (see Mips in linear light), dims the fade levels
(through one 256-entry table per gain, the full levels only floored),
floors every level (see BC1 floor), BC1-encodes each level through
`dds::compress_level` with `IspcCompressor` named, whatever
`texture.compressor` says, and repairs the blocks of each level whose
decode tints a dim texel, against that level's own texels (see BC1
endpoint repair). Each level is compressed and repaired in its own
slice of one buffer allocated at the payload's exact size from
`dds::layout` (699,064 bytes for a ZL16 texture at 1024 px, 174,776 at
512 px; `dds::compress_level_into`, #393); no DDS header is built. Until
#392 the payload was cut from a full DDS that the encoder had grown by
doubling to a 1,048,832-byte mapping at ZL16. The unwritten tail of
that mapping was never touched, so it took address space, not memory:
the payload cache, bounded by payload length, was not exceeded in
resident terms. At its exact size a generated ZL16 payload is below
glibc's 1 MiB mmap threshold (#227) and comes from the arenas, as
payloads read from the lit disk tier always did. A
lit payload is about a tenth of an ortho tile, so wiring the GPU channel
worker into the lit path is not worth it now. A real ZL16 texture at 1024 px
generated in about 130 ms warm and 147 ms cold before the LED stage, and
generates in about 153 ms warm and 168 ms cold with it (release build,
developer machine). Three things keep it there: recipes sit in a small
table of tile slots rather than a hash map, patterns resolve once per
tile and palette index, and the tiling caches the lattice corners
neighbouring texels share (with the same bits as without the cache).

Per-texel work is hoisted out of the loop (#437, #438), each change
computing the same `f32` values by the same operations, so the pinned
digests hold and no version changed:
- `FieldSampler` (`field/sampler.rs`) builds, once per texture, dense
  class and brightness cell tables over the cells the texture's samples
  can reach, and per column and per row the grid, warp and brightness
  coordinates, so a cell read no longer divides or hashes. A read outside
  a table falls back to the field; tests assert real generations never
  miss.
- Both warp noises come from one lattice lookup with a one-entry corner
  cache (`NoisePair`); `floor` calls are replaced by `night::math::floor_i64`;
  a pattern whose size is a power of two wraps its taps with a mask decided
  when it is decoded; the LED patch keeps one cache for the pattern pass;
  `BrightnessScale` takes the radiance logarithm once.
- A class mask (`CellTable::near_class`, cells within 2 of a class cell,
  which covers the warp's reach) lets a texel far from any class skip the
  class lookup, and a texel whose pattern sum is exactly black skips the
  patch draw and the tone path; road light still reaches it.

On 99 real ZL16 textures at 1024 px (the #436 sample: release, one thread),
the hoisted parts took 98.1 to 29.8 ms (class weights), 35.5 to 10.3 ms
(radiance), 134.7 to 97.7 ms (pattern sampling) and 29.5 to 7.9 ms (LED
patch) per texture, and a whole generation about 335 to 180 ms. These are
one laptop's figures (AMD Ryzen AI 9 HX 370), measured while other builds
ran, for reference only; the ignored tests `time_the_hoisted_texel_parts`
and `time_the_class_mask` repeat them.

Determinism (R13): hashes use fixed seeds and integer inputs, and the
generator holds no state between calls, so the same recipe, art and
settings give the same bytes on the same build.

### LitTextureService (`night/service.rs`)

The entry point FUSE calls. `NightLitSource` is the phase 2b service (the
phase 2a name is kept) behind the `LitTextureSource` trait. It answers
`has_light` once per footprint and remembers the answer for the session, so
`size` and `read` agree (R7). The service has two operations:

- `size(name) -> u64`: from `has_light` and the layout. It is cheap and
  never generates.
- `read(name, offset, len) -> Bytes`: serves the header, the zero region
  and the payload, generating the payload on first use.

Inside:

- **Probe.** `has_light` builds the same `RecipeField` the generator uses
  and follows its sampling model, conservatively. A texel is lit only when
  a lit class gives it weight and its bilinear brightness gives a gain
  above zero. So the probe visits every class cell whose centre is within
  the class reach of the footprint (1.5 class cells: the warp's half cell
  plus the bilinear neighbour), and for each one that resolves to lit art,
  every brightness cell that can enter the bilinear brightness of the
  texels it weights (one brightness cell further, 15 arc-seconds). It
  answers lit when one of them has a radiance above the gain floor.
  Bilinear brightness never exceeds its brightest corner, so the probe
  never answers dark where the generator lights; it may answer lit where
  the generator ends up black, which costs a full-size black file. Cells
  are indexed globally, so both reaches cross 1° tile edges. A property
  test checks the contract across block edges, both kinds of degree line
  and zooms 14 to 18. Probes are limited to 2 permits.
- **Recipe reads.** The probe and the generator read recipes through
  `SessionRecipes`. A read that fails with an I/O error is retried twice,
  50 ms apart (`RecipeStore::get_checked` does not cache a failure). Once
  the retries run out the tile is given up on for the session: it reads
  as dark for every later probe, without retries, delay or a new warning.
- **Memo.** The first committed `has_light` answer for a footprint wins
  (`entry().or_insert`), so two probes that finish differently cannot give
  `size` and `read` different lengths.
- **Coalescing.** Concurrent requests for one footprint share one load
  (`night/payloads.rs`, #432): a `DashMap` of `OnceCell` load slots keyed by
  footprint. A read whose parts are not all in memory joins the load in
  flight, or checks the caches again under the slot's entry lock and only
  then starts one; the load keeps its parts and leaves the map with no await
  between, so a later read finds either the load or what it kept. A read
  joining a load keeps the parts it wants itself, so a head read joining a
  tail read's load is still one generation. If the read running a load is
  dropped, a waiting read runs its own; the orphaned generation is bounded by
  the permits and its result is discarded. The memory caches are the lit
  path's own (R8). The key is `(row, col, zoom)` with no settings
  fingerprint, because `night.brightness` and the texture setting are
  fixed for the session. A failed generation is not cached, nor a payload
  generated while one of its road tiles was still building (see Road
  lights): every caller waiting on that generation is served it, and the
  next read generates again.
  Generation reads all the recipes of its field first; if one cannot be
  read it returns no payload (not cached) and the read serves full-size
  black, so a payload is never built with a tile missing.
- **CPU budget.** `(cores / 8).clamp(2, 4)` generation permits bound
  concurrent generations, which run on the blocking pool. They are admitted
  by priority (`night/admission.rs`, #433), not first come first served:
  reads that reach the first visible level (the nearest textures), then
  reads of the smaller levels only, then pre-generations, first come first
  served within a class. A read that joins a load in flight raises the
  load's class. On a flight on 2026-11-26 a read in a scene load waited
  0.4 to 1.1 s, mostly for a permit (p95 223 to 582 ms, up to 21 waiting),
  against about 20 ms median generation.
- **Pre-generation** (`night/pregen.rs`, #433). When the probe first commits
  a footprint lit, the footprint is queued for a background load of its
  smaller levels at the pre-generation class: nothing when they are in
  memory, a lit disk tier read when it holds a valid entry, else a
  generation with the usual write-back. It moves the work to `stat` time,
  overlapping X-Plane's DSF loading; on that flight every footprint the
  probe answered lit was read, so it is not wasted. The queue holds at most
  `MAX_QUEUED_PREGENERATIONS` (512), more dropped and counted (a dropped
  footprint is generated by its read). A pre-generated tail waits in memory
  until X-Plane reads it, so the bound is held, by a const assertion, to
  half the tail memory in ZL16 tails: a larger queue could evict its own
  tails before they were read. `permits - 1` workers (at least one) hold
  the source only weakly, so one permit is always left for reads. No worker
  takes a footprint while ortho demand is in flight (`DemandTracker`, the
  signal the prefetch lane's admission gate reads, R17); reads are never
  held. A read arriving meanwhile joins the load
  and raises it. Only a source built with `NightLitSource::shared()` does
  this (the mount manager's); `LitTextureSource::shutdown()`, called first
  in `MountManager::unmount_all`, closes the queue without waiting. Generation,
  probe, disk (2, for the lit disk tier's fingerprint, read and
  write-back) and road extract permits (2, see Road lights) are not added
  to `LaneSizing::blocking_threads_required()`. They sit inside
  `BLOCKING_POOL_HEADROOM` (32), held there by a compile-time assertion
  that probe permits plus the maximum generation permits plus disk permits
  plus road extract permits are at most a third of the headroom (10 of 10,
  so the night share is fully used). This keeps `executor` free of a
  dependency on `night`.
- **Cache.** Reads go memory, then disk, then generate. The memory cache
  holds each payload as two parts (#432), separate allocations: the first
  visible level (head) and the levels below it (tail), each bounded by
  bytes, least recently used evicted first. Their size is a share of
  `cache.memory_size` (`memory_budgets` in `service/cache_layer.rs`), taken
  out of it as the lit disk tier takes 5% of `cache.disk_size`: 15% for
  night lighting, the ortho memory tier keeping 85%, the tail the larger of
  40% of the share and 192 MiB, the head the rest. Below about 1.67 GiB,
  where 15% is under 256 MiB, night lighting keeps the floor (192 MiB tail,
  about 1,150 ZL16 tails, and 64 MiB head) on top and ortho keeps all of
  `cache.memory_size`. Both parts only grow with `cache.memory_size`. A
  fixed 256 MiB was too small: over the Bay Area on 2026-11-27 X-Plane read
  the head of 623 of 1,197 lit textures and returned to 20 to 47 distinct
  heads a minute in cruise; the 64 MiB head budget held about 150 (53% head
  hit rate), and once about 1,500 textures had been read the tail evicted
  too. That scene needed about 300 MiB of heads and 185 MB of tails, and
  dense cities need more. With an 8 GB memory cache the share is about
  1.29 GB (515 MB tail, 773 MB head). The tail is kept on
  every load; the head only when a read reaches it. X-Plane reads the small
  levels first and most textures never read the head (44 of 1,006 on
  2026-11-14), and with whole payloads in one 256 MB cache every scene load
  on 2026-11-26 evicted 340 to 1,300 payloads within a minute, sending a
  quarter to a half of cruise reads back to disk for minutes after. A read
  is composed from the parts its range overlaps by `LitLayout::read_lit`. A
  head read after a tail-only load reads the disk tier, or generates again
  for a memory-only payload; in the rare case where the payload's art or
  road sources changed in between, its head and tail come from two
  generations. LRU, not moka's default TinyLFU
  (#387): TinyLFU admits a new entry only if it is read more often than the
  one it would evict, and a payload just generated has been read once, so
  once the cache was full it was refused and the next read of the texture,
  tens of seconds later, went to disk or generated it again. The disk tier holds payloads only (file sizes and the DDS layout
  are unchanged) under `{cache_dir}/night/` in region folders, with its own
  GC daemon on the prefetch lane. With `night.enabled` it takes 5% of
  `cache.disk_size` first (`LIT_DISK_SHARE`) and `dds_disk_ratio` splits the
  remaining 95%; with night lighting off there is no tier, no directory and
  no daemon, and the split is unchanged. The key is `lit:{zoom}:{row}:{col}`
  and an entry is magic `XELL`, a 32-byte SHA-256 fingerprint, the payload
  length (u64 LE) and the payload. The fingerprint covers
  `LIT_GENERATOR_VERSION` (bump it when generated bytes change),
  `night.brightness`, the size and mtime of the Laminar lit art
  (`library.txt`, `.ter`, `_LIT.dds`) and of the recipe files the
  footprint's field reads, each with the package region it was read from
  (the LED stage depends on it), and with road lights the road digest
  (`ROADS_VERSION` and the road model) and the identities of each road
  tile's DSFs (see Road lights). The tier also holds the road extractions
  of 7z DSFs. Recipe identities are taken when each recipe is loaded, so a
  recipe replaced while XEL runs does not lend its new identity to a
  payload built from the old one. A mismatch is a miss: the payload is
  regenerated and overwrites the entry, so package updates, X-Plane
  updates and brightness changes need no cache clear. Write-back is
  fire-and-forget, at most `MAX_PENDING_WRITE_BACKS` (16) pending (more are
  dropped and counted; the payload is still served and kept in memory),
  writing the header and the generated payload as two slices
  (`Cache::set_parts`, no copy of the payload), only for payloads built from a complete set of recipes
  (black is never
  written), and never for a payload one of whose own Laminar art lookups
  met a doubt (#435): a file given up on after I/O errors, or a file the
  fingerprint walk does not cover (outside the art root, under a symlink
  below it, or named other than `library.txt`, `*.ter` or `*_LIT.dds`).
  Either could read differently next session under the same fingerprint.
  The generator looks art up through `LaminarArt::pattern_recorded`, which
  notes each doubt in the payload's `ArtRecord`; such a payload stays in
  memory, and a debug line (`Night lighting: lit texture kept off disk`)
  names the file and the reason. Art reads go through the `ArtFiles` seam
  (`read`, `is_file`), are single-flight (one `OnceLock` per key) and are
  retried like recipes (`night/retry.rs`: two more attempts, 50 ms apart).
  A missing file is not retried. A missing file, a non-UTF-8 `library.txt`
  or `.ter`, a `.ter` without lit lines and a lit texture that is not square
  DXT1 make their class dark. When the walk covers the file this is settled
  under the fingerprint, so those payloads are written. The walk runs before
  the first art read, so a file replaced while XEL runs files a payload only
  under the older identity. Until #435 one art failure of any kind stopped
  every lit disk write for the session, and an unusable file did so in every
  session. Disk work runs under `LIT_DISK_PERMITS` (2). Evicting is
  safe: a payload regenerates to the same bytes, or to full-size black once
  one of its recipes has been given up on, and the size never changes. The
  tier reports no dashboard metrics yet; its counts are in the lit serving
  summary below.

**Lit serving statistics** (`night/stats.rs`, #426). `LitStats` counts lit
serving as the road service's `Stats` does: atomic counters, latencies in
`metrics::LatencyWindow`s, and a `Night lighting: summary` line at debug
every 60 s that logs nothing for a window without activity. The summary
task, started on the first `size` or `read` inside a runtime, holds the
statistics and the payload cache only weakly, so it ends with the source.
Latency samples are moved out of their lock before they are sorted, so a
summary never holds up a read. Log only: `DiskTier::Lit` has no counters in
the metrics system and the dashboard shows nothing of it. The line has:

- **Reads** by outcome, each with `median/p95/max` latency (probe
  included): `dark` (the minimal DDS), `blank` (header or the black levels
  above the payload), `memory` (a payload already in the memory cache),
  `joined` (a read that waited on another read's load of the same payload),
  `disk`, `generated` and `black`. A memory hit is a read whose every
  wanted part was in memory; a read that waited on a load in flight is a
  join. A larger cache would have saved a memory hit, never a join.
- **Probes** by the source of the committed answer (`recipe` first, then
  `road`, then `dark`), with probe permit wait and, with road lights on,
  road wait, which only a probe its recipe left dark has (#428). A probe
  that finds another probe's answer after its permit wait records its
  permit wait but no answer.
- **Generations**: count and black generations; permit wait and generation
  time, timed apart; count and permit wait by class (`head_read`,
  `tail_read`, `pregeneration`); `generations_waiting_peak`; road state (complete,
  timed out, failed); `roads_ready_at_permit`, generations whose roads had
  timed out but were all ready once the permit was held (#427).
- **Write-backs skipped**, one reason per payload in this order:
  `roads_late` (kept nowhere), then `no_fingerprint`, `road_failed` and
  `art_failed` (each kept in memory only; `art_failed` means one of the
  payload's own art lookups met a doubt, see the lit disk tier).
- **Pre-generations**: queued, dropped at the bound, started, cancelled at
  shutdown, and by outcome (`generated`, `skipped_memory`, `skipped_disk`,
  `joined`, `black`, `uncached`: generated but not kept, such as roads
  late), with the queue peak.
- **Payload cache**, per part (`payload_head_*`, `payload_tail_*`): hits,
  joins, misses, hit rate (true hits over all lookups), size evictions (an
  `eviction_listener`), entries and bytes; `payload_entries` and
  `payload_bytes` are both parts together.
- **Lit disk tier**: hits; misses by reason (`absent`, `error`, `corrupt`,
  `fingerprint`, `length`, `no_fingerprint`; `disk::check_entry` says why
  an entry is rejected); `disk_hit_rate` over the lookups that could hit
  (no-fingerprint lookups and read errors left out); writes and write
  failures; `write_backs_pending_peak`, counted from before the write-back
  is spawned; `write_backs_dropped` at the bound; and the lookup's disk permit wait.

The per-event lines carry the same split: `lit texture generated` has
`permit_wait_ms` beside `ms`, `lit probe` has `answer` and `road_wait_ms`,
and `lit read` has `from` (the read outcome) and `us`.

### FUSE changes

One concept, a **texture kind** (`Ortho` or `Lit`), carried by the parsed
filename:

- The DDS filename pattern accepts an optional `_LIT` before `.dds`, and
  `DdsFilename` gains a `kind`. `Display` writes the suffix back, so names
  round-trip.
- Virtual inodes carry a kind bit, so a lit texture and its ortho texture
  never share an inode (R8).
- `lookup`, `getattr`, `open` and `read` route lit inodes to the
  `LitTextureSource` (phase 1: `BlackLitSource`; phase 2a: `NightLitSource`; phase 2b's `LitTextureService` replaces it). Ortho inodes take the existing path unchanged.
- A real `*_LIT.dds` file on disk in a package still wins: real files
  resolve before virtual ones, as today.
- Lit sizes are read once: the texture setting is read at start and held
  for the session, so a lit file's size never changes while the mount is
  up and the kernel cannot serve a stale one. A change to the setting
  takes effect the next time XEL starts.
- The single global DDS size (`EXPECTED_DDS_SIZE`) and the placeholder
  validator stay ortho-only. Lit sizes come from the service.

### Publisher (R1 to R3)

- `Ortho4XPProcessor` looks for `<source>/yOrtho4XP_Night/<block>/<tile>.oxn`
  for each tile it scans and records it on the tile (`TileInfo`).
- `process()` copies each recipe to `<package>/night/<block>/<tile>.oxn`.
  It checks the magic and the version digit only, since the publisher does
  not link the runtime, and skips a file that fails with a warning.
- A recipe with no matching tile is reported and not copied.
- A source without `yOrtho4XP_Night/` publishes exactly as today.
- `.ter` files are already copied byte for byte, and the dedupe module only
  reads them, so lit lines survive both (R3).
- Archiving already includes every folder in the package. On an XEL
  version since 0.3.0, the union index's root allowlist hides `night/`, but
  that version cannot serve the lit textures: it answers ENOENT for every
  `_LIT.dds` an OXO night `.ter` names. Night packages therefore must not
  reach a library a pre-0.5.0 client reads (see Decisions).

### Configuration (R20)

```ini
[night]
; Generate night lighting where packages carry night recipes.
enabled = true
; Overall lit brightness, 0.5 to 1.5. A development setting.
brightness = 1.0
```

With `enabled = false`, every lit texture is the minimal black DDS. The
`.ter` files' `LUMINANCE` is fixed by OXO at build time and is not XEL's
to change.

## LED lighting

Laminar's urban lit patterns are one sodium hue (the nine stock patterns
share a mean lit texel of about sRGB 56, 36, 18), and NASA Black Marble is
panchromatic, so neither source says where a city has gone LED white. The
LED stage in `night/generator/led.rs` adds that colour from three signals
that are available: the terrain class, the VIIRS brightness and the region
of the package.

**Patches.** The ground is cut into jittered cells about 400 m across in
Web Mercator metres (`PATCH_SIZE_M`, about 53 texels at ZL16); a texel
belongs to the nearest of the 3 × 3 neighbouring jittered cell centres. A
patch is a function of position alone, so it is independent of zoom and
texture borders, like the stochastic tiling. Each patch hashes two draws:
`u` decides whether it is LED and a jitter moves its temperature.

**Category per texel.** Industrial where the class's palette stem ends in
`_ind`; otherwise urban, blended smoothly from dim to bright over `ln(R)`
between 25 and 80 nW·cm⁻²·sr⁻¹ (a smoothstep). Mixed-class texels use the
class-weighted category.

**Share and temperature.** A texel is LED when the patch's `u` is below
share = category share × region share scale, so patches change only where
the share crosses `u`. Temperature = category temperature + region shift +
patch jitter (±300 K), clamped to 2700 to 5700 K. Provisional constants,
with no config keys, tuned in flight:

| Category | Share | Base temperature |
|---|---|---|
| Industrial | 0.90 | 4500 K |
| Urban bright | 0.90 | 4500 K |
| Urban dim | 0.80 | 4000 K |

The shares were raised from 0.80, 0.75 and 0.45 in #363: night photos of
central Mexico and Mexico City from the air showed city light mostly
neutral white (median saturation 0.08 to 0.21 over the brightest lit
pixels, almost no saturated orange), against 0.34 and a quarter saturated
orange in a 0.5.0 screenshot of Ontario, California, where most
residential ground stayed sodium.

The temperatures were raised from 4000 K (urban bright) and 3200 K (urban
dim), and the region shifts halved from ±400 K, also in #363. Once the
mips kept their light, the generated Bay Area textures (61 ZL16 textures
from San Francisco to San Jose) were nine tenths LED by texel but still
averaged to amber (linear R : G : B over luminance 1.44 : 0.92 : 0.53,
saturation 0.37 as an sRGB colour), because a distant city is mostly
residential white. With these values it averages to saturation 0.28; a
stronger sodium tone (`mix` 0.45) moved it by nothing measurable. Field
practice: US conversions before 2016 were mostly 4000 to 5000 K, after
which the AMA's 3000 K guidance turned many residential streets warmer
(Tacoma: 3000 K residential, 4000 K arterials); EU procurement guidance
asks for warm white in residential areas, but most UK councils install
4000 K or more. The continents converge more than they differ, so the
shift is ±200 K, and how much of a region has converted (Belgium and
Germany least in Europe, per the ISS colour maps of Sánchez de Miguel et
al., 2022) is left to the share scale.

| Region | Share scale | Temperature shift |
|---|---|---|
| NA | 1.0 | +200 K |
| EU | 0.9 | -200 K |
| OC | 0.9 | -200 K |
| AS | 0.8 | -200 K |
| SA | 0.7 | +200 K |
| AF | 0.5 | +200 K |
| Patch or unknown | 0.75 | 0 |

**Recolour.** An LED texel keeps the luminance of the pattern sum (decoded
with the 2.2 gamma, Rec.709 weights) times `LedModel::energy` (1.25: about
half compensates the extra brightness orange appears to have over white of
equal luminance, the rest is a net gain), and takes the hue of a blackbody
white at the patch temperature, from a table every 100 K. If a channel
would exceed full scale, all three scale down together so the hue holds; if
that leaves the texel dimmer than its sodium input, it is mixed towards
white until it is not. LED is therefore never dimmer than the sodium it
replaces, and this holds after rounding to bytes: the generator compares
the decoded luminance of the LED bytes with that of the sodium bytes for
the same gain, and rounds the LED channels up, then falls back to the
sodium bytes, when rounding to nearest would lose any. `LedModel::OFF`
reproduces the sodium-only output.

**Sodium tone.** The texels that stay sodium are toned too (#363,
`SodiumTone`, `LedModel::sodium`). Laminar's sodium is linear
R : G : B ≈ 1 : 0.38 : 0.08, about as saturated as a 2700 K blackbody
(1 : 0.42 : 0.10), and high-pressure sodium itself sits near 2000 K, so a
low-temperature white would not desaturate it: the hue is physically fair,
and the change is perceptual (scatter, and the eye adapting to a mostly
white city). Each sodium texel is mixed `mix` (0.30) of the way towards the
3200 K white at its own luminance in linear light, then its luminance is
multiplied by `1 + mix × (energy - 1)` with `energy` 1.125, about half the
LED energy, because a less saturated light of equal luminance looks dimmer.
Laminar's mean texel goes from saturation 0.68 to 0.63; sodium districts
stay amber and never read as LED. The byte rule is the same as for LED: the
toned bytes are never dimmer than Laminar's bytes at the same gain, and LED
bytes are never dimmer than the toned ones. `SodiumTone::NONE` (in
`LedModel::OFF`) returns Laminar's bytes exactly.

**The tone path** (#436). `led::Toner` prepares what the tone derives from
the request alone once (`PreparedTone`: the clamped mix, the white of its
temperature and the gain), and the generator decodes each texel's pattern
sum to linear light once (`Decoded`), for both the sodium tone and the LED
recolour. The byte rule's luminance of bytes reads the decode of every
byte from `led::linear_table()` (shared with road light and the lit chain),
and each of its steps hands the luminance it settles on to the next as its
floor. The same `f32` values come from the same operations, so no byte
changed and `LIT_GENERATOR_VERSION` stayed. `powf` per toned texel went
from 12 to 30 down to 6 to 9: what is left is the decode of a non-integer
colour and the encode before a per-texel gain, which no 256-entry table
replaces exactly. On 99 real ZL16 textures at 1024 px (the 5 x 5 grids
around Mexico City, Geneva, Paris and Montreal, `LedModel::DEFAULT`, no
roads, warm, release, one thread) the tone path took 71.5 ms per texture
before and 43.7 ms after, and generation 284 ms and 256 ms (10% less);
these are one laptop's figures (AMD Ryzen AI 9 HX 370), for reference
only. The ignored test `time_the_tone_path_on_real_zl16_textures` repeats
the measurement, with the installation's paths from `XEL_NIGHT_PACKAGES`
and `XEL_LAMINAR_ART` (read by tests only).

**Regions.** The region is the first segment of the code of the package a
recipe was read from (`NA-USA-MX-CENTRAL` is NA; legacy `na` and `eu`
work), recorded when the recipe is loaded, as the identity is. Patches
(`_patches/...`), unknown codes and missing recipes are Unknown.

**Cache.** The region is part of each tile's entry in the footprint
fingerprint, and `LIT_GENERATOR_VERSION` is 10 (2 added the LED stage, 3
the raised shares and the sodium tone, 4 the linear-light mips, 5 the
cooler whites, 6 road lights, 7 the two-level fade, 8 a quarter of the
light at its nearest level, 9 the BC1 floor, 10 the BC1 endpoint repair),
so lit entries from before
the current generator miss once and regenerate; the GC ages the old ones out.

**Cost.** Measured with the ignored test
`night::generator::cpu::tests::time_a_real_zl16_texture` (release, a real
ZL16 1024 px texture, California night package): warm 130 and 132 ms before,
152 and 155 ms with the LED stage; cold 146 and 149 ms before, 168 ms both
runs with it. About 22 ms (17%) more per texture, inside the existing
generation permits.

**Deferred.** An OXO brightness-trend LED model computed once and encoded
per tile at tile creation, which would replace the formula here; OSM
commercial flags for the category; SDGSAT-1 colour data.

## Road lights

Close to the aircraft X-Plane's 3D roads and their lights take over,
over the lit layer at a quarter of its light, and alone on the nearest
textures, whose top level X-Plane uploads black. The lit layer's streets are
Laminar's pattern art, not real roads, so without road lights a 3D road
appears where the lit texture showed no road to match, and a road across
dark countryside, where VIIRS sees nothing and the gain is zero, appears
out of nothing. Road lights draw the roads the simulator actually presents
into the lit texture: a chain of lamps along every lit road class, on top
of the pattern in towns and faint between them, so the hand-off from far
to near matches. The code is `xearthlayer/src/night/roads/`.

| File | Responsibility |
|---|---|
| `roads/packs.rs` | `PackIndex`: which DSFs X-Plane loads for each 1° tile, in priority order |
| `roads/dsf/` | Streaming DSF reader: `stream.rs` (atoms, 7z), `pool.rs` (PO32 point pools), `walk.rs` (the command walk) |
| `roads/exclusion.rs` | `sim/exclude_net` zones |
| `roads/class.rs` | `RoadClass` from a DSF road subtype |
| `roads/merge.rs` | `Merger` and `MergedRoads`: the priority merge and the compact merged tile |
| `roads/codec.rs` | The disk cache entry of one DSF's extraction |
| `roads/model.rs` | `RoadModel`: glow per class, rural floor, colour, lamps |
| `roads/field.rs` | `RoadField`: a texture's glow raster, and `reach_deg` |
| `roads/service/` | `RoadService`: per-tile builds (overlays only), waits, caches, identities; `real.rs` holds the ignored real-install measurements |
| `roads/stats.rs` | Counters, latency windows and the minute summary |

### Source of truth

The roads are those of the DSFs X-Plane loads for a tile, read at run time
from the user's installation, not a layer built at publishing time:
whatever overlays the user has (X-World Pro, airport packs, other road
packs) decide the network, as they do in the simulator.

`PackIndex::build` reads `Custom Scenery/scenery_packs.ini` (with the
parser X-World Pro detection uses), keeps the enabled entries in file
order, appends Global Scenery (`X-Plane 12 Global Scenery`) last, and
lists `<pack>/Earth nav data/*/*.dsf` of each by name. `*GLOBAL_AIRPORTS*`
resolves to `Global Scenery/Global Airports`. Only directories are read:
no DSF is opened or stat'ed. XEL's own mount is left out, so XEL never
reads its own DSFs back through its own filesystem: a pack is skipped when
its resolved root equals a mount point, or when its ini entry is relative
and its folder name equals the mount point's (which covers a mount point
spelled with another prefix, a symlink or a case difference). The index is
built once per session, in its own blocking task, on first use; building
the lit source reads nothing from the installation. The index lives for
the session, so it stores no path per DSF: one root per pack and one
sorted array of six-byte entries (tile, rank, and whether the file is
spelled as X-Plane names it, `Earth nav data/+40+000/+46+006.dsf`), so a
tile's sources are one binary-searched run, highest priority first. A
standard path is rebuilt from its pack's root when the tile is built; any
other spelling the listing accepts (an upper-case `.DSF`, a tile filed
under another block's folder) keeps its relative path in a small side
table, so it is still found where it is. The listing parses each directory
entry's name straight into an entry, in parallel across packs. On the
developer's installation (315 pack lines, 301 enabled, X-World Pro on
every continent) it lists 181,290 DSFs over 39,017 tiles in 16 to 33 ms
and holds them in 1.1 MiB (`PackIndex::bytes`).

**Which DSFs give roads.** X-Plane shows the networks of every overlay DSF
of a tile (`sim/overlay 1`) but of one base mesh only, the highest
priority one. Wherever XEL serves lit textures that base mesh is XEL's own
(left out of the index, and it carries no roads), so a lower base mesh's
roads, Global Scenery's among them, are never on screen. The lit layer
follows the same convention: it shows exactly the overlay roads X-Plane
draws, and a tile no overlay gives a road has no road glow.

The sources are taken from the highest priority down; each overlay's
network and `sim/exclude_net` zones are merged (see Merge), so a zone in a
higher pack removes the roads of the packs below it, as X-Plane does,
until a zone covers the whole tile. A base mesh, wherever it ranks, is
read only to its HEAD (`ReadMode::OverlaysOnly`: 169 bytes of Global
Scenery's Geneva 7z) and passed over; no path reads one further. Its
extraction is never stored, so it costs a disk cache lookup that misses
and that HEAD read. Entries the version 1 road cache stored for base
meshes (Global Scenery's Geneva entry is 3.7 MB) stay on disk until the
tier's GC evicts them, but are never loaded: the entry key hashes
`ROAD_EXTRACT_VERSION` (now 4) with the path, so an earlier version's
keys are never looked up.

A source whose header names another tile than the one it is filed as (a
misnamed file) is skipped with a debug line, rather than merged at the
wrong offsets. On the developer's installation, Geneva without its
simHeaven packs has six airport road segments from Global Airports, and
without Global Airports too it has none.

### Streaming DSF reader (`roads/dsf/`)

`read_dsf_file` reads a DSF forward only, raw or as the single entry of a
7z, and keeps only what roads need:

- **HEAD** properties: `sim/west`, `sim/south`, `sim/overlay` and every
  `sim/exclude_net`.
- **DEFN**'s **NETW**: whether its first string, the network definition
  (`.net`), is present and not empty. A DSF without one has no network. The
  name itself is not kept: nothing uses it, so neither `DsfRoads` nor the
  disk entry stores it.
- **GEOD**'s 32-bit pools (**PO32**, each with its **SC32**): longitude and
  latitude planes, and the junction plane kept as one bit per point
  (junction ids are only ever tested for zero). Elevation and curve planes,
  the 16-bit pools of a mesh and their scales are read past.
- **CMDS**: walked once. Network chains (commands 9, 10 and 11, with the
  junction offset of command 2 and the subtype of command 6) become road
  segments by DSFLib's rule: a junction ends the open segment, unless it is
  the chain's first point, and begins the next, unless it is the last; any
  other point is a shape point. Every other command is sized, so it is
  skipped byte-exactly. Points of segments whose subtype is not lit are
  never computed.

Everything else (rasters, other atoms) is read past without being
buffered. Reading stops as soon as the roads are known: after HEAD for a
base mesh read for overlays only, after HEAD and DEFN when there is no
network, after GEOD when a network is declared but the pools hold no
32-bit point (mesh DSFs do this), and once GEOD and CMDS are read
otherwise. The result is a `DsfRoads`: tile, overlay flag, network
definition, zones and the kept segments as flat arrays (`Chains`:
tile-relative `f32` longitude and latitude, start offsets, a subtype byte).

A 7z is read only when it is X-Plane's form, one entry in one block; any
other archive is refused from its header, before a decoder is built. The
entry is streamed into the same reader, and the entry iteration stops with
it, so decompression stops where reading does. `sevenz-rust2` is built
without its `compress` feature in the runtime, so it decodes only.

Bounds, all checked before anything is allocated for them, so a hostile or
damaged file cannot make the reader use unbounded memory or time:

| Bound | Value |
|---|---|
| A DSF (decompressed size for a 7z) | 512 MiB (`MAX_DSF_BYTES`) |
| Any atom body the reader keeps (HEAD, DEFN, a PO32, an SC32, CMDS) | 256 MiB (`MAX_RETAINED_ATOM_BYTES`), read in 16 MiB steps as the bytes arrive |
| Points across all PO32 pools | 2^25, about 33.5 million (`MAX_POOL_POINTS`, about four times the largest real file seen, X-World Pro's Tokyo) |
| Chain indices the walk visits | 4 per pool point plus 65,536, and never more than 2^25 (`MAX_WALK_VISITS`) |
| An SC32 | 255 planes |

Run-length planes can claim millions of points in a few bytes, which is why
the point and visit caps are absolute. A junction plane with both a scale
and an offset is refused (the tile has no road lights), because whether a
scaled id is zero cannot be decided before its SC32 arrives; no real file
seen uses one.

Measured on the developer's installation (release build): X-World Pro's
Geneva roads (`+46+006`, 32 MB raw, 412,443 lit segments kept) in 22 ms,
its Tokyo roads (`+35+139`, 108 MB raw, 1,927,833 kept) in 70 ms, and
Global Scenery's Geneva mesh read in full (11.6 MB 7z, 17.2 MB read after
decompression, 109,384 kept) in 310 ms, which the service never does (a
base mesh is read to its HEAD). DSFs without roads (objects, forests,
facades) cost a few kilobytes of reading each.

### Road classes (`roads/class.rs`)

A DSF segment's subtype is the `.net` file's `ROAD_TYPE` divided by 100.
Laminar's `roads.net` and `roads_EU.net` and X-World Pro's nets share these
ranges:

| Subtypes | Class | Glows | Half width | Sigma | Intensity |
|---|---|---|---|---|---|
| 100 to 119 | Motorway | yes | 8 m | 10 m | 1.00 |
| 120 to 129 | Ramp | yes | 4 m | 8 m | 0.90 |
| 10 to 29 | Primary | yes | 6 m | 9 m | 0.95 |
| 30 to 49 | Secondary | yes | 5 m | 8 m | 0.425 |
| 50 to 69 | Local | no | 3 m | 7 m | 0.60 |
| 70 to 73, 80 to 82 | Minor | no | 2 m | 6 m | 0.45 |
| 75 | Lit footway | no | 1.5 m | 5 m | 0.30 |

Only motorways, ramps, primary and secondary roads glow
(`RoadClass::is_lit`); the reader drops the other classes with the unlit
subtypes. In the second flight (KCCR, 2026-11-14) a lit pixel was as bright
on both sides of the hand-off, but 24 to 54% of the screen was lit above it
against 3 to 11% below: X-Plane's residential streets read as scattered
building light, which the Laminar pattern already carries, and only
motorways and arterials as lines of lamps. Over 20 ZL16 textures around
KCCR, glowing every class lit 19.1% of the texels and the main classes
alone 4.7% (the pattern alone 1.2%). The parameters of the dark classes
are kept, should residential areas need road light again.

X-World Pro adds 70 to 73 (single-lane roads), 74 (`#Pedestrian`), 75
(`#Footway, cycleway streetlights`), 80 (local entrance) and 82
(residential street) through its own nets. Subtypes known to carry no road
light are dropped: 0 (Laminar's unnamed type), 74, 151 to 154 (rail) and
220 (power lines). Any other subtype is drawn unlit and logged once per
session (`Night roads: unknown road subtype, drawn unlit`); resolving it
through the pack's `.net` is deferred (the reader would have to keep the
network definition name, which it does not).

### Merge (`roads/merge.rs`)

`Merger` takes a tile's overlay DSFs from the highest priority down (see
Which DSFs give roads). A segment
of a DSF is dropped when any of its points lies in a zone of a
higher-priority DSF (tested on its exact points), then the DSF's own zones
are added for the DSFs below; a DSF's zones never apply to its own roads.
A zone is a west/south/east/north box or, in X-Plane 12, that box followed
by a polygon (`;lon,lat,...`, even-odd test). Unlit subtypes are dropped by
the reader (its `keep` filter) before the merge sees them, and the merge
skips any chain of fewer than two points, which has no segment to draw.
When a rectangle zone covers the whole tile, no lower DSF can add a road,
so the service stops reading there (`skipped_below_cover`): X-World Pro's
road packs carry `sim/exclude_net` over their whole tile, so with them
installed Global Scenery's mesh is never opened for the tile.

Two counts are easily confused. A *segment* in a DSF, and in
`merged_segments`, `segments_kept` and `removed`, is a chain from junction
to junction; the merged tile stores it as one or more pieces (below), and
`merged_segments` counts the pieces. The raster's `road_segments` counts
point-to-point segments, the straight pieces between two consecutive
points, of every chain the merged tiles hand over near the texture,
including chains just outside its reach that a cell visit hands over too.

**Compact merged tiles.** The merged tile is what the memory cache keeps,
so it is stored compactly. Points are 16-bit offsets in the tile (1/65535
degree, about 1.7 m north to south, far below a lit texel). Segments are
stored back to back with one byte each: the class in the top three bits
and the point count less one in the low five, so a segment of more than
32 points is stored as pieces that share the joint point (1% of Geneva's
segments, 0.1% of Tokyo's). The tile is a 256 × 256 grid of cells of
1/256 degree:

- In a dense tile the segments are sorted by the cell of their box's
  south-west corner, and a prefix index gives each cell's first segment
  and first point (two `u32` per cell, 512 KiB). A segment whose box spans
  more than one cell (`MAX_SPAN`; 1% of Geneva's segments, 0.1% of
  Tokyo's) carries its 4-byte box in a short list instead, and the tile
  records the largest span of the others. A query visits the cells from
  that span south and west of its box to its north-east corner, row by
  row, then tests the boxes of the short list.
- Where the index would cost more than a box for every segment (fewer
  than 131,075 segments within a cell's span), the tile has no index and
  every segment carries its box.

`Merger` collects segments in arrival order with their boxes, and `finish`
lays the tile out by cell with a counting sort into exactly sized arrays
(about 30 ms for Tokyo, 3 ms for Geneva), a transient peak of two to three
times the tile while the exact copies are allocated. A one-bit-per-cell
occupancy map (8 KiB per tile) answers `MergedRoads::crosses` in time
proportional to the query's cells, not to the tile's segments: two cell
boxes meet exactly when they share a cell. Consumers visit the segments
near a box through `MergedRoads::for_each_chain_near`, which hands over a
`ChainRef` (the class, the points in degrees, and the quantised offsets),
never the encoding; it may hand over a segment just outside the box, never
miss one inside. `MergedRoads::bytes`, the weight in the tile cache, is
exact.

| Tile | Segments stored | Index | Merged |
|---|---|---|---|
| Geneva `+46+006`, X-World Pro | 42,920 | no | 1.1 MiB |
| Tokyo `+35+139`, X-World Pro and the Haneda overlay | 281,233 | yes | 4.0 MiB |
| 3 × 3 tiles around Geneva | | | 6.7 MiB |
| 3 × 3 tiles around Tokyo | | | 9.9 MiB |

Before local streets, minor roads and footways were left dark, Geneva
stored 418,937 segments in 8.8 MiB and Tokyo 3,197,492 in 37.2 MiB.

The `Night roads: tile ready` field `merged_segments` counts the stored
segments (pieces included).

### Glow model (`roads/field.rs`, `roads/model.rs`)

`RoadField::raster` turns the merged roads of the tiles a texture's field
reads into one value from 0 to 1 per texel of the first visible level, at
the texel centres `CpuLitGenerator` samples. Each segment between two
points contributes its class profile: the class intensity within the half
width of the line, then a Gaussian fall-off with the class sigma, cut at
half width plus 3 sigma. The fall-off `exp(-u^2 / 2)`, with `u` the
distance beyond the half width in sigmas, is computed as `(1 - u^2 / 2^11)`
squared ten times, in `f32`: within 2.7e-4 of the Gaussian (a test holds it
under 3e-4, far inside a 1024th of the class intensity) and exactly 1 at
0, with no branch and no table. Glow and lamps share this one kernel, so
world anchoring is untouched. Distances are in ground metres (Web Mercator
metres times cos(latitude) on each row), so the glow is anchored to the
world and the same ground gives the same light at any zoom. The sigma is
`max(class sigma, a texel's ground size)` on each row, not once per
texture, so a ZL16 texture and its ZL17 quarter agree exactly. Overlapping
roads take the **maximum**, not the sum, so a junction or a dual
carriageway is no brighter than its brightest road.

Cost is kept down at every level: a merged tile hands over only the
segments filed in cells near the footprint and its reach (no test per
segment of the tile), a point's Web Mercator northing is read from a table
built once per tile per raster over the footprint's quantised latitudes
and a margin of two cells (the value the projection gives, so the table
changes no result; a point beyond it is computed), and a segment
visits only the texels in its reach window, whose bounds come from the
even texel spacing (a guess and a step or two, not a binary search). The
texel loop has no branch: every texel of the window computes its value,
zero beyond the reach by a select, and keeps the larger, so the compiler
vectorises it. What varies by row (the sigma and its inverse, the reach)
is computed once per row without a division, and lit texels are counted
once after drawing. The profile (one `f32` per texel, 4 MiB at 1024 px)
is allocated when the first segment reaches the grid, so a texture with no
road nearby costs no heap.

`reach_deg(name, size, model)` is how far outside a footprint a road can
still light one of its texels, in degrees: the widest class reach (half
width plus 3 sigma, with the sigma never below the texture's largest
texel) over the fewest metres per degree (longitude at the footprint's
highest latitude, floored at cos 85°). The raster culls with it and the
probe tests with it, so the two cannot disagree.

**Composite.** The generator adds the glow last, only on texels it reaches,
in linear light: the pattern's bytes below are decoded (through a 256-entry
table), the road light is added, and the sum is encoded to bytes by
`ByteEncoder`, which gives exactly the byte the sRGB encode would round to
(checked for every float from 0 to 1) with no `powf`: 255 thresholds found
once by bisection, and a table on the float's top 16 bits saying where to
start counting them. A texel with no class skips the gain, which only the
pattern needs. An empty field leaves every byte as it was, and a field
rastered for another size is ignored rather than misplaced. The texel
centres the raster draws at and the generator samples come from one
function, `TexelCentres::of` (`night/footprint.rs`). The level is

```
peak × glow × (country + (1 − country) × clamp(R / full_nw, 0, 1)) × night.brightness
```

with `peak` 0.0106, `country` 0.2 and `full_nw` 10 nW·cm⁻²·sr⁻¹. `peak` was
0.12 until the first flight, at KCCR, where the glow was 98.6% of the lit
light (72 times the pattern's area light) and the lit side of the near
hand-off was 10 to 20 times brighter in area average than X-Plane's 3D
roads and lights; the second flight flew three stops down (0.015), since
auto exposure makes up part of that gap. After the third flight a lit
pixel was as bright on both sides of the hand-off and the lit side still
read a little bright, so `peak` went down half a stop more and secondary
roads, the most numerous glowing class, to half their intensity (0.85 to
0.425): together 0.6 stops less area light over 20 ZL16 textures around
KCCR, where halving the core widths gave only 0.14, since at 7.6 m
texels the one-texel sigma floor sets a road's width. Roads do
not use the gain curve: the gain is zero below its floor radiance, so dark
countryside would stay dark. Instead the VIIRS radiance `R` raises the road
from a rural floor (a fifth of the light) to full light at 10. The colour
follows the pattern's: where the texel's LED patch is LED (the same
`patch_at` decision, with the urban share for its brightness and region),
the patch's white at luminance 1; elsewhere the road sodium (stored
255, 178, 92) toned by the same sodium tone as the pattern, at luminance 1.
The region is looked up once per 1° tile, the patch the pattern's LED
stage drew is reused (otherwise it is found once per patch cell as a row is
walked), and the white of the last temperature is kept.

**Lamps.** With `RoadModel::lamps` set, as in `RoadModel::DEFAULT`, every
glowing road is drawn as a chain of lamps instead of the continuous glow:
one lamp every `spacing_m` of ground arc length along a segment, each a
point glow (no core) with its class's sigma and intensity, the lamp
`sigma_m` a floor (`max(lamp sigma, class sigma)`, and a texel at least)
and the lamp `intensity` a factor on the class's. Each lamp is lit or not
by a hash of its segment's first point and its index (`lit_share`), so
lamps land on the same ground at any zoom. A segment stored as pieces
starts a walk at each joint, the same in every texture. After the fourth
flight (KCCR, 2026-11-14) the light levels were close to right but the
roads still read a little dense, and X-Plane's own 3D roads beside them
read as dotted chains of lamps (sodium dots along motorways, white along
arterials) where the texture drew continuous bands. `LampParams::DEFAULT`
lights every lamp (`lit_share` 1, a regular chain like the simulator's,
no random gaps), with a sigma floor of 4 m (below every glowing class's,
so the class sigma rules) and its spacing chosen for half the light per
metre of road. Over the 20 ZL16 textures around KCCR at 1024 px (road
light being the mean linear luminance with roads minus the pattern's
alone), the lamps give this share of the continuous glow's light:

| Spacing | 20 m | 24 m | 26 m | 28 m | 30 m | **32 m** | 36 m | 40 m |
|---|---|---|---|---|---|---|---|---|
| Road light | 0.602 | 0.565 | 0.547 | 0.530 | 0.512 | **0.495** | 0.464 | 0.438 |
| Texels above 0.002 | 3.25% | 3.14% | 3.08% | 3.03% | 2.98% | **2.93%** | 2.83% | 2.73% |

The continuous glow lights 4.08% of the texels above linear 0.002 and the
pattern alone 1.24%. The light falls slower than the spacing grows,
because at 7.5 m texels neighbouring lamps' glows overlap and take the
maximum. `lamps: None` draws the continuous glow, byte for byte as before
(a test pins its digest), kept so a flight can compare the two.

### Road service (`roads/service/`)

`RoadService` makes a tile's merged roads ready for the lit source:

- **Per-tile builds.** A build lists the tile's sources from the pack
  index, then reads them on the blocking pool as Which DSFs give roads
  says, feeding each extraction to the merger and dropping it,
  so at most one extraction is alive at a time. Every scratch buffer is
  reused, and the merged tile is laid out in exactly sized arrays (see
  Merge).
- **Permits.** `ROAD_EXTRACT_PERMITS` (2) bound concurrent builds. A build
  holds one extract permit while it reads its tile and no other lit permit,
  and releases it before its disk entries are written (#429), so a slow
  write never holds the next build. A successful build hands its entries
  to one detached write task: readiness never waits for a write. At most
  `ROAD_WRITE_BACKLOG` (2) tiles' writes wait in memory; past that a tile's
  entries are dropped (counted as `road_writes_dropped`, logged at debug)
  and re-extracted next session. Each entry's write takes one of the lit
  disk permits and releases it before the next entry, so a lit disk lookup
  on X-Plane's read path waits for at most one entry's write, never a whole
  tile's.
- **Coalescing.** A build runs in its own task, kept in a map of shared
  futures by tile, so concurrent callers share it and a caller that stops
  waiting never cancels it. A tile with an unreadable source is marked
  failed and has no roads for the session; later callers are answered at
  once.
- **Tile cache.** Ready tiles are kept in memory, weighted by
  `MergedRoads::bytes`, up to `ROAD_TILE_CACHE_BYTES` (128 MiB), evicting
  least recently used. LRU, not moka's default TinyLFU: admission by
  popularity would refuse a tile just built once the cache is full, and
  every request for it would rebuild it. An eviction is logged
  (`Night roads: LRU`) and costs a rebuild on the next request.
- **Disk cache.** Only extractions of 7z overlays are written to the lit
  disk tier (a base mesh, read only to its HEAD, writes none):
  decompression costs far more than reading the entry back (302 ms
  against 3.5 ms, measured on Global Scenery's Geneva mesh when version 1
  still read it; XEL's own overlay packages carry Global Scenery's overlay
  part, roads included, as 7z).
  Raw DSFs are parsed again each session, because their parse (16 to 90 ms
  measured) is no slower than reading an entry of the same size, and
  caching them would duplicate tens of megabytes per city tile on disk and
  in transient memory. The key is `roads:{lat}:{lon}:{hex16}` (the tile,
  so the entry lands in the tile's region folder and the tier's startup
  scan and GC see it, and the first 8 bytes of the SHA-256 of
  `ROAD_EXTRACT_VERSION` and the DSF path, so an earlier version's entries
  are never looked up). An entry is the lit tier's format (magic `XELL`, a fingerprint,
  the length, the payload); the fingerprint covers `ROAD_EXTRACT_VERSION`
  (bump it when what an extraction keeps changes) and the DSF's path, size
  and mtime, so an updated pack or a new reader misses and re-extracts.
  The payload is the `DsfRoads` in `codec.rs`'s little-endian layout,
  decoded with every count checked against the bytes present before it
  allocates. Entries are written only after the whole tile succeeded.
  Version 1 also stored the base meshes a fallback read (Geneva's Global
  Scenery entry was 3.7 MB, 109,384 segments); version 2 stores overlays
  only, and its keys never reach a version 1 entry. Version 4 drops the
  network definition name from the payload (codec version 2), and its keys
  never reach an entry of an earlier version.
  `ROAD_EXTRACT_VERSION` guards road entries only; lit payloads have their
  own versions (see Lit source integration).
- **Identities.** `identities(tile)` gives each source's path, size and
  mtime (one `stat` each, on the blocking pool, under the caller's permit),
  highest priority first, taken once per tile per session and shared by
  concurrent first callers. A DSF that is gone (NotFound) is left out; any
  other `stat` failure fails the pass, which answers `None` for that call
  and is not kept, so the lit source keeps that payload in memory only
  rather than fingerprint it as a tile without the DSF.
- **What a build read (#430).** Each build records the identity of every
  DSF it read (`ReadFrom`), taken with an `fstat` on the handle it reads
  through (`read_dsf_open`), so the identity is that of the bytes read even
  if the file is replaced meanwhile. It is kept with the merged roads in the
  tile cache (`ReadyTile`) and dropped with them. A payload is written to
  disk only when the identity pass agrees with what each of its tiles' builds
  read, or with a prefix of it when a covering exclusion zone stopped the
  build early (sources below it cannot change the roads). Otherwise the DSF
  changed since its roads were read, the payload is kept in memory only
  (`WriteBackSkip::RoadSourcesChanged`), and the next session regenerates it
  under the new identity.
- **Waits.** A caller asks for a footprint's tiles with a purpose: `Probe`
  waits up to `ROAD_PROBE_WAIT` (2 s), `Generate` up to
  `ROAD_GENERATE_WAIT` (10 s). The wait is a budget per build, not per
  call: the first call that waits on a tile's build for a purpose sets
  that build's deadline, and later calls wait only until it. The budget
  runs from when the build gets its extract permit, not from the ask, so
  a tile queued behind other builds is not given up on before it starts
  (#429); a cap bounds the whole wait from the first ask, queue included:
  `ROAD_PROBE_CAP` (5 s) for probes, the generation cap equal to its wait.
  The deadline is `min(ask + cap, max(ask, permit) + wait)`
  (`RoadServiceConfig::deadline`), never earlier than before #429. A probe's
  `stat` can therefore wait up to 5 s, against 2 s before, when its tile was
  queued for a permit. X-Plane
  stats thousands of lit names while a scene loads, so a wait per call
  would stall loading for as long as the slowest tile multiplied by the
  names it covers. A new build (a tile rebuilt after the LRU evicted it)
  clears the tile's deadlines, so its first caller waits a full budget
  again. A caller past its deadline first looks whether the build has
  just finished, and uses it if so. A footprint answers with the ready
  tiles, whether it is complete, and whether a tile was still building at
  its deadline (a failed tile is incomplete but not timed out).
- **Construction** reads nothing: `RoadService::new` stores the
  installation, the packs to skip and the disk tier. Waits and the cache
  size come from `RoadServiceConfig`, whose default is the constants.

**Permit budget.** Night work runs on the blocking pool inside
`BLOCKING_POOL_HEADROOM` (32) and may use a third of it: probe permits (2)
plus the most generation permits (4) plus lit disk permits (2) plus road
extract permits (2) is 10, held there by a compile-time assertion. The lit
disk permits are one set (`LitDiskPermits`), made by the mount manager and
given to both the lit source and the road service: lit disk reads and
writes and road cache writes share them, so they count once. The
share is fully used, so a new night permit needs a smaller one elsewhere
or a larger headroom. The pack index build takes no permit (it is a
one-off listing), so a caller that holds a lit disk permit while it asks
for identities never holds two.

### Lit source integration

`NightLitSource::with_roads` attaches the service; without it every answer
and payload is exactly as without road lights.

**Probe.** `has_light` answers in this order: a committed answer for the
footprint, if any; otherwise it starts the footprint's road builds, takes
a probe permit and reads the recipe field (`recipe_lights`), and answers
lit at once when the recipe lights the footprint (#428: such a probe never
waits for roads). Only when the recipe leaves it dark does it release the
permit, wait for the road tiles (purpose `Probe`) holding none, and answer
lit when any ready road tile comes within `reach_deg`, at the size of its
first visible level, of the footprint. The decision is one pure function,
`probe_answer(recipe_lit, footprint, road_tiles, reach)`, tested by value:
the recipe's answer first, the road tiles consulted only when it is dark.
Its road test is `crosses_footprint`, `crosses` over the same `RoadWindow`
the raster culls with, a tile across the antimeridian shifted by 360
degrees to the footprint's side (#434), so the probe and the raster cannot
disagree at the line. The road tiles a footprint asks for (in the probe,
the generation and the fingerprint alike) are those of the footprint grown
by `max(FIELD_MARGIN_DEG, reach_deg)`, so a coarse zoom, a low texture
setting, a high latitude or a wider model cannot leave a lit road's tile
out. A road just outside a footprint whose glow reaches in
therefore makes it full size, so glow is never cut at a texture edge. The
occupancy map makes the road test about 20 ns per footprint on Tokyo's
3.2 million segments.

**The size never changes.** The first committed answer stands for the
session, as for recipes. If the probe's roads were late (timed out) and the
committed answer is dark, the texture stays the minimal black DDS for the
session even when the roads arrive; this is counted as
`answered_before_roads`. If the probe answered lit because of a road and
the generation then finds that tile missing, the texture is generated
without it, possibly full-size black; its size is unchanged.

**Generation.** `generate` waits for the footprint's road tiles (purpose
`Generate`) holding no permit, then takes a generation permit. If its wait
had timed out, it looks at the tiles again once it holds the permit
(`RoadService::tiles_now`, which never waits and starts nothing) and adds
the tiles that became ready while it queued (#427), so a generation that
waited out its budget for a permit is not served without roads that are
there; the write-back and the generation line use the state after this
look (`roads_ready_at_permit`). The raster runs inside the generation task,
and the tiles' handles are dropped once rastered, so the tile cache alone
keeps merged roads.

**Memory and disk.** A payload is written to the lit tier only when every
road tile it asked for was ready (and, as before, every recipe was read
and none of its art lookups met a doubt). A payload generated while a road tile was still
building past its deadline is served but kept neither in memory nor on
disk: a timeout is transient, so the next read generates again, with the
roads once they are ready (while the tile is still building past the
generation budget, each read generates). A payload missing a failed tile
is kept in memory (the tile has no roads this session) but not on disk,
because a later session may read the tile. A payload whose tiles' DSFs
changed since their roads were read is kept in memory only (#430, see What
a build read), logged once per tile.

**Fingerprints.** The session fingerprint includes the road digest
(`roads::roads_digest`: `ROADS_VERSION` and `RoadModel::digest()`; zeros
without road lights), and the footprint fingerprint includes, for each
road tile, the identities of that tile's DSFs in priority order. Order
matters because it decides what the merge keeps. A changed, added or
removed DSF, a reordered `scenery_packs.ini`, a new road model or a new
`ROADS_VERSION` therefore regenerates the payload. Each version constant
guards one thing:

| Constant | Bump when | Guards |
|---|---|---|
| `LIT_GENERATOR_VERSION` (10) | Generated bytes change for the same inputs: ramp, tiling, encoder (the BC1 floor and endpoint repair), layout of the visible levels, the road composite | Lit payloads (generated tops are memory only and need none) |
| `ROADS_VERSION` (3) | The same DSFs and road model can give different road light: the reader, the class table, the merge and source rules, quantisation, the kernel | Lit payloads |
| `ROAD_MODEL_VERSION` (2) | The meaning of a `RoadModel` parameter changes (2: lamps light every glowing class, the lamp sigma a floor on the class's and the lamp intensity a factor on it) | Lit payloads (through the model digest) |
| `ROAD_EXTRACT_VERSION` (4) | What one DSF's extraction keeps, or which DSFs are stored, changes: the reader, the subtype filter, the codec's input (2: overlays only; 3: glowing classes only; 4: no network definition name in the payload; it is hashed into the key, so an earlier version's entries are never loaded) | Road disk entries |
| `ROAD_CODEC_VERSION` (2) | The byte layout of a road entry's payload changes | Road disk entries (another layout does not decode; 2: no network definition name) |

### Trigger (FUSE)

X-Plane looks up and opens a tile's DSF before it asks about the tile's lit
textures. XEL's ortho mount serves those DSFs (every tile it serves
textures for has one), so `lookup` and `open` of a real file at
`Earth nav data/<block>/<tile>.dsf` call `LitTextureSource::dsf_seen(lat,
lon)`, and the road service starts that tile's build at once
(`ensure`, which returns immediately and does nothing for a tile ready,
building or failed). A hint is ignored, with `Night roads: hint ignored`,
while `ROAD_HINT_BACKLOG` (8) builds are pending, so a burst of hints
cannot queue ahead of the builds a probe or a generation waits on; those
are never refused. Both handlers hint because the kernel may open a DSF
whose entry it cached (from `readdirplus`) without a lookup. The lit source
says once, when it is attached, whether it wants hints
(`wants_dsf_hints`); the filesystem keeps the answer in a bool, so with
night lighting off or without a road service the handlers gain one branch
and nothing else. With hints on, a real open borrows the inode's path and
tests its name without allocating. A probe or generation for a tile no hint
reached starts the build itself (triggers `Probe` and `Generate`).

The `Night roads: lead time` event gives, for each tile, how long before
(positive) or after (negative) its first probe it was ready. A prefetch box
hint (starting builds for tiles ahead of the aircraft) is deferred until
flights show negative lead times.

### Failure rules

| Failure | Effect | Logged |
|---|---|---|
| A source DSF cannot be read: I/O error, not a DSF, malformed, over a bound, a refused 7z, or a panic while reading | The whole tile has no road lights for the session (a partial merge would keep roads a higher pack's zones remove) | Once per tile, as a warning, with the path |
| A source removed since the pack index was built | Skipped | No |
| A source whose header names another tile | Skipped | `Night roads: DSF tile mismatch`, debug |
| A source whose `stat` fails when its identities are taken | No fingerprint: the payload is kept in memory only | `Night roads: DSF identities unavailable for this call`, debug |
| A build that ends without an answer (a panic outside the reading) | The tile has no road lights for the session | Once, debug |
| Roads late at the probe | The answer commits without them and stands for the session | Counted as `answered_before_roads` |
| Roads late at generation | Generated without them, kept neither in memory nor on disk (the next read generates again) | `roads_timed_out = true` on the generation line |
| A road tile failed at generation | Generated without it, kept in memory only | `roads_complete = false`, `roads_timed_out = false` |
| More than `ROAD_HINT_BACKLOG` builds pending at a DSF hint | The hint is ignored; a probe or generation still starts the build | `Night roads: hint ignored`, debug |
| A road disk entry that does not match or does not decode | Re-extracted from the file | No |
| A road disk entry write fails | Nothing (re-extracted next session) | Debug |
| More than `ROAD_WRITE_BACKLOG` tiles' writes waiting | The tile's entries are dropped (re-extracted next session) | `Night roads: road cache writes dropped; too many waiting`, debug; counted as `road_writes_dropped` |
| A road DSF replaced after its roads were read | Payloads over that tile are served but kept in memory only; the next session regenerates | `Night lighting: lit textures over a road tile kept off disk; its DSFs changed since its roads were read`, once per tile, debug; counted as `write_back_skipped_road_sources_changed` |
| An unknown road subtype | Drawn unlit | Once per subtype, debug |
| `scenery_packs.ini` missing | Only Global Scenery is listed; its meshes are read to their HEAD, so no road light | No |

### Instrumentation

Every event is at debug level and starts with `Night roads:`, except the
warning for an unreadable DSF. Read them with `run --debug`.

| Message | Fields |
|---|---|
| `Night roads: pack index built` | `packs`, `enabled`, `dsfs`, `tiles`, `bytes`, `ms` |
| `Night roads: DSF extracted` | `path`, `file_bytes`, `read_bytes`, `compressed`, `head_only` (a base mesh read to its HEAD), `ms`, `encode_ms`, `segments_kept`, `exclusions`, `pool_points`, `walk_visits`, `cached` |
| `Night roads: DSF cache hit` | `path`, `read_ms`, `decode_ms`, `segments` |
| `Night roads: DSF tile mismatch` | `lat`, `lon`, `path`, `dsf_lat`, `dsf_lon` |
| `Night roads: tile ready` | `lat`, `lon`, `trigger`, `dsfs`, `overlays_read`, `base_skipped` (base meshes read to their HEAD), `cache_hits`, `cache_misses`, `merged_segments`, `removed`, `skipped_below_cover`, `bytes`, `total_ms` |
| `Night roads: hint ignored` | `lat`, `lon`, `pending` |
| `Night roads: footprint waited` | `purpose`, `tiles`, `waited_ms`, `timed_out`, `complete` (only when the call waited or timed out) |
| `Night roads: lead time` | `lat`, `lon`, `ms` |
| `Night roads: LRU` | `lat`, `lon`, `bytes_held` |
| `Night roads: summary` (every 60 s, only when something happened) | `tile_cache_bytes`, `cache_hits`, `cache_misses`, `cache_hit_rate`, `tiles_ready`, `tiles_failed`, `probe_waits`, `probe_timeouts`, `generate_waits`, `generate_timeouts`, `answered_before_roads`, `extracted`, `extract_median_ms`, `extract_p95_ms`, `extract_max_ms`, `ready_median_ms`, `ready_p95_ms`, `ready_max_ms`, `permit_wait_p95_ms`, `road_cpu_ms`, `road_writes_dropped` |
| `Night roads: road cache writes dropped; too many waiting` | the tile and its entry count |
| `Night roads: unknown road subtype, drawn unlit` | `subtype` |
| `Night roads: DSF unreadable; tile has no road lights this session` (warning) | `lat`, `lon`, `path`, `error` |
| `Night roads: tile build ended without an answer; no road lights this session` | `lat`, `lon` |
| `Night roads: DSF identities unavailable for this call` | `lat`, `lon`, `error` |
| `Night roads: road cache write failed` | `error`, `key` |
| `Night roads: road field size differs from the texture; no road light` | `field`, `size` |

The existing `Night lighting: lit texture generated` line gains
`road_segments` (point-to-point segments handed to the raster, see
Merge), `road_raster_us`, `road_texels`, `roads_complete` and
`roads_timed_out`, `roads_ready_at_permit` (#427), and `size`. `Night lighting: lit probe`
gains `roads_complete` and `roads_timed_out`. `Night lighting: lit read`
says in `part` whether a read reached the payload: `blank` (header or the
black levels above the payload), `lit`, or `black` when the payload it
reached was served black. `cache_misses` counts 7z overlays read from their file
with a disk tier present; a base mesh read to its HEAD is not one.

### Measured cost

Release build on the developer's machine (32 threads), over the developer's
installation, with the ignored tests
`night::roads::service::real::*` (one test per process, so memory
readings are its own) and
`night::generator::cpu::tests::time_real_roads_on_real_zl16_textures`.
Files were in the page cache.

**Tile readiness** (cold service, every installed pack):

| Tile | Sources | Read | Ready |
|---|---|---|---|
| Geneva `+46+006` | 14 | 13 overlays (Global Scenery skipped below X-World Pro's cover) | 13 to 15 ms |
| Tokyo `+35+139` | 18 | 16 overlays | 65 to 70 ms |
| Geneva without simHeaven packs | 4 | 3 overlays (6 airport road segments from Global Airports); Global Scenery to its HEAD only (169 of 11,638,782 bytes) | 0.9 ms |
| Geneva without simHeaven packs or Global Airports | 3 | 2 overlays, no road; Global Scenery to its HEAD only | 0.9 ms |

**Memory** (resident, allocator configured as `run` configures it, with
rayon's pool and a blocking-pool thread already running, as they are in
`run` long before a road tile is asked for):

| | Geneva 3 × 3 | Tokyo 3 × 3 |
|---|---|---|
| Merged roads (`MergedRoads::bytes`) | 6.7 MiB | 9.9 MiB |
| Resident growth readying them | 12.5 to 13.2 MiB | 13.8 to 14.2 MiB |
| The service's first build (the pack index and an empty tile) | 7.4 to 7.9 MiB | 7.8 to 7.9 MiB |
| Peak resident (process) | 89 MiB | 212 MiB |

Merged tiles cost about their `bytes` in resident memory and are returned
to the system when evicted (their arrays are large single allocations).
Steady-state tile memory is bounded by the 128 MiB tile cache, which a
3 × 3 block around Tokyo fills to 8%. The Tokyo peak is the reader's
transient while it streams the 103 MB X-World Pro road DSF. The pack index is outside that
bound and holds 1.1 MiB. Built alone in a process whose rayon pool is
running, it adds 4 to 5 MiB resident: the rest is the listing's transient
allocations in the pool's threads, which later allocations reuse (a
second index built beside the first adds 1.5 to 2 MiB). Of the service's
first build, nearly all stays resident after the service and its index
are dropped and the allocator is trimmed: it is the runtime's first use,
not the index.

**Generation** at 1024 px (nine interleaved rounds, medians):

| Texture | Road texels | Raster | Composite | Without roads | Raster and generation | Increase |
|---|---|---|---|---|---|---|
| Geneva `23248_33872_ZL16` (8,878 segments rastered) | 8.8% | 2 ms | 4 ms | 309 ms | 314 ms | 1.9% |
| Densest Tokyo `25792_58208_ZL16` (26,896 segments rastered) | 17.2% | 7 ms | 6 ms | 423 ms | 435 ms | 2.9% |

That is with lamps (32 m). In the same session the continuous glow
(`lamps: None`) rastered in 4 and 10 ms, lit 10.8% and 21.0% of the
texels, and cost 2.6% and 4.5%: a lamp's window is smaller than a
segment's. With every class glowing these were 9.8 to 9.9% for Geneva
and 15.8% for Tokyo.

The densest Tokyo texture is the ZL16 texture of the tile holding the most
road points. The segments rastered are those the cell index hands over,
a few more than reach the texture. The raster tests no segment box of the
tile and computes no logarithm for a point inside its northing table;
what it spends is the texel loop and each segment's window (only the
raster's total is measured).
Where no road reaches a texture the raster allocates nothing and the
generator's output is unchanged byte for byte.

### Decisions

| Decision | Choice | Why |
|---|---|---|
| Source of the roads | The DSFs the simulator loads, at run time | The lit layer has to match the 3D roads the user's own scenery presents; a publish-time layer could not know the user's overlays |
| Urban look | Glow on top of the pattern everywhere | Simple, and the pattern's street structure stays |
| Road look | Every glowing road a regular chain of lit lamps (32 m), the continuous glow kept as `lamps: None` | X-Plane's 3D roads read as dotted chains of lamps, and after the fourth flight the continuous glow read a little dense; the chain gives half its light per metre |
| Airport roads | Included as any other road | `sim/filter/aptid` segments are kept; no airport ownership model |
| Configuration | None | The road model is internal, like the LED model, and tuned by flight |
| Road sources | Overlay DSFs only; a base mesh is read to its HEAD | X-Plane shows every overlay's network but one base mesh's, XEL's own wherever lit textures are served, which has no roads; so a tile no overlay gives a road has no road glow, as in the simulator. A Global Scenery fallback for such tiles was removed after the first flight as against X-Plane's conventions |

### Deferred

- A prefetch box hint that starts builds for the tiles ahead of the
  aircraft, if flights show negative lead times.
- Resolving unknown subtypes through the pack's `.net` and `library.txt`
  (the reader does not keep the network definition name, so it would first
  have to).
- GPU generation, decided from the stage timings above.
- A seekable path for raw DSFs, so skipped atoms (DEMS) are not read.
- Honouring the airport filter comments (commands 32 to 34, for
  `sim/filter/aptid`): segments an airport filters are drawn whichever
  airport is installed.
- Curved pools: a chain's points are drawn as chords through them, with
  no bezier flattening; none of the real files measured uses curves.

**Known costs.** The road profile is `size^2` `f32`, held while a texture
generates under its generation permit: 4 MiB at a 1024 px first visible
level, and proportionally more for a coarser-zoom texture with a larger
first visible level. A dense tile's build peaks at about 200 MiB of
transient memory (Tokyo, its DSF read and the counting sort's copies),
and two builds may run at once.

## Error Handling

**X-Plane never sees an error for a `_LIT.dds` name** (R4). Every failure
degrades to black, at the narrowest scope it affects (R19):

| Failure | Black for | Logged |
|---|---|---|
| No X-Plane installation found, or `library.txt` missing or not UTF-8 | Every lit texture this session | Once |
| A palette name that does not resolve, a `.ter` without a lit texture, a missing or undecodable lit file | That class only (payloads are still written to the lit tier) | Once per name |
| A `library.txt`, `.ter` or `_LIT.dds` that cannot be read (I/O error after two retries, 50 ms apart), or an art file the lit fingerprint does not cover | That class, for the session if unreadable; payloads that use it are kept in memory only | Once per file as a warning when given up on; at debug for each payload kept off disk |
| A recipe rejected by the reading rules, or from a newer major version | That 1° tile | Once per file |
| A recipe that cannot be read (I/O error after two retries, 50 ms apart) | That tile, for the session: probes treat it as having no light, and any lit footprint whose generation needs it is served full-size black, not cached (memory or disk) | Once per tile, when it is given up on |
| No recipe for a tile | That tile | Never (normal for countryside) |
| Generation or encoding fails | That texture, not cached (memory or disk) | Every time, as a warning |
| A road DSF cannot be read | Road light for that 1° tile, for the session; recipe light is unchanged (see Road lights) | Once per tile, as a warning |
| `X-Plane.prf` unreadable | Nothing: the full setting is assumed | Once |

**Size invariant (R7).** `getattr` and `read` always agree. If generation
fails after a full size was reported, the read returns a full-size black
texture (zero blocks under the announced header), never the 152-byte file.

## Phases

**Phase 1: packaging and black serving.** Recipe reader, `RecipeStore`,
publisher, FUSE texture kind, and a `LitTextureService` that always serves
the minimal black DDS. This satisfies R1 to R8 and makes OXO night
packages publishable: X-Plane finds every lit texture the `.ter` files
name, and they draw nothing.

**Phase 2: the generator.** `LaminarArt`, `XPlanePrefs`, the layout's
blank levels, `LitGenerator` and its CPU implementation, the lit cache, and
the tuning flight that sets the gain constants and confirms `LUMINANCE 150`.
The LED stage (`led.rs`) turns neighbourhood patches LED white by class,
brightness and package region.

**Road lights.** The road network the simulator presents, read from the
DSFs X-Plane loads, drawn as chains of lamps composited last (see Road
lights).

## Testing

- **Conformance.** OXO's three golden recipes are copied into
  `xearthlayer/tests/fixtures/night/` and decoded to exactly the cells the
  recipe format's Conformance section lists. Each reading rule has a
  negative test: bad magic, version mismatch, trailing bytes, a short zlib
  stream, an index past the palette.
- **Units, with test doubles at every boundary.** Footprint arithmetic at
  tile corners and across 1° edges; `library.txt` resolution against a
  synthetic tree with aliases and region blocks; decoding a known BC1
  block; the gain curve; the blank-level count for each setting from 0 to
  4; the layout's zero-region reads at every boundary.
- **Generator properties.** The same input gives identical bytes;
  adjacent footprints match along their shared edge at the first visible
  level; the same ground gives the same light at any zoom (a ZL15 texture
  at 128 px and the ZL16 texture covering one quarter of it at 64 px
  agree to within 2 levels per channel); no brightness step appears at a
  degree line, where two tiles' recipes meet; an empty footprint gives
  the minimal black DDS; the levels X-Plane drops and the uploaded top
  are all zero at any split of the read, and a read of the uploaded top
  generates nothing. `encode_visible` floors a texel at 7 and keeps one
  at 8.
  The BC1 endpoint repair decodes the palette as bcdec does, repairs a
  dim lamp block ISPC tints purple (the purple decode is asserted first,
  so a changed encoder fails loudly), leaves a block without an
  introduced tint byte for byte, keeps a four-colour and a three-colour
  block in their mode when one step would equalise or swap their
  endpoints (with no transparent texel), decodes no worse than ISPC over
  2,000 pseudo-random dim blocks, repairs each block of a non-square level
  against its own texels (lamps at two positions, mirrored, stay where
  they are; the other blocks are untouched), ignores texels past a small
  level's edge, and through `encode_visible` and the shared level
  encoder leaves no introduced tint on a level of such lamps.
  A golden test pins the SHA-256 of `encode_visible` on a fixture of
  eight levels with both fade levels, dim lamps ISPC tints and the
  repair changes, and a dim ramp across the floor; the hash is pinned
  for x86 only, where every ISPC kernel class (SSE2 to AVX-512) was
  measured to give it, and other targets run its structural checks
  (exact length, no introduced tint, light at every level). The payload
  is allocated at exactly its length, and an empty level is an error.
  The table encode is checked against the code it replaced (#439): the
  shared decode table bit for bit, the chain encode against `led::encode`
  and rounding on special values and on floats either side of every byte
  boundary, `lit_chain` and `dim` against their former forms on every
  byte value at each fade gain, and `encode_visible` on six images.
  `led::ByteEncoder` itself is checked on every float from 0 to 1 by an
  ignored test, `the_byte_encoder_exhaustive`, run in release.
  The hoisted sampling is checked against copies of the code before #437
  (`field/lookup_reference.rs`, `tiling/sample_reference.rs`) bit for bit
  over dense sweeps, missing recipes, mixed grids and degree lines, and a
  road-lit LED generation is pinned (`the_road_lit_output_is_pinned`)
  beside the sodium and LED pins; the class mask and the black stop are
  compared with the full path (`Shortcuts::NONE`); adjacent footprints
  agree along their edge with a mixed-grid recipe; the probe and the raster
  agree across the antimeridian (`probe_answer`, `crosses_footprint`),
  and a footprint its recipe lights is answered whatever its road tiles.
  The tone path is checked against its former `powf` forms
  (`led::powf_reference`, test only, #436): the generator run with both
  through `CpuLitGenerator::generate_with`, the tone steps on 25,000
  pseudo-random colours per model at gains from the floor to 1, and
  byte luminance on every byte of each channel; the pinned LED and
  sodium digests hold unchanged.
  The encode's memory is measured by a counting allocator in its own
  test binary (`tests/lit_encode_memory.rs`, #440): a 1024 px level is
  held to the chain level and payload it must hold plus 16 KiB, and a
  copy of the former whole-chain encoder (`tests/support/`) gives the
  same bytes at more than five times the memory. The ignored timing
  harness is `tests/lit_encode_timing.rs`.
  The service keeps no payload whose encode failed, lights only the
  payload of a read spanning the uploaded top and the payload, and
  writes only the payload to disk.
- **LED.** Share: the fraction of lit texels that are LED matches the
  model, and a region moves it. Coherence: patches are contiguous, not
  per-texel. Zoom agreement: the LED decision for the same ground agrees at
  ZL15 and ZL16. OFF digest: `LedModel::OFF` reproduces the pinned
  sodium-only output byte for byte. Byte-level never-dimmer: across
  radiance down to the gain floor, brightness 0.5 to 1.5 and several
  patterns and temperatures, every changed texel is at least as bright as
  its sodium bytes. Region tags and every `DEFAULT` region row are pinned,
  and a different package region invalidates a lit disk entry while the
  same one is served from disk.
- **Road lights.** The DSF reader against a builder of synthetic DSFs
  (every point encoding, junction rule, command, 7z, each bound and each
  early exit) and, in ignored golden tests, against `DSFTool`'s decoding
  of real files; exclusion zones and the merge order; the codec's round
  trip and bounded decoding; the occupancy map against a scan of every
  box, on random queries; the cell visit against every chain, on random
  tiles and queries; the raster against an independent plain `f64`
  reference (exact distances and `exp`, every class and lamps, two zooms
  and two sizes, within 3e-4 of the class intensity); glow and lamp zoom
  invariance (lamps on every glowing class), the continuous glow's digest
  without lamps, reach and culling; an
  empty field leaving the generator's output byte for byte, with the
  LED-on golden digests pinned; the service's coalescing, deadlines,
  failure, permit and cache rules; the probe, generation, disk and
  fingerprint rules in the lit source; and the FUSE hints. Ignored tests
  time all of it on a real installation (see Measured cost).
- **FUSE integration.** `stat` and a full read agree on size; a lit
  texture and the ortho texture with the same row, column and zoom have
  different inodes and different bytes; `night/` does not appear in a
  listing.
- **Local tests against a real installation**, skipped when none is
  present: every urban and industrial terrain name in the stock
  `library.txt` resolves to one of the nine lit textures.
- **Acceptance in the simulator**: the scenarios below, plus one tuning
  flight.

## Acceptance Criteria

```gherkin
Feature: Night lighting on ortho scenery

  Scenario: Recipes are packaged and hidden
    Given an OXO job's output with night recipes
    When it is published and the package is installed
    Then the package contains night/<block>/<tile>.oxn for each tile
    And no recipe file is visible through the mount

  Scenario: A lit texture over a city
    Given a footprint over a city with a recipe
    And the texture quality setting is 4
    When X-Plane reads its _LIT.dds
    Then the file is a DXT1 DDS with a 2048 px top level
    And the top level X-Plane uploads is blank
    And the next level has non-zero content
    And stat and the read agree on size

  Scenario: Countryside and missing recipes are cheap
    Given a footprint with no lit class, or no recipe
    When X-Plane reads its _LIT.dds
    Then it receives the 152-byte minimal black DDS

  Scenario: The fade follows the texture setting
    Given the texture quality setting is 2
    When X-Plane reads a city footprint's _LIT.dds
    Then the two top levels X-Plane drops are blank

  Scenario: Deterministic and seamless
    Given two adjacent city footprints
    When each is generated twice
    Then each generation gives identical bytes
    And the two join without a seam at their first visible level

  Scenario: Missing art degrades to black
    Given the installation's urban_FAR folder is renamed
    When X-Plane reads any _LIT.dds
    Then it receives a black texture
    And XEL logs the cause once

  Scenario: LED and sodium mix
    Given a night flight over the SF Bay Area
    Then white and orange patches are mixed
    And downtown reads whiter than the suburbs
    And no LED patch reads dimmer than the sodium next to it
    And LED_ENERGY is tuned by eye

  Scenario: In the simulator
    Given a night flight over a lit city on generated textures
    Then city light appears beyond the near field without visible tiling
    And it fades in through dusk and is absent at midday
    And lowering texture quality keeps the near field dark
```

## Requirements Traceability

| Requirement | Where |
|---|---|
| R1, R3 | Publisher |
| R2 | Publisher; union index root allowlist |
| R4 | Error handling; `LitTextureService` |
| R5 | Footprint |
| R6 | Lit DDS layout (minimal black DDS) |
| R7 | Footprint `has_light`; size invariant |
| R8 | Separate lit path; texture kind; lit coalescer and cache |
| R9 | LaminarArt |
| R10 | Gain curve |
| R11, R12 | Stochastic tiling; class weighting |
| R13 | Determinism |
| R14 | Texel density decision (fixed at 1024 px, not configurable) |
| R15, R16 | Lit DDS layout; XPlanePrefs |
| R17 | `LitGenerator` trait; lit CPU budget |
| R18 | RecipeStore and LaminarArt caches |
| R19 | Error handling |
| R20 | Configuration |
| R21 | Attribution |

R14 departs from the requirements on purpose: density is fixed rather than
configurable, at 1024 px instead of 2048 px, by the project owner's
decision on video memory.

## Attribution

The brightness layer comes from NASA's VIIRS annual night lights. XEL's
user documentation credits it as **NASA Black Marble VJ146A4 annual
nighttime lights** (R21), with NASA's full citation in `docs/configuration.md`
under `[night]`; release notes that ship night lighting carry it too. The lit patterns are Laminar Research's, read
from the user's own X-Plane installation and never redistributed.

## Open Questions

- The gain constants and `LUMINANCE 150`: set on the tuning flight.
- The ZL16/ZL18 edge at SFO: in the 2026-10-17 flight the ZL16 hill texture
  `25360_10464_GO216` looked unlit at an edge where ZL18 was lit. An
  offline run of the real generator on the real package lit that texture
  (22% of texels), so the generator is ruled out. Serving or X-Plane's
  drawing remain the candidates; the next flight with `--debug` decides
  between them. One serving cause is now removed: the probe used to look
  only at the brightness cell under each lit class cell, so it could
  answer dark beside light the generator spreads bilinearly.
- Whether X-Plane accepts a 32768 px header at texture setting 0 (see the
  layout's risk).
- Whether X-Plane re-reads lit textures after a mid-session change to the
  texture setting, which decides whether a watching
  `TextureSettingSource` is worth building.
- Whether `night.brightness` survives to release or becomes a constant.
- Publishing night packages: staging library only until XEL 0.5.0 is the minimum client; whether to also bump the package spec version.

## Out of Scope

- Changes to OXO or the recipe format.
- Area lighting (the lit pattern) outside Laminar's urban and industrial
  classes from VIIRS alone. Roads are lit everywhere, faintly in the
  countryside (see Road lights).
- A GPU lit generator (the trait leaves room for one).
- Following VR's texture setting.
