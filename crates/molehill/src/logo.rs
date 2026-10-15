//! The Molehill mark (a mound with a tunnel and two eyes peeking out),
//! rasterized in code so the tray icon can change color with the VPN state
//! and build.rs can produce the .ico without extra tooling. Mirrors
//! `ui/mole.svg` on a 64×64 grid.

pub const ACCENT: [u8; 3] = [0x7a, 0xa2, 0xf7];
pub const GREEN: [u8; 3] = [0x9e, 0xce, 0x6a];
pub const YELLOW: [u8; 3] = [0xe0, 0xaf, 0x68];
pub const MUTED: [u8; 3] = [0x73, 0x7a, 0xa2];
const HOLE: [u8; 3] = [0x16, 0x16, 0x1e];
const EYES: [u8; 3] = [0xc0, 0xca, 0xf5];

fn in_half_ellipse(x: f32, y: f32, cx: f32, base: f32, rx: f32, ry: f32) -> bool {
    y <= base && ((x - cx) / rx).powi(2) + ((y - base) / ry).powi(2) <= 1.0
}

fn sample(x: f32, y: f32, mound: [u8; 3]) -> Option<[u8; 3]> {
    let eye = |cx: f32| (x - cx).powi(2) + (y - 46.0).powi(2) <= 2.6f32.powi(2);
    if eye(27.5) || eye(36.5) {
        return Some(EYES);
    }
    if in_half_ellipse(x, y, 32.0, 52.0, 12.0, 13.0) {
        return Some(HOLE);
    }
    if in_half_ellipse(x, y, 32.0, 52.0, 28.0, 26.0) || ((2.0..=62.0).contains(&x) && (51.0..=54.0).contains(&y)) {
        return Some(mound);
    }
    None
}

/// RGBA pixels, `size`×`size`, 4×4 supersampled.
pub fn render(size: u32, mound: [u8; 3]) -> Vec<u8> {
    const SS: u32 = 4;
    let scale = 64.0 / size as f32;
    // Trim the empty top of the 64 grid so small icons use their pixels.
    let (offset_y, zoom) = (14.0, 64.0 / 44.0);
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) * scale;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) * scale;
                    let (x, y) = (32.0 + (x - 32.0) / zoom, offset_y + y / zoom);
                    if let Some(c) = sample(x, y, mound) {
                        r += c[0] as u32;
                        g += c[1] as u32;
                        b += c[2] as u32;
                        a += 1;
                    }
                }
            }
            if a == 0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                out.extend_from_slice(&[(r / a) as u8, (g / a) as u8, (b / a) as u8, (a * 255 / (SS * SS)) as u8]);
            }
        }
    }
    out
}
