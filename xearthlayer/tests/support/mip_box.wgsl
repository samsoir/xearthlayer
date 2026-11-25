// GPU mip generation prototype (#476, for #424 and #467): one level from
// the one above by a 2x2 box filter, in integer arithmetic on the RGBA
// bytes, never used by XEarthLayer itself.
//
// Each level is a storage buffer of packed RGBA8 texels (one u32 each,
// R in the low byte, as the CPU's `RgbaImage` bytes read little-endian),
// row-major, `width` texels a row. An odd last column or row of the source
// has no partner and is dropped, as `downsample_box_2x` drops it.
//
// `mip_stored` averages the stored bytes, rounding half to even (#394):
// byte for byte `MipmapGenerator::downsample_box_2x`.
//
// `mip_linear` averages R, G and B in 16-bit linear light (#467, after
// #458): each byte through a 256-entry decode table, the four summed and
// rounded half to even, then a 65,536-entry encode table back to a byte.
// Alpha keeps the stored average.

struct Params {
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
}

@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<storage, read_write> dst: array<u32>;
@group(0) @binding(2) var<uniform> params: Params;
// Linear light only: sRGB byte -> 16-bit linear, and 16-bit linear -> byte
// packed four to a word.
@group(0) @binding(3) var<storage, read> decode_lut: array<u32>;
@group(0) @binding(4) var<storage, read> encode_lut: array<u32>;

// The average of four values from their sum, rounded half to even: the
// CPU's `quarter_half_to_even`, for any sum.
fn average4(sum: u32) -> u32 {
    return (sum + 1u + ((sum >> 2u) & 1u)) >> 2u;
}

fn channel(texel: u32, c: u32) -> u32 {
    return (texel >> (8u * c)) & 255u;
}

fn to_byte(linear: u32) -> u32 {
    return (encode_lut[linear >> 2u] >> (8u * (linear & 3u))) & 255u;
}

@compute
@workgroup_size(8, 8)
fn mip_stored(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.dst_width || gid.y >= params.dst_height) {
        return;
    }
    let top = 2u * gid.y * params.src_width + 2u * gid.x;
    let bottom = top + params.src_width;
    let a = src[top];
    let b = src[top + 1u];
    let c = src[bottom];
    let d = src[bottom + 1u];
    var out = 0u;
    for (var ch = 0u; ch < 4u; ch++) {
        let sum = channel(a, ch) + channel(b, ch) + channel(c, ch) + channel(d, ch);
        out |= average4(sum) << (8u * ch);
    }
    dst[gid.y * params.dst_width + gid.x] = out;
}

@compute
@workgroup_size(8, 8)
fn mip_linear(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.dst_width || gid.y >= params.dst_height) {
        return;
    }
    let top = 2u * gid.y * params.src_width + 2u * gid.x;
    let bottom = top + params.src_width;
    let a = src[top];
    let b = src[top + 1u];
    let c = src[bottom];
    let d = src[bottom + 1u];
    var out = 0u;
    for (var ch = 0u; ch < 3u; ch++) {
        let sum = decode_lut[channel(a, ch)] + decode_lut[channel(b, ch)]
            + decode_lut[channel(c, ch)] + decode_lut[channel(d, ch)];
        out |= to_byte(average4(sum)) << (8u * ch);
    }
    let alpha = channel(a, 3u) + channel(b, 3u) + channel(c, 3u) + channel(d, 3u);
    out |= average4(alpha) << 24u;
    dst[gid.y * params.dst_width + gid.x] = out;
}
