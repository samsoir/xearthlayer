// The GPU worker's mip chain (#424): each level built from the one above by
// a 2x2 box filter, in integer arithmetic on the RGBA bytes, written where
// the BC1 kernel reads it.
//
// `src` is the level above (an `Rgba8Unorm` texture: `textureLoad` gives
// each byte over 255, exactly, and rounding it times 255 gives the byte
// back). `dst` is this level's texture, padded to whole 4 x 4 blocks; the
// level itself is `params.width` x `params.height` texels. An odd last
// column or row of the source has no partner and is dropped, as
// `MipmapGenerator::downsample_box_2x` drops it. A padding texel is the
// level's nearest edge texel, as `pad_to_block_multiple` pads on the CPU:
// its block average is that edge texel's. The byte is written as itself
// over 255, which the store converts back to the same byte.
//
// `mip_stored` averages the stored bytes, rounding half to even (#394): the
// CPU chain's filter before #467. Not used by any chain.
//
// `mip_linear` averages R, G and B in 16-bit linear light (#467): each byte
// through a 256-entry decode table, the four summed and rounded half to
// even, then a 65,536-entry encode table (four bytes to a word) back to a
// byte. Alpha keeps the stored average. Byte for byte `downsample_box_2x`,
// the CPU chain's filter, with the CPU's own tables.

struct Params {
    width: u32,
    height: u32,
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(2) var<uniform> params: Params;
@group(0) @binding(3) var<storage, read> decode_lut: array<u32>;
@group(0) @binding(4) var<storage, read> encode_lut: array<u32>;

// The four source bytes of each channel of the block under `texel`, a
// texel of `dst`, padding included.
struct Block {
    a: vec4<u32>,
    b: vec4<u32>,
    c: vec4<u32>,
    d: vec4<u32>,
}

fn byte_of(x: u32, y: u32) -> vec4<u32> {
    return vec4<u32>(round(textureLoad(src, vec2<u32>(x, y), 0) * 255.0));
}

fn block(texel: vec2<u32>) -> Block {
    let x = 2u * min(texel.x, params.width - 1u);
    let y = 2u * min(texel.y, params.height - 1u);
    return Block(byte_of(x, y), byte_of(x + 1u, y), byte_of(x, y + 1u), byte_of(x + 1u, y + 1u));
}

// The average of four values from their sum, rounded half to even: the
// CPU's `quarter_half_to_even`, for any sum.
fn average4(sum: vec4<u32>) -> vec4<u32> {
    let one = vec4<u32>(1u);
    let two = vec4<u32>(2u);
    return (sum + one + ((sum >> two) & one)) >> two;
}

fn store(texel: vec2<u32>, bytes: vec4<u32>) {
    textureStore(dst, texel, vec4<f32>(bytes) / 255.0);
}

fn to_byte(linear: u32) -> u32 {
    return (encode_lut[linear >> 2u] >> (8u * (linear & 3u))) & 255u;
}

@compute
@workgroup_size(8, 8)
fn mip_stored(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = textureDimensions(dst);
    if (gid.x >= size.x || gid.y >= size.y) {
        return;
    }
    let q = block(gid.xy);
    store(gid.xy, average4(q.a + q.b + q.c + q.d));
}

@compute
@workgroup_size(8, 8)
fn mip_linear(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = textureDimensions(dst);
    if (gid.x >= size.x || gid.y >= size.y) {
        return;
    }
    let q = block(gid.xy);
    var out = average4(q.a + q.b + q.c + q.d);
    for (var ch = 0u; ch < 3u; ch++) {
        let sum = decode_lut[q.a[ch]] + decode_lut[q.b[ch]] + decode_lut[q.c[ch]]
            + decode_lut[q.d[ch]];
        out[ch] = to_byte(average4(vec4<u32>(sum)).x);
    }
    store(gid.xy, out);
}
