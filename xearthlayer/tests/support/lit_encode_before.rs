//! The lit encode before #439 and #440, for the integration tests that
//! measure what it cost against `encode_visible`: a copy of the library's
//! `encode_visible_reference`, which is `#[cfg(test)]` inside the library
//! and so out of their reach. The tests that use it assert its payload is
//! `encode_visible`'s byte for byte, which keeps the copy honest.
//!
//! Its memory is the point: the whole chain at once, with a copy of the
//! first level, and a linear copy of the first level with four channels
//! (#440).

use image::RgbaImage;
use xearthlayer::dds::{compress_level_into, layout, DdsFormat, IspcCompressor};
use xearthlayer::night::generator::bc1_repair::repair_level;
use xearthlayer::night::generator::{BC1_FLOOR, FADE_RAMP, SRGB_GAMMA};

/// `encode_visible` before #439, the full path with no shortcut.
pub fn encode_visible_before(image: &RgbaImage) -> Vec<u8> {
    let mut chain = lit_chain(image);
    for (n, level) in chain.iter_mut().enumerate() {
        dim(level, FADE_RAMP.get(n).copied().unwrap_or(1.0));
    }
    let level_len = |l: &RgbaImage| layout::level_len(l.width(), l.height(), DdsFormat::BC1);
    let mut payload = vec![0; chain.iter().map(level_len).sum()];
    let mut at = 0;
    for level in &chain {
        let blocks = &mut payload[at..at + level_len(level)];
        compress_level_into(&IspcCompressor, level, DdsFormat::BC1, blocks).unwrap();
        repair_level(blocks, level);
        at += blocks.len();
    }
    payload
}

/// `lit_chain` before #439: every level, averaged in linear light.
fn lit_chain(image: &RgbaImage) -> Vec<RgbaImage> {
    let decode: [f32; 256] = std::array::from_fn(|v| (v as f32 / 255.0).powf(SRGB_GAMMA));
    let encode = |c: f32| (c.clamp(0.0, 1.0).powf(1.0 / SRGB_GAMMA) * 255.0).round() as u8;
    let (mut w, mut h) = image.dimensions();
    let mut linear: Vec<[f32; 4]> = image
        .pixels()
        .map(|p| {
            let [r, g, b, a] = p.0;
            let d = |v: u8| decode[usize::from(v)];
            [d(r), d(g), d(b), f32::from(a) / 255.0]
        })
        .collect();
    let mut chain = vec![image.clone()];
    while w > 1 && h > 1 {
        let (nw, nh) = (w / 2, h / 2);
        let at = |x: u32, y: u32| linear[(y * w + x) as usize];
        let next: Vec<[f32; 4]> = (0..nh)
            .flat_map(|y| (0..nw).map(move |x| (x * 2, y * 2)))
            .map(|(x, y)| {
                let q = [at(x, y), at(x + 1, y), at(x, y + 1), at(x + 1, y + 1)];
                std::array::from_fn(|c| (q[0][c] + q[1][c] + q[2][c] + q[3][c]) / 4.0)
            })
            .collect();
        (linear, w, h) = (next, nw, nh);
        chain.push(RgbaImage::from_fn(w, h, |x, y| {
            let [r, g, b, a] = linear[(y * w + x) as usize];
            image::Rgba([encode(r), encode(g), encode(b), (a * 255.0).round() as u8])
        }));
    }
    chain
}

/// `dim` before #439: a multiply and a `round` per channel, then the floor.
fn dim(level: &mut RgbaImage, gain: f32) {
    for px in level.pixels_mut() {
        let rgb = &mut px.0[..3];
        for c in rgb.iter_mut() {
            *c = (f32::from(*c) * gain).round() as u8;
        }
        if rgb.iter().all(|&c| c < BC1_FLOOR) {
            rgb.fill(0);
        }
    }
}
