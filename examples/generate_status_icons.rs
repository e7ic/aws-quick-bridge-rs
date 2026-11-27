//! Regenerate the desktop status icons with `cargo run --example generate_status_icons`.
use anyhow::{Context, Result};
use image::ExtendedColorType;
use image::codecs::ico::{IcoEncoder, IcoFrame};
use resvg::tiny_skia::{ColorU8, Pixmap, Transform};
use resvg::usvg;
use std::{fs, path::Path};

const TRAY_SIZES: &[u32] = &[16, 20, 24, 28, 32, 40, 48, 56, 64];

fn render(tree: &usvg::Tree, width: u32, height: u32, ink: u8) -> Result<Vec<u8>> {
    let mut canvas = Pixmap::new(width, height).context("invalid icon dimensions")?;
    let source = tree.size();
    let scale = (width as f32 / source.width()).min(height as f32 / source.height());
    let x = (width as f32 - scale * source.width()) / 2.0;
    let y = (height as f32 - scale * source.height()) / 2.0;
    let transform = Transform::from_row(scale, 0.0, 0.0, scale, x, y);
    resvg::render(tree, transform, &mut canvas.as_mut());
    for pixel in canvas.pixels_mut() {
        *pixel = ColorU8::from_rgba(ink, ink, ink, pixel.alpha()).premultiply();
    }
    Ok(canvas.encode_png()?)
}

fn status_icons() -> Result<[(&'static str, Vec<u8>); 3]> {
    let tree = usvg::Tree::from_data(
        include_bytes!("../assets/menu-bar-icon.svg"),
        &Default::default(),
    )?;
    let tray = |ink| -> Result<Vec<u8>> {
        let mut frames = Vec::new();
        for &size in TRAY_SIZES {
            frames.push(IcoFrame::with_encoded(
                render(&tree, size, size, ink)?,
                size,
                size,
                ExtendedColorType::Rgba8,
            )?);
        }
        let mut ico = Vec::new();
        IcoEncoder::new(&mut ico).encode_images(&frames)?;
        Ok(ico)
    };
    Ok([
        ("menu-bar-icon.png", render(&tree, 36, 32, 0)?),
        ("tray-on-light.ico", tray(0)?),
        ("tray-on-dark.ico", tray(255)?),
    ])
}

fn main() -> Result<()> {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    for (name, bytes) in status_icons()? {
        fs::write(assets.join(name), bytes)?;
    }
    Ok(())
}
