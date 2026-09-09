//! Wallpaper rendering + immersive colour extraction (wallpaper feature).
//!
//! The two built-in wallpapers are drawn *procedurally* (simple gradients with a
//! soft accent glow — no asset files, keeping the binary lean and on-brand
//! "lightweight / simple"). A custom wallpaper is decoded from any PNG/JPEG the
//! user picks. Both paths yield:
//!   • an RGBA pixel buffer wrapped as a Slint `Image`, and
//!   • a derived [`Palette`] (accent colour + light/dark + average tint),
//! so the whole UI can recolour itself to match the image ("immersive" mode).

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Render size for the built-in wallpapers. `image-fit: cover` in the UI scales
/// this up to the window, so a fixed, generous size stays crisp without any
/// re-render on resize.
const W: u32 = 1600;
const H: u32 = 1000;

/// Cap the long edge of a decoded custom wallpaper so a huge photo doesn't pin
/// a whole 6000px texture in GPU memory.
const MAX_EDGE: u32 = 2560;

/// Colours derived from a wallpaper, pushed into the Theme global so panels,
/// accent and backgrounds harmonise with the image.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// Wallpaper is dark overall → use the dark base palette.
    pub is_dark: bool,
    /// Vivid accent colour sharing the wallpaper's dominant hue.
    pub accent: (u8, u8, u8),
    /// Average colour, used to subtly tint panel/background surfaces.
    pub tint: (u8, u8, u8),
}

pub struct Wallpaper {
    pub image: Image,
    pub palette: Palette,
}

/// Resolve a stored wallpaper id into an image + palette.
///
/// ids: `""` → none; `"builtin:light"`; `"builtin:dark"`; anything else is
/// treated as a filesystem path to a user image. Returns `None` for "no
/// wallpaper" or when a custom file can't be decoded.
pub fn load(id: &str) -> Option<Wallpaper> {
    if id.is_empty() {
        return None;
    }
    let buf = match id {
        "builtin:light" => render_builtin(false),
        "builtin:dark" => render_builtin(true),
        path => decode_custom(path)?,
    };
    let palette = derive_palette(&buf);
    Some(Wallpaper {
        image: Image::from_rgba8(buf),
        palette,
    })
}

/// True if `id` names one of the procedurally-drawn built-ins.
pub fn is_builtin(id: &str) -> bool {
    matches!(id, "builtin:light" | "builtin:dark")
}

// ── Built-in wallpapers ───────────────────────────────────────────────────────

fn render_builtin(dark: bool) -> SharedPixelBuffer<Rgba8Pixel> {
    // (top-left base, bottom-right base, accent glow) — a calm diagonal gradient
    // with one soft off-centre glow in the brand blue. Minimal by design.
    let (c0, c1, glow) = if dark {
        ((0x10, 0x13, 0x1a), (0x1b, 0x22, 0x33), (0x4a, 0x6c, 0xe0))
    } else {
        ((0xef, 0xf3, 0xfb), (0xd5, 0xe1, 0xf2), (0x4a, 0x90, 0xe2))
    };

    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(W, H);
    let px = buf.make_mut_slice();

    let glow_cx = W as f32 * 0.74;
    let glow_cy = H as f32 * 0.26;
    let glow_r = W as f32 * 0.6;
    let glow_strength = if dark { 0.38 } else { 0.22 };

    for y in 0..H {
        for x in 0..W {
            // Diagonal gradient factor (0 at top-left → 1 at bottom-right).
            let t = ((x as f32 / W as f32) + (y as f32 / H as f32)) * 0.5;
            let mut r = lerp(c0.0, c1.0, t);
            let mut g = lerp(c0.1, c1.1, t);
            let mut b = lerp(c0.2, c1.2, t);

            // Soft radial glow toward the accent (quadratic falloff).
            let dx = x as f32 - glow_cx;
            let dy = y as f32 - glow_cy;
            let d = (dx * dx + dy * dy).sqrt() / glow_r;
            let gf = (1.0 - d).clamp(0.0, 1.0);
            let gf = gf * gf * glow_strength;
            r = blend(r, glow.0, gf);
            g = blend(g, glow.1, gf);
            b = blend(b, glow.2, gf);

            let i = (y * W + x) as usize;
            px[i] = Rgba8Pixel { r, g, b, a: 255 };
        }
    }
    buf
}




// ── Custom wallpapers ─────────────────────────────────────────────────────────

fn decode_custom(path: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    Some(to_buffer(image::open(path).ok()?.to_rgba8()))
}



/// Downscale an oversized decoded image (preserving aspect; the UI covers it)
/// and pack it into a Slint pixel buffer.
fn to_buffer(img: image::RgbaImage) -> SharedPixelBuffer<Rgba8Pixel> {
    let (w, h) = img.dimensions();
    let img = if w.max(h) > MAX_EDGE {
        let scale = MAX_EDGE as f32 / w.max(h) as f32;
        let nw = ((w as f32 * scale) as u32).max(1);
        let nh = ((h as f32 * scale) as u32).max(1);
        image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let (w, h) = img.dimensions();
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    buf.make_mut_bytes().copy_from_slice(img.as_raw());
    buf
}

// ── Palette derivation ────────────────────────────────────────────────────────

fn derive_palette(buf: &SharedPixelBuffer<Rgba8Pixel>) -> Palette {
    let px = buf.as_slice();
    let (mut sr, mut sg, mut sb, mut n) = (0u64, 0u64, 0u64, 0u64);
    // Sample ~a few thousand pixels regardless of image size.
    let step = (px.len() / 4096).max(1);
    let mut i = 0;
    while i < px.len() {
        sr += px[i].r as u64;
        sg += px[i].g as u64;
        sb += px[i].b as u64;
        n += 1;
        i += step;
    }
    let n = n.max(1);
    let (ar, ag, ab) = ((sr / n) as u8, (sg / n) as u8, (sb / n) as u8);
    let lum = 0.299 * ar as f32 + 0.587 * ag as f32 + 0.114 * ab as f32;
    let is_dark = lum < 128.0;
    Palette {
        is_dark,
        accent: vivid_accent(ar, ag, ab, is_dark),
        tint: (ar, ag, ab),
    }
}

/// A saturated, fixed-lightness version of the average colour so the accent
/// stays vivid and readable. Near-grey averages fall back to the brand blue.
fn vivid_accent(r: u8, g: u8, b: u8, is_dark: bool) -> (u8, u8, u8) {
    let (h, s, _l) = rgb_to_hsl(r, g, b);
    let (h, s) = if s < 0.08 {
        (210.0 / 360.0, 0.70) // brand blue when the wallpaper is essentially grey
    } else {
        (h, s.max(0.55))
    };
    let l = if is_dark { 0.62 } else { 0.50 };
    hsl_to_rgb(h, s, l)
}

fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < f32::EPSILON {
        return (0.0, 0.0, l); // achromatic
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    (h, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    if s.abs() < f32::EPSILON {
        let v = (l * 255.0).round() as u8;
        return (v, v, v);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let r = hue_to_rgb(p, q, h + 1.0 / 3.0);
    let g = hue_to_rgb(p, q, h);
    let b = hue_to_rgb(p, q, h - 1.0 / 3.0);
    (
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
    )
}

fn hue_to_rgb(p: f32, q: f32, mut t: f32) -> f32 {
    if t < 0.0 {
        t += 1.0;
    }
    if t > 1.0 {
        t -= 1.0;
    }
    if t < 1.0 / 6.0 {
        p + (q - p) * 6.0 * t
    } else if t < 1.0 / 2.0 {
        q
    } else if t < 2.0 / 3.0 {
        p + (q - p) * (2.0 / 3.0 - t) * 6.0
    } else {
        p
    }
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t)
        .round()
        .clamp(0.0, 255.0) as u8
}

fn blend(base: u8, over: u8, f: f32) -> u8 {
    (base as f32 * (1.0 - f) + over as f32 * f)
        .round()
        .clamp(0.0, 255.0) as u8
}
