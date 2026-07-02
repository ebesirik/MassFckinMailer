//! Generates the app icon (`assets/icon.png` and `assets/icon.ico`) from the
//! same design as `assets/icon.svg`: an indigo rounded square with a white
//! envelope. Rendered at 4× and downscaled for smooth edges.
//!
//! Run from the workspace root: `cargo run -p mmm-app --example gen_icon`

use image::{Rgba, RgbaImage, imageops};

const SCALE: u32 = 4;
const OUT: u32 = 256;
const INDIGO: Rgba<u8> = Rgba([0x4f, 0x46, 0xe5, 0xff]);
const WHITE: Rgba<u8> = Rgba([0xff, 0xff, 0xff, 0xff]);

fn main() {
    let s = OUT * SCALE;
    let mut img = RgbaImage::from_pixel(s, s, Rgba([0, 0, 0, 0]));
    let k = SCALE as f32;

    // Indigo rounded-square background.
    fill_rounded_rect(&mut img, 0.0, 0.0, 256.0, 256.0, 56.0, k, INDIGO);
    // White envelope body.
    fill_rounded_rect(&mut img, 48.0, 80.0, 160.0, 104.0, 16.0, k, WHITE);
    // Indigo flap "V".
    let flap = [(56.0, 98.0), (128.0, 148.0), (200.0, 98.0)];
    for pair in flap.windows(2) {
        thick_line(&mut img, pair[0], pair[1], 8.0, k, INDIGO);
    }
    for &p in &flap {
        disc(&mut img, p.0, p.1, 8.0, k, INDIGO);
    }

    let out = imageops::resize(&img, OUT, OUT, imageops::FilterType::Lanczos3);
    std::fs::create_dir_all("assets").expect("create assets dir");
    out.save("assets/icon.png").expect("write icon.png");
    out.save("assets/icon.ico").expect("write icon.ico");
    println!("wrote assets/icon.png and assets/icon.ico");
}

/// Fill a rounded rectangle given in 256-space, scaled by `k`.
#[allow(clippy::too_many_arguments)]
fn fill_rounded_rect(img: &mut RgbaImage, x: f32, y: f32, w: f32, h: f32, r: f32, k: f32, color: Rgba<u8>) {
    let (x0, y0, x1, y1) = (x * k, y * k, (x + w) * k, (y + h) * k);
    let rk = r * k;
    for py in y0 as u32..y1 as u32 {
        for px in x0 as u32..x1 as u32 {
            let fx = px as f32 + 0.5;
            let fy = py as f32 + 0.5;
            if inside_rounded(fx, fy, x0, y0, x1, y1, rk) {
                img.put_pixel(px, py, color);
            }
        }
    }
}

fn inside_rounded(fx: f32, fy: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    // Nearest point on the inner rectangle (corners inset by r).
    let cx = fx.clamp(x0 + r, x1 - r);
    let cy = fy.clamp(y0 + r, y1 - r);
    let (dx, dy) = (fx - cx, fy - cy);
    dx * dx + dy * dy <= r * r
}

fn disc(img: &mut RgbaImage, cx: f32, cy: f32, radius: f32, k: f32, color: Rgba<u8>) {
    let (cx, cy, r) = (cx * k, cy * k, radius * k);
    let r2 = r * r;
    for py in (cy - r) as u32..=(cy + r) as u32 {
        for px in (cx - r) as u32..=(cx + r) as u32 {
            let (dx, dy) = (px as f32 + 0.5 - cx, py as f32 + 0.5 - cy);
            if dx * dx + dy * dy <= r2 && px < img.width() && py < img.height() {
                img.put_pixel(px, py, color);
            }
        }
    }
}

fn thick_line(img: &mut RgbaImage, a: (f32, f32), b: (f32, f32), half: f32, k: f32, color: Rgba<u8>) {
    let steps = 800;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let x = a.0 + (b.0 - a.0) * t;
        let y = a.1 + (b.1 - a.1) * t;
        disc(img, x, y, half, k, color);
    }
}
