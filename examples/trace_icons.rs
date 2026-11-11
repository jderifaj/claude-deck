//! Rasterize the Claude mark and a color-bars test pattern, then trace both
//! into SVGs with VTracer. Run after changing either drawing:
//!
//!     cargo run --example trace_icons
//!
//! Writes assets/icons/claude.svg and assets/icons/color-bars.svg, which the
//! app embeds (src/vector.rs).

#[path = "../src/vector.rs"]
#[allow(dead_code)]
mod vector;

use vtracer::{ColorImage, Config, Hierarchical};

/// Raster size; the drawings use a 24-unit grid like the line icons.
const N: usize = 384;
const UNIT: f32 = N as f32 / 24.;

/// Claude's orange.
const ORANGE: [u8; 3] = [0xD9, 0x77, 0x57];

/// The Claude mark's twelve rays, in 24-unit coordinates.
const RAYS: [[f32; 4]; 12] = [
    [12.3, 9.8, 13.2, 2.6],
    [13.3, 10.2, 16.5, 6.0],
    [14.0, 11.1, 20.7, 8.3],
    [14.2, 12.3, 19.4, 12.9],
    [13.8, 13.3, 19.6, 17.7],
    [12.9, 14.0, 14.9, 18.9],
    [11.7, 14.2, 10.8, 21.4],
    [10.7, 13.8, 7.5, 18.0],
    [10.0, 12.9, 3.3, 15.7],
    [9.8, 11.7, 4.6, 11.1],
    [10.2, 10.7, 4.4, 6.3],
    [11.1, 10.0, 9.1, 5.1],
];

fn canvas(paint: impl Fn(f32, f32) -> Option<[u8; 3]>) -> ColorImage {
    let mut image = ColorImage::new_w_h(N, N);
    for y in 0..N {
        for x in 0..N {
            // Sample at the pixel centre, in 24-unit coordinates.
            let (u, v) = ((x as f32 + 0.5) / UNIT, (y as f32 + 0.5) / UNIT);
            if let Some([r, g, b]) = paint(u, v) {
                let i = (y * N + x) * 4;
                image.pixels[i..i + 4].copy_from_slice(&[r, g, b, 255]);
            }
        }
    }
    image
}

fn near_segment(px: f32, py: f32, [ax, ay, bx, by]: [f32; 4], half_width: f32) -> bool {
    let (dx, dy) = (bx - ax, by - ay);
    let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0., 1.);
    (px - ax - t * dx).hypot(py - ay - t * dy) <= half_width
}

fn claude(u: f32, v: f32) -> Option<[u8; 3]> {
    // Rays taper from 2.4 wide at the hub to 1.6 at the tip, around a solid hub.
    let hub = (u - 12.).hypot(v - 12.) <= 2.6;
    (hub || RAYS.iter()
        .any(|&ray| {
            let [ax, ay, bx, by] = ray;
            let len = (bx - ax).hypot(by - ay);
            let t = (((u - ax) * (bx - ax) + (v - ay) * (by - ay)) / (len * len)).clamp(0., 1.);
            near_segment(u, v, ray, 1.2 - 0.4 * t)
        }))
    .then_some(ORANGE)
}

fn color_bars(u: f32, v: f32) -> Option<[u8; 3]> {
    // A rounded 21x16 card of SMPTE-style bars.
    let (x0, y0, x1, y1, r) = (1.5, 4., 22.5, 20., 2.);
    let (qx, qy) = ((u - (x0 + x1) / 2.).abs() - (x1 - x0) / 2. + r, (v - (y0 + y1) / 2.).abs() - (y1 - y0) / 2. + r);
    if qx.max(0.).hypot(qy.max(0.)) + qx.max(qy).min(0.) - r > 0. {
        return None;
    }
    const TOP: [[u8; 3]; 7] = [
        [0xC0, 0xC0, 0xC0], // white
        [0xC0, 0xC0, 0x00], // yellow
        [0x00, 0xC0, 0xC0], // cyan
        [0x00, 0xC0, 0x00], // green
        [0xC0, 0x00, 0xC0], // magenta
        [0xC0, 0x00, 0x00], // red
        [0x00, 0x00, 0xC0], // blue
    ];
    const BLACK: [u8; 3] = [0x13, 0x13, 0x13];
    const MIDDLE: [[u8; 3]; 7] = [TOP[6], BLACK, TOP[4], BLACK, TOP[2], BLACK, TOP[0]];
    const BOTTOM: [[u8; 3]; 4] = [[0x00, 0x21, 0x4C], [0xFF, 0xFF, 0xFF], [0x32, 0x00, 0x6A], BLACK];
    let fx = ((u - x0) / (x1 - x0)).clamp(0., 0.999);
    let fy = (v - y0) / (y1 - y0);
    Some(if fy < 0.66 {
        TOP[(fx * 7.) as usize]
    } else if fy < 0.76 {
        MIDDLE[(fx * 7.) as usize]
    } else if fx < 5. / 7. {
        BOTTOM[(fx * 7. * 4. / 5.) as usize]
    } else {
        BLACK
    })
}

fn trace(image: ColorImage, config: Config, out: &str) {
    let svg = vtracer::convert(image, config).expect("trace");
    std::fs::write(out, vector::sized(&svg.to_string())).expect("write");
    println!("wrote {out}");
}

fn main() {
    std::fs::create_dir_all("assets/icons").expect("mkdir");
    trace(canvas(claude), Config { filter_speckle: 8, ..Config::default() }, "assets/icons/claude.svg");
    trace(
        canvas(color_bars),
        Config { hierarchical: Hierarchical::Cutout, color_precision: 8, layer_difference: 1, filter_speckle: 8, ..Config::default() },
        "assets/icons/color-bars.svg",
    );
}
