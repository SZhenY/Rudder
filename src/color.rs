//! HSL/RGB conversion shared by terminal palette adjustment and wallpaper
//! palette extraction. Both callers used to carry private copies.

/// Convert 8-bit RGB to HSL with all components in `0.0..=1.0`.
pub(crate) fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-6 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < 1e-6 {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if (max - g).abs() < 1e-6 {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    (h, s, l)
}

/// Convert HSL (`0.0..=1.0`) back to 8-bit RGB.
pub(crate) fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    if s < 1e-6 {
        let v = (l * 255.0).round() as u8;
        return (v, v, v);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    (
        (hue_to_rgb(p, q, h + 1.0 / 3.0) * 255.0).round() as u8,
        (hue_to_rgb(p, q, h) * 255.0).round() as u8,
        (hue_to_rgb(p, q, h - 1.0 / 3.0) * 255.0).round() as u8,
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
    } else if t < 0.5 {
        q
    } else if t < 2.0 / 3.0 {
        p + (q - p) * (2.0 / 3.0 - t) * 6.0
    } else {
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primaries_and_gray_round_trip() {
        for rgb in [
            (0, 0, 0),
            (255, 255, 255),
            (255, 0, 0),
            (0, 255, 0),
            (0, 0, 255),
            (128, 128, 128),
        ] {
            let (h, s, l) = rgb_to_hsl(rgb.0, rgb.1, rgb.2);
            assert_eq!(hsl_to_rgb(h, s, l), rgb, "round trip for {rgb:?}");
        }
    }

    #[test]
    fn arbitrary_colors_round_trip_within_one_step() {
        for r in (0..=255).step_by(17) {
            for g in (0..=255).step_by(17) {
                for b in (0..=255).step_by(17) {
                    let (h, s, l) = rgb_to_hsl(r as u8, g as u8, b as u8);
                    let (rr, gg, bb) = hsl_to_rgb(h, s, l);
                    assert!(
                        (i32::from(rr) - r).abs() <= 1
                            && (i32::from(gg) - g).abs() <= 1
                            && (i32::from(bb) - b).abs() <= 1,
                        "({r},{g},{b}) -> ({rr},{gg},{bb})"
                    );
                }
            }
        }
    }

    #[test]
    fn achromatic_has_zero_hue_and_saturation() {
        let (h, s, l) = rgb_to_hsl(90, 90, 90);
        assert_eq!((h, s), (0.0, 0.0));
        assert!((l - 90.0 / 255.0).abs() < 1e-6, "lightness {l}");
    }
}
