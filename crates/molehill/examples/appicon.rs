//! Writes the app icon as a macOS iconset (PNG files), for `iconutil`:
//!
//!     cargo run -p molehill --example appicon -- Molehill.iconset
//!     iconutil -c icns Molehill.iconset

#[path = "../src/logo.rs"]
#[allow(dead_code)]
mod logo;

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "Molehill.iconset".into()));
    std::fs::create_dir_all(&dir).expect("creating the iconset folder");
    for base in [16u32, 32, 128, 256, 512] {
        for scale in [1u32, 2] {
            let size = base * scale;
            let suffix = if scale == 2 { "@2x" } else { "" };
            let path = dir.join(format!("icon_{base}x{base}{suffix}.png"));
            let rgba = logo::render_app_icon(size, logo::ACCENT);
            let file = std::io::BufWriter::new(std::fs::File::create(&path).expect("creating a PNG"));
            let mut encoder = png::Encoder::new(file, size, size);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().and_then(|mut w| w.write_image_data(&rgba)).expect("writing a PNG");
        }
    }
    println!("{}", dir.display());
}
