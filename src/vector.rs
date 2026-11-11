//! Bitmap -> SVG tracing with VTracer (https://lib.rs/crates/vtracer).
//!
//! The activity bar shows every icon as a traced vector: installed apps'
//! icons are traced at runtime (see `browser::app_icon`), and the Claude and
//! theme icons are traced ahead of time by `cargo run --example trace_icons`
//! into `assets/icons/`.

use std::path::Path;

/// The Claude mark, traced from an orange raster.
pub const CLAUDE_SVG: &[u8] = include_bytes!("../assets/icons/claude.svg");
/// Color bars (the theme view), traced from a raster test pattern.
pub const COLOR_BARS_SVG: &[u8] = include_bytes!("../assets/icons/color-bars.svg");

/// Trace a full-color app icon PNG into an SVG file.
pub fn trace_file(png: &Path, svg: &Path) -> Result<(), String> {
    let config = vtracer::Config {
        color_precision: 7,
        layer_difference: 12,
        filter_speckle: 3,
        ..vtracer::Config::default()
    };
    let rgba = image::open(png).map_err(|e| e.to_string())?.into_rgba8();
    let (width, height) = (rgba.width() as usize, rgba.height() as usize);
    let mut pixels = rgba.into_raw();
    // VTracer only keys out fully transparent pixels, so a soft drop shadow
    // would trace as an opaque fringe: make every pixel in or out.
    for px in pixels.chunks_exact_mut(4) {
        px[3] = if px[3] >= 160 { 255 } else { 0 };
    }
    let traced = vtracer::convert(vtracer::ColorImage { pixels, width, height }, config)?;
    std::fs::write(svg, sized(&traced.to_string())).map_err(|e| e.to_string())
}

/// VTracer writes pixel `width`/`height` and no viewBox; give it a viewBox so
/// it scales, and a small intrinsic size so GPUI rasterizes it at icon size.
pub fn sized(svg: &str) -> String {
    let Some(start) = svg.find("<svg ") else { return svg.to_string() };
    let end = start + svg[start..].find('>').unwrap_or(0);
    let attr = |name: &str| {
        let tag = &svg[start..end];
        let i = tag.find(&format!(" {name}=\""))? + name.len() + 3;
        tag[i..].split('"').next()?.parse::<f32>().ok()
    };
    let (Some(w), Some(h)) = (attr("width"), attr("height")) else { return svg.to_string() };
    format!(
        r#"{}<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="32" height="{}">{}"#,
        &svg[..start],
        32. * h / w,
        &svg[end + 1..]
    )
}
