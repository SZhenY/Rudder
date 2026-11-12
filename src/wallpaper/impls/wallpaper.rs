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
use crate::color::{hsl_to_rgb, rgb_to_hsl};
use image::GenericImageView as _;
use std::path::{Path, PathBuf};

// ── 用户壁纸目录（与字体目录**并排**：`config/wallpapers`）────────────────────
//
// 位置规则直接复用字体那套（Windows 在 exe 旁的 config 下、其余平台在每用户配置目录下），
// 靠 `with_file_name` 保证两者始终并排 —— 用户找字体和找壁纸是同一个地方。
// 上传的图片**复制**到这里（旧行为是记住原文件路径：原文件一移走壁纸就失效）。

/// 用户壁纸目录：上传的图片落在这里，也是选择器扫描的来源。
pub(crate) fn external_wallpapers_dir() -> PathBuf {
    crate::fonts::external_fonts_dir().with_file_name("wallpapers")
}

/// 接受的图片扩展名（与文件对话框的过滤器一致）。
const WALLPAPER_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp"];

/// 扫描用户壁纸目录（按文件名排序；目录不存在给空表）。
pub(crate) fn scan_wallpaper_files(dir: &Path) -> Vec<PathBuf> {
    crate::files::scan_files(dir, WALLPAPER_EXTS)
}

/// 把用户挑的图片**复制**进壁纸目录，返回落点；重名时**不覆盖**，
/// 改成 `<名字> 2.png`、`<名字> 3.png`……
pub(crate) fn import_wallpaper_file(src: &Path) -> Option<PathBuf> {
    crate::files::import_file(src, &external_wallpapers_dir())
}

/// Render size for the built-in wallpapers. `image-fit: cover` in the UI scales
/// this up to the window, so a fixed, generous size stays crisp without any
/// re-render on resize.
const W: u32 = 1600;
const H: u32 = 1000;

/// Long edge of a decoded custom wallpaper. 4K on a 4K display is 1:1; anything
/// larger only burns GPU memory.
const MAX_EDGE: u32 = 3840;

/// Largest source edge accepted for decoding: 16384×16384 (268 MP, 1 GiB as
/// RGBA8). A single edge cap is equivalent to a pixel cap here and keeps the
/// rejection message unambiguous.
const MAX_SOURCE_EDGE: u32 = 16384;

/// `image`'s default max_alloc is 512 MiB, which would reject a 16K RGBA PNG.
/// 16384² RGBA8 is exactly 1 GiB; leave 512 MiB for the decoder's own scratch
/// buffers (`Limits` is a decreasing budget).
const MAX_DECODE_ALLOC: u64 = 1536 * 1024 * 1024;

/// Reject dimensions that would make a full-size decode unsafe.
fn check_decode_dimensions(w: u32, h: u32) -> Result<(), String> {
    if w > MAX_SOURCE_EDGE || h > MAX_SOURCE_EDGE {
        return Err(format!(
            "{w}x{h} exceeds the {MAX_SOURCE_EDGE}px edge limit"
        ));
    }
    Ok(())
}

/// Long-edge-capped dimensions (round, at least 1px, never upscale).
fn scaled_dims_for(w: u32, h: u32, edge: u32) -> (u32, u32) {
    let long = w.max(h);
    if long <= edge {
        return (w.max(1), h.max(1));
    }
    let scale = f64::from(edge) / f64::from(long);
    (
        (f64::from(w) * scale).round().max(1.0) as u32,
        (f64::from(h) * scale).round().max(1.0) as u32,
    )
}

fn scaled_dims(w: u32, h: u32) -> (u32, u32) {
    scaled_dims_for(w, h, MAX_EDGE)
}

/// Colours derived from a wallpaper, pushed into the Theme global so panels,
/// accent and backgrounds harmonise with the image.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// 深浅档：由**主导色**的亮度判定（不是平均亮度 —— 免得被小面积高光/暗部带偏）。
    pub is_dark: bool,
    /// 强调色：主导色的**分裂互补**（+150°），在底图上一定跳得出来。
    pub accent: (u8, u8, u8),
    /// 主导色（面积最大的颜色簇），面板朝它轻微靠拢。
    pub tint: (u8, u8, u8),
    /// 内容区底色：主导色夹到可读区间后的颜色（面板 / 终端底用它）。
    pub base: (u8, u8, u8),
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
    let (w, h) = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if let Err(reason) = check_decode_dimensions(w, h) {
        tracing::warn!("refusing to decode wallpaper {path}: {reason}");
        return None;
    }
    let mut reader = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    match reader.decode() {
        Ok(img) => Some(to_buffer(img)),
        Err(err) => {
            tracing::warn!("failed to decode wallpaper {path}: {err:#}");
            None
        }
    }
}



/// Downscale before converting to RGBA. Two stages keep the peak at the decoded
/// buffer plus one intermediate: `thumbnail` is an area average with no scratch
/// allocation, then Lanczos3 finishes the last 2×. A single `resize_exact` from
/// 16384² would allocate a 960 MiB Rgba32F intermediate, pushing the peak from
/// ~1.2 GiB to ~2.0 GiB.
fn to_buffer(img: image::DynamicImage) -> SharedPixelBuffer<Rgba8Pixel> {
    let (w, h) = img.dimensions();
    let (nw, nh) = scaled_dims(w, h);
    let img = if (w, h) == (nw, nh) {
        img
    } else if w.max(h) > MAX_EDGE * 2 {
        let (mw, mh) = scaled_dims_for(w, h, MAX_EDGE * 2);
        img.thumbnail_exact(mw, mh)
            .resize_exact(nw, nh, image::imageops::FilterType::Lanczos3)
    } else {
        img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3)
    };
    let rgba = img.into_rgba8();
    let (w, h) = rgba.dimensions();
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    buf.make_mut_bytes().copy_from_slice(rgba.as_raw());
    buf
}

// ── Palette derivation ────────────────────────────────────────────────────────

/// 采样点数与直方图桶数（均为 8k）：统计够细，又不会把一张图拆得太散。
const SAMPLES: usize = 8192;
const BUCKETS: usize = 8192;

fn derive_palette(buf: &SharedPixelBuffer<Rgba8Pixel>) -> Palette {
    let px = buf.as_slice();
    let step = (px.len() / SAMPLES).max(1);
    let mut hist = vec![0u32; BUCKETS];
    let mut total = 0u32;
    let mut i = 0;
    while i < px.len() {
        hist[bucket(px[i])] += 1;
        total += 1;
        i += step;
    }
    let total = total.max(1) as f32;

    // 主导色 = 占比最大的颜色桶（面积最大的那一片），而不是平均色 ——
    // 平均色遇到高对比图会糊成灰，定档与取色都会被带偏。
    let (dom_idx, dom_count) = hist
        .iter()
        .enumerate()
        .max_by_key(|(_, c)| *c)
        .unwrap_or((0, &0));
    let dominant = bucket_color(dom_idx);
    let share = *dom_count as f32 / total;
    let (dh, ds, dl) = rgb_to_hsl(dominant.0, dominant.1, dominant.2);

    // 深浅档由**主导色**的亮度决定。
    let is_dark = dl < 0.5;
    // 鲜明簇：占比前 24 的桶里，彩度最高且亮度居中者（给 accent 供色相/彩度）。
    let vivid = pick_vivid(&hist);
    Palette {
        is_dark,
        accent: split_complementary_accent(dh, ds, vivid, is_dark),
        tint: dominant,
        base: content_base(dh, ds, dl, is_dark, share),
    }
}

/// RGB → 桶号：5 位红 + 4 位绿 + 4 位蓝 = 8192 桶。
fn bucket(p: Rgba8Pixel) -> usize {
    ((p.r >> 3) as usize) * 256 + ((p.g >> 4) as usize) * 16 + (p.b >> 4) as usize
}

/// 桶号 → 该桶中心的颜色。
fn bucket_color(idx: usize) -> (u8, u8, u8) {
    let r = ((idx / 256) as u8) * 8 + 4;
    let g = (((idx / 16) % 16) as u8) * 16 + 8;
    let b = ((idx % 16) as u8) * 16 + 8;
    (r, g, b)
}

/// 占比前 24 的桶里挑"最鲜明"的一个：彩度最高、且亮度落在中间区间（不过曝也不死黑）。
fn pick_vivid(hist: &[u32]) -> Option<(f32, f32, f32)> {
    let mut top: Vec<(u32, usize)> = hist
        .iter()
        .enumerate()
        .map(|(i, c)| (*c, i))
        .collect();
    top.sort_unstable_by_key(|(c, _)| std::cmp::Reverse(*c));
    let mut best: Option<(f32, f32, f32)> = None;
    for (count, idx) in top.into_iter().take(24) {
        if count == 0 {
            break;
        }
        let (r, g, b) = bucket_color(idx);
        let (h, s, l) = rgb_to_hsl(r, g, b);
        if !(0.25..=0.75).contains(&l) {
            continue;
        }
        if best.is_none_or(|(_, bs, _)| s > bs) {
            best = Some((h, s, l));
        }
    }
    best
}

/// 内容区底色：主导色夹到**可读区间**。
///
/// 彩度压到 ≤ 0.30（满屏高饱和会刺眼，也让文字难读），亮度按档位夹紧：
/// 深档 0.10–0.22、浅档 0.88–0.96。花图（主导色占比 < 12%）退回中性底。
fn content_base(h: f32, s: f32, l: f32, is_dark: bool, share: f32) -> (u8, u8, u8) {
    if share < 0.12 {
        return if is_dark { (0x23, 0x26, 0x2d) } else { (0xff, 0xff, 0xff) };
    }
    let s = s.min(0.30);
    let l = if is_dark {
        l.clamp(0.10, 0.22)
    } else {
        l.clamp(0.88, 0.96)
    };
    hsl_to_rgb(h, s, l)
}

/// 强调色 = 主导色的**分裂互补**（色相 +150°）。
///
/// 用分裂互补而不是"同色相的鲜明版"：同色相的强调色和背景一个色系、跳不出来；
/// 纯互补（+180°）又容易和图片里的色块直接撞上。+150° 兼顾。
/// 彩度取鲜明簇的彩度（≥ 0.55 兜底），亮度按档位给到与底色拉开对比。
fn split_complementary_accent(
    dh: f32,
    ds: f32,
    vivid: Option<(f32, f32, f32)>,
    is_dark: bool,
) -> (u8, u8, u8) {
    // 鲜明簇也要有彩度才算数（灰图的"最鲜明"仍然是灰，色相没有意义）。
    let vivid = vivid.filter(|(_, s, _)| *s >= 0.08);
    let (hue, sat) = match (ds >= 0.08, vivid) {
        // 主导色有色相 → 分裂互补（+150°），彩度跟着鲜明簇走
        (true, v) => (
            (dh + 150.0 / 360.0) % 1.0,
            v.map_or(0.65, |(_, s, _)| s.max(0.55)),
        ),
        // 主导色是灰，但图里有鲜明色块 → 用那一块的色相
        (false, Some((h, s, _))) => (h, s.max(0.55)),
        // 整张图基本是灰 → 品牌蓝
        (false, None) => (210.0 / 360.0, 0.70),
    };
    let light = if is_dark { 0.65 } else { 0.48 };
    hsl_to_rgb(hue, sat, light)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(r: u8, g: u8, b: u8) -> SharedPixelBuffer<Rgba8Pixel> {
        let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(64, 64);
        for p in buf.make_mut_slice().iter_mut() {
            *p = Rgba8Pixel { r, g, b, a: 255 };
        }
        buf
    }

    /// 纯暗图 → 深档；底色落在可读的暗区间；accent 与主导色相差约 150°。
    #[test]
    fn dark_wallpaper_drives_dark_base_and_split_complementary_accent() {
        let p = derive_palette(&solid(20, 18, 30));
        assert!(p.is_dark);
        let (_h, _s, l) = rgb_to_hsl(p.base.0, p.base.1, p.base.2);
        assert!((0.10..=0.22).contains(&l), "base lightness {l}");
        let (dh, ds, _dl) = rgb_to_hsl(p.tint.0, p.tint.1, p.tint.2);
        let (ah, as_, _al) = rgb_to_hsl(p.accent.0, p.accent.1, p.accent.2);
        assert!(as_ >= 0.55, "accent saturation {as_}");
        // 纯色的彩度可能低于阈值（走品牌蓝），否则应为主色相 +150°
        if ds >= 0.08 {
            let want = (dh + 150.0 / 360.0) % 1.0;
            let delta = (ah - want).abs().min(1.0 - (ah - want).abs());
            assert!(delta < 0.02, "accent hue {ah} vs want {want}");
        }
    }

    /// 纯亮图 → 浅档，且底色落在可读的亮区间。
    #[test]
    fn light_wallpaper_drives_light_base() {
        let p = derive_palette(&solid(235, 232, 226));
        assert!(!p.is_dark);
        let (_h, _s, l) = rgb_to_hsl(p.base.0, p.base.1, p.base.2);
        assert!((0.88..=0.96).contains(&l), "base lightness {l}");
    }

    /// 灰图（无彩度、无鲜明簇）→ 强调色回到品牌蓝。
    #[test]
    fn grey_wallpaper_falls_back_to_brand_blue() {
        let p = derive_palette(&solid(128, 128, 128));
        let (h, _s, _l) = rgb_to_hsl(p.accent.0, p.accent.1, p.accent.2);
        let want = 210.0 / 360.0;
        let delta = (h - want).abs().min(1.0 - (h - want).abs());
        assert!(delta < 0.02, "accent hue {h}");
    }

    /// 高对比图不能用"平均亮度"定档：一半纯黑一半纯白时主导桶不应把档位带偏到中间值。
    #[test]
    fn high_contrast_image_still_picks_a_bucket() {
        let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(64, 64);
        for (i, p) in buf.make_mut_slice().iter_mut().enumerate() {
            *p = if i % 2 == 0 {
                Rgba8Pixel { r: 0, g: 0, b: 0, a: 255 }
            } else {
                Rgba8Pixel { r: 255, g: 255, b: 255, a: 255 }
            };
        }
        let p = derive_palette(&buf);
        // 两个桶各占一半：取到的主导桶必然是纯黑或纯白，亮度贴边（不是中间灰）。
        let (_h, _s, l) = rgb_to_hsl(p.tint.0, p.tint.1, p.tint.2);
        assert!(l < 0.05 || l > 0.95, "dominant lightness {l}");
    }

    #[test]
    fn decode_limit_accepts_16k_and_square_wallpapers() {
        assert!(check_decode_dimensions(16384, 16384).is_ok());
        assert!(check_decode_dimensions(16384, 8640).is_ok());
        assert!(check_decode_dimensions(15360, 8640).is_ok());
        assert!(check_decode_dimensions(16384, 100).is_ok());
    }

    #[test]
    fn decode_limit_rejects_oversized_images_with_details() {
        let err = check_decode_dimensions(16385, 100).unwrap_err();
        assert!(err.contains("16385x100"), "missing dimensions: {err}");
        assert!(err.contains("16384px"), "missing limit: {err}");
        assert!(check_decode_dimensions(20000, 20000).is_err());
        assert!(check_decode_dimensions(16384, 16385).is_err());
    }

    #[test]
    fn scaled_dims_keeps_small_images_untouched() {
        assert_eq!(scaled_dims(3840, 2160), (3840, 2160));
        assert_eq!(scaled_dims(100, 50), (100, 50));
    }

    #[test]
    fn scaled_dims_caps_the_long_edge() {
        assert_eq!(scaled_dims(7680, 4320), (3840, 2160));
        assert_eq!(scaled_dims(16384, 16384), (3840, 3840));
        assert_eq!(scaled_dims(16384, 9216), (3840, 2160));
        assert_eq!(scaled_dims(10000, 500), (3840, 192));
    }

    #[test]
    fn intermediate_dims_are_twice_the_target_and_never_upscale() {
        assert_eq!(scaled_dims_for(16384, 16384, MAX_EDGE * 2), (7680, 7680));
        assert_eq!(scaled_dims_for(16384, 9216, MAX_EDGE * 2), (7680, 4320));
        assert_eq!(scaled_dims_for(20000, 1000, MAX_EDGE * 2), (7680, 384));
        assert_eq!(scaled_dims_for(1000, 500, MAX_EDGE * 2), (1000, 500));
    }
}
