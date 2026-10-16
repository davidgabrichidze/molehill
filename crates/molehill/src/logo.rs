//! The Molehill mark (layered hill arches, a tunnel, two eyes and a coral
//! nose), rasterized in code so the tray icon can follow the VPN state and
//! build.rs can produce the .ico without extra tooling. Same geometry as
//! `docs/brand/svg/logos/molehill-mark-tokyo-blue.svg` (64×64 grid).

/// Colour of the middle arch: blue idle, mint connected, amber connecting,
/// slate offline (the brand's icon variants).
pub const ACCENT: [u8; 3] = [0x7a, 0xa2, 0xf7];
pub const GREEN: [u8; 3] = [0x9e, 0xce, 0x6a];
pub const YELLOW: [u8; 3] = [0xe0, 0xaf, 0x68];
pub const MUTED: [u8; 3] = [0x41, 0x48, 0x68];

const NIGHT: [u8; 3] = [0x1a, 0x1b, 0x26];
const WHITE: [u8; 3] = [0xf4, 0xf4, 0xf8];
const SLATE: [u8; 3] = [0x41, 0x48, 0x68];
const SIDES: [u8; 3] = [0xc0, 0xca, 0xf5];
const NOSE: [u8; 3] = [0xff, 0x9e, 0x8a];
const SHINE: [u8; 3] = [0xff, 0xf4, 0xef];
/// Rounded-square tile behind the mark in the app icon.
const TILE: [u8; 3] = NIGHT;

const BASE: f32 = 56.0;

fn arch(x: f32, y: f32, rx: f32, ry: f32) -> bool {
    y <= BASE && ((x - 32.0) / rx).powi(2) + ((y - BASE) / ry).powi(2) <= 1.0
}

/// The little mounds either side: `M2 56 Q8 51 14 56 L14 58 L2 58 Z` and its mirror.
fn side(x: f32, y: f32) -> bool {
    let bump = |x0: f32| {
        let t = (x - x0) / 12.0;
        (0.0..=1.0).contains(&t) && y <= 58.0 && y >= BASE - 10.0 * t * (1.0 - t)
    };
    bump(2.0) || bump(50.0)
}

/// The mark at (x, y) on the 64 grid, topmost layer first.
fn mark(x: f32, y: f32, tint: [u8; 3]) -> Option<[u8; 3]> {
    let (nx, ny) = (x - 43.5, y - 49.8);
    let (c, s) = (25f32.to_radians().cos(), 25f32.to_radians().sin());
    let (rx, ry) = (nx * c - ny * s, nx * s + ny * c);
    if (x - 44.8).powi(2) + (y - 48.7).powi(2) <= 0.7f32.powi(2) {
        return Some(SHINE);
    }
    if (rx / 3.6).powi(2) + (ry / 2.8).powi(2) <= 1.0 {
        return Some(NOSE);
    }
    let eye = |cx: f32| (x - cx).powi(2) + (y - 49.0).powi(2) <= 2.15f32.powi(2);
    if eye(30.0) || eye(37.0) {
        return Some(WHITE);
    }
    if side(x, y) {
        return Some(SIDES);
    }
    if arch(x, y, 7.0, 9.0) {
        return Some(NIGHT);
    }
    if arch(x, y, 11.0, 13.0) {
        return Some(SLATE);
    }
    if arch(x, y, 18.0, 18.0) {
        return Some(tint);
    }
    if arch(x, y, 25.0, 25.0) {
        return Some(WHITE);
    }
    None
}

fn rasterize(size: u32, f: impl Fn(f32, f32) -> Option<[u8; 3]>) -> Vec<u8> {
    const SS: u32 = 4;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let v = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    if let Some(c) = f(u, v) {
                        r += c[0] as u32;
                        g += c[1] as u32;
                        b += c[2] as u32;
                        a += 1;
                    }
                }
            }
            match (r.checked_div(a), g.checked_div(a), b.checked_div(a)) {
                (Some(r), Some(g), Some(b)) => out.extend_from_slice(&[r as u8, g as u8, b as u8, (a * 255 / (SS * SS)) as u8]),
                _ => out.extend_from_slice(&[0, 0, 0, 0]),
            }
        }
    }
    out
}

/// The bare mark, cropped to its drawing (x 2…62, y 30…58) and centred,
/// for the tray and the in-app header.
pub fn render(size: u32, tint: [u8; 3]) -> Vec<u8> {
    // 60 grid units wide; vertically centre the 28-unit-high drawing.
    rasterize(size, |u, v| mark(2.0 + u * 60.0, 44.0 - 30.0 + v * 60.0, tint))
}

/// The application icon: the mark on a rounded night tile, like
/// `molehill-app-icon-blue.svg`, with the mark larger at small sizes so it
/// stays readable in the taskbar.
pub fn render_app_icon(size: u32) -> Vec<u8> {
    let small = size <= 32;
    let (inset, corner) = (if small { 0.0 } else { 20.0 / 1024.0 }, if small { 0.18 } else { 220.0 / 1024.0 });
    // The mark spans this fraction of the tile's width.
    let span = if small { 0.92 } else { 0.62 };
    rasterize(size, |u, v| {
        let (lo, hi) = (inset, 1.0 - inset);
        if u < lo || u > hi || v < lo || v > hi {
            return None;
        }
        let r = corner;
        let (cx, cy) = (u.clamp(lo + r, hi - r), v.clamp(lo + r, hi - r));
        if (u - cx).powi(2) + (v - cy).powi(2) > r * r {
            return None;
        }
        let gx = 2.0 + (u - (1.0 - span) / 2.0) / span * 60.0;
        let gy = 44.0 + (v - 0.5) / span * 60.0;
        Some(mark(gx, gy, ACCENT).unwrap_or(TILE))
    })
}
