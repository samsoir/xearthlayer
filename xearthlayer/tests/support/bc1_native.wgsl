// A minimal GPU-native BC1 encoder (#476): prototype for comparison with
// `block_compression`'s BC1, never used by XEarthLayer itself.
//
// One 4x4 block per invocation, the block's 16 texels kept as vec3s with
// constant-bound loops the drivers unroll, so they stay in registers
// (`block_compression` keeps a 64-float array indexed in loops, a port of
// the CPU algorithm's layout). Endpoints by principal axis: the block's
// covariance, three power iterations seeded with the bounding box's
// diagonal, the texels projected on it, the extremes taken. Indices by
// the standard 4-colour assignment: each texel to the nearest of the four
// colours the decoder makes from the quantised endpoints.
//
// Two entry points: `bc1_fit` as above, and `bc1_fit_refine`, which adds
// one least-squares refit of the endpoints to those indices (as ISPC and
// `block_compression` do) and assigns again.

struct Params {
    width: u32,
    height: u32,
    _pad0: u32,
    _pad1: u32,
}

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> blocks: array<u32>;
@group(0) @binding(2) var<uniform> params: Params;

fn quantise565(c: vec3<f32>) -> u32 {
    let q = clamp(round(c * vec3<f32>(31.0, 63.0, 31.0) / 255.0), vec3<f32>(0.0), vec3<f32>(31.0, 63.0, 31.0));
    return (u32(q.r) << 11u) | (u32(q.g) << 5u) | u32(q.b);
}

fn expand565(p: u32) -> vec3<f32> {
    let r = (p >> 11u) & 31u;
    let g = (p >> 5u) & 63u;
    let b = p & 31u;
    return vec3<f32>(f32((r << 3u) | (r >> 2u)), f32((g << 2u) | (g >> 4u)), f32((b << 3u) | (b >> 2u)));
}

// The 2-bit index of each texel: the nearest of the four colours.
fn assign(px: ptr<function, array<vec3<f32>, 16>>, p0: u32, p1: u32) -> u32 {
    let c0 = expand565(p0);
    let c1 = expand565(p1);
    let c2 = (2.0 * c0 + c1) / 3.0;
    let c3 = (c0 + 2.0 * c1) / 3.0;
    var bits = 0u;
    for (var i = 0u; i < 16u; i++) {
        let p = (*px)[i];
        let d0 = dot(p - c0, p - c0);
        let d1 = dot(p - c1, p - c1);
        let d2 = dot(p - c2, p - c2);
        let d3 = dot(p - c3, p - c3);
        var best = 0u;
        var bd = d0;
        if (d1 < bd) { best = 1u; bd = d1; }
        if (d2 < bd) { best = 2u; bd = d2; }
        if (d3 < bd) { best = 3u; }
        bits |= best << (2u * i);
    }
    return bits;
}

// The endpoints that best fit `bits` in least squares, or the given ones
// when the indices do not determine them.
fn refit(px: ptr<function, array<vec3<f32>, 16>>, bits: u32, e0: ptr<function, vec3<f32>>, e1: ptr<function, vec3<f32>>) {
    var aa = 0.0;
    var ab = 0.0;
    var bb = 0.0;
    var ap = vec3<f32>(0.0);
    var bp = vec3<f32>(0.0);
    for (var i = 0u; i < 16u; i++) {
        let idx = (bits >> (2u * i)) & 3u;
        // The weight of endpoint 0 for each index: 1, 0, 2/3, 1/3.
        var w = 1.0;
        if (idx == 1u) { w = 0.0; }
        if (idx == 2u) { w = 2.0 / 3.0; }
        if (idx == 3u) { w = 1.0 / 3.0; }
        let v = 1.0 - w;
        aa += w * w;
        ab += w * v;
        bb += v * v;
        ap += w * (*px)[i];
        bp += v * (*px)[i];
    }
    let det = aa * bb - ab * ab;
    if (abs(det) < 1e-6) {
        return;
    }
    *e0 = clamp((ap * bb - bp * ab) / det, vec3<f32>(0.0), vec3<f32>(255.0));
    *e1 = clamp((bp * aa - ap * ab) / det, vec3<f32>(0.0), vec3<f32>(255.0));
}

fn encode(gid: vec3<u32>, refine: bool) {
    let bw = (params.width + 3u) / 4u;
    let bh = (params.height + 3u) / 4u;
    if (gid.x >= bw || gid.y >= bh) {
        return;
    }

    var px: array<vec3<f32>, 16>;
    var mean = vec3<f32>(0.0);
    var lo = vec3<f32>(255.0);
    var hi = vec3<f32>(0.0);
    for (var i = 0u; i < 16u; i++) {
        let c = textureLoad(source, vec2<u32>(gid.x * 4u + (i & 3u), gid.y * 4u + (i >> 2u)), 0).rgb * 255.0;
        px[i] = c;
        mean += c;
        lo = min(lo, c);
        hi = max(hi, c);
    }
    mean /= 16.0;

    // Covariance: xx, xy, xz, yy, yz, zz.
    var cov0 = 0.0;
    var cov1 = 0.0;
    var cov2 = 0.0;
    var cov3 = 0.0;
    var cov4 = 0.0;
    var cov5 = 0.0;
    for (var i = 0u; i < 16u; i++) {
        let d = px[i] - mean;
        cov0 += d.x * d.x;
        cov1 += d.x * d.y;
        cov2 += d.x * d.z;
        cov3 += d.y * d.y;
        cov4 += d.y * d.z;
        cov5 += d.z * d.z;
    }

    var e0 = mean;
    var e1 = mean;
    var axis = hi - lo;
    for (var k = 0; k < 3; k++) {
        axis = vec3<f32>(
            cov0 * axis.x + cov1 * axis.y + cov2 * axis.z,
            cov1 * axis.x + cov3 * axis.y + cov4 * axis.z,
            cov2 * axis.x + cov4 * axis.y + cov5 * axis.z,
        );
        let m = max(max(abs(axis.x), abs(axis.y)), abs(axis.z));
        if (m > 0.0) {
            axis /= m;
        }
    }
    if (dot(axis, axis) > 1e-12) {
        axis = normalize(axis);
        var tmin = 1e9;
        var tmax = -1e9;
        for (var i = 0u; i < 16u; i++) {
            let t = dot(px[i] - mean, axis);
            tmin = min(tmin, t);
            tmax = max(tmax, t);
        }
        e0 = clamp(mean + axis * tmax, vec3<f32>(0.0), vec3<f32>(255.0));
        e1 = clamp(mean + axis * tmin, vec3<f32>(0.0), vec3<f32>(255.0));
    }

    var p0 = quantise565(e0);
    var p1 = quantise565(e1);
    if (p0 < p1) {
        let t = p0;
        p0 = p1;
        p1 = t;
    }
    var bits = 0u;
    if (p0 != p1) {
        bits = assign(&px, p0, p1);
        if (refine) {
            var r0 = expand565(p0);
            var r1 = expand565(p1);
            refit(&px, bits, &r0, &r1);
            var q0 = quantise565(r0);
            var q1 = quantise565(r1);
            if (q0 < q1) {
                let t = q0;
                q0 = q1;
                q1 = t;
            }
            if (q0 != q1) {
                p0 = q0;
                p1 = q1;
                bits = assign(&px, p0, p1);
            }
        }
    }
    // p0 == p1: the 3-colour mode, every index 0 decodes to the colour.

    let at = (gid.y * bw + gid.x) * 2u;
    blocks[at] = p0 | (p1 << 16u);
    blocks[at + 1u] = bits;
}

@compute
@workgroup_size(8, 8)
fn bc1_fit(@builtin(global_invocation_id) gid: vec3<u32>) {
    encode(gid, false);
}

@compute
@workgroup_size(8, 8)
fn bc1_fit_refine(@builtin(global_invocation_id) gid: vec3<u32>) {
    encode(gid, true);
}
