//! Picture adjustments: the sliders under the effect.
//!
//! The four effects are fixed looks ported from Pillow. These are continuous, and
//! run *after* the effect on the finished canvas, so `bw` plus brightness does what
//! it says. They have no Pillow counterpart and therefore no goldens; what keeps the
//! golden suite meaningful is that the neutral setting is an exact identity —
//! [`apply`] returns the canvas untouched when every value is 0.
//!
//! All six are integers in `[display]`. Not floats: `config::json_to_toml_value`
//! writes a whole-number float back as an integer, so a float key would change type
//! on every save.
//!
//! Where the arithmetic has a Pillow shape it keeps it. Brightness, contrast and
//! saturation are the `ImageEnhance` blends that [`crate::effects`] uses —
//! `a + f*(b - a)`, truncated and clamped — against black, a flat mean-luma grey,
//! and the picture's own greyscale. They are computed per pixel rather than through
//! whole degenerate images, which on a three-monitor canvas would be another 75 MB
//! each.
//!
//! Blur and vignette work **per monitor**. The canvas is the whole virtual desktop,
//! and a blur that ran across it would bleed one screen's picture into the next,
//! while a single vignette would darken the middle of a three-screen setup and leave
//! the inner edges alone. Both are also sized relative to the monitor, not in pixels,
//! so the preview — composed at full size and only scaled down afterwards — shows the
//! same thing the desktop gets.

use image::RgbImage;
use serde_json::Value;

use crate::effects::{luma, mean_luma};
use crate::monitor::{virtual_desktop, Monitor};
use crate::parallel::MAX_WORKERS;

/// Every adjustment: its `[display]` key and its range. 0 is neutral for all of them.
pub const RANGES: [(&str, i32, i32); 6] = [
    ("brightness", -100, 100),
    ("contrast", -100, 100),
    ("saturation", -100, 100),
    ("warmth", -100, 100),
    ("blur", 0, 100),
    ("vignette", 0, 100),
];

/// The strongest warm or cool shift: red and blue move this far in opposite
/// directions at ±100.
const WARMTH_SPAN: f64 = 0.12;

/// Blur radius at 100, as a share of the monitor's shorter side.
const BLUR_SPAN: f64 = 0.02;

/// How dark the corners get at vignette 100.
const VIGNETTE_STRENGTH: f64 = 0.6;

/// Where the vignette starts, as a share of the centre-to-corner distance.
const VIGNETTE_START: f64 = 0.35;

/// The six sliders, clamped to their ranges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Adjustments {
    pub brightness: i32,
    pub contrast: i32,
    pub saturation: i32,
    pub warmth: i32,
    pub blur: i32,
    pub vignette: i32,
}

impl Adjustments {
    /// Read `[display]`. A missing or malformed value is neutral and an out-of-range
    /// one is clamped — like every other config reader here, this never refuses to
    /// draw a wallpaper over a setting.
    pub fn from_config(cfg: &Value) -> Self {
        let read = |key: &str| -> i32 {
            let (_, lo, hi) = RANGES.iter().find(|(k, _, _)| *k == key).copied().unwrap();
            cfg.pointer(&format!("/display/{key}"))
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite())
                .map(|v| (v.round() as i64).clamp(lo as i64, hi as i64) as i32)
                .unwrap_or(0)
        };
        Self {
            brightness: read("brightness"),
            contrast: read("contrast"),
            saturation: read("saturation"),
            warmth: read("warmth"),
            blur: read("blur"),
            vignette: read("vignette"),
        }
    }

    /// Whether applying these would change nothing.
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }

    fn has_tone(&self) -> bool {
        self.brightness != 0 || self.contrast != 0 || self.saturation != 0 || self.warmth != 0
    }
}

/// A monitor's rectangle in canvas coordinates, clipped to the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Region {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// The canvas split by monitor. The whole canvas when there are no monitors to go by.
fn regions(canvas: &RgbImage, monitors: &[Monitor]) -> Vec<Region> {
    let (cw, ch) = canvas.dimensions();
    let whole = vec![Region {
        x: 0,
        y: 0,
        width: cw,
        height: ch,
    }];
    let Ok((min_x, min_y, _, _)) = virtual_desktop(monitors) else {
        return whole;
    };
    let found: Vec<Region> = monitors
        .iter()
        .filter_map(|m| {
            let x0 = (m.x - min_x).clamp(0, cw as i32) as u32;
            let y0 = (m.y - min_y).clamp(0, ch as i32) as u32;
            let x1 = (m.x - min_x + m.width).clamp(0, cw as i32) as u32;
            let y1 = (m.y - min_y + m.height).clamp(0, ch as i32) as u32;
            (x1 > x0 && y1 > y0).then(|| Region {
                x: x0,
                y: y0,
                width: x1 - x0,
                height: y1 - y0,
            })
        })
        .collect();
    if found.is_empty() {
        whole
    } else {
        found
    }
}

/// Apply *adjust* to the finished canvas.
///
/// Blur first, so it softens the picture rather than the vignette; then tone; then
/// the vignette, last, so brightness does not wash it back out.
pub fn apply(canvas: RgbImage, adjust: &Adjustments, monitors: &[Monitor]) -> RgbImage {
    if adjust.is_identity() {
        return canvas;
    }
    let mut canvas = canvas;
    let regions = regions(&canvas, monitors);

    if adjust.blur > 0 {
        for region in &regions {
            let radius = blur_radius(adjust.blur, region);
            if radius > 0 {
                blur_region(&mut canvas, region, radius);
            }
        }
    }
    if adjust.has_tone() {
        tone(&mut canvas, adjust);
    }
    if adjust.vignette > 0 {
        for region in &regions {
            vignette_region(&mut canvas, region, adjust.vignette);
        }
    }
    canvas
}

/// `a + f*(b - a)`, truncated and clamped — [`crate::effects`]' `blend`, for one value.
#[inline]
fn lerp(a: f64, b: f64, factor: f64) -> u8 {
    (a + factor * (b - a)).clamp(0.0, 255.0) as u8
}

/// 1 at 0, 0 at −100, 2 at +100 — the `ImageEnhance` factor a slider stands for.
fn factor(value: i32) -> f64 {
    1.0 + value as f64 / 100.0
}

/// Brightness, contrast, saturation and warmth.
fn tone(canvas: &mut RgbImage, adjust: &Adjustments) {
    // `ImageEnhance.Brightness`: a blend against black.
    if adjust.brightness != 0 {
        let f = factor(adjust.brightness);
        let lut: [u8; 256] = std::array::from_fn(|v| lerp(0.0, v as f64, f));
        for pixel in canvas.pixels_mut() {
            for c in 0..3 {
                pixel[c] = lut[pixel[c] as usize];
            }
        }
    }

    // `ImageEnhance.Contrast`: a blend against a flat grey at the mean luminance —
    // measured *after* brightness, since that is the picture contrast is applied to.
    let contrast = (adjust.contrast != 0).then(|| {
        let mean = mean_luma(canvas) as f64;
        let f = factor(adjust.contrast);
        std::array::from_fn::<u8, 256, _>(|v| lerp(mean, v as f64, f))
    });

    let saturation = (adjust.saturation != 0).then(|| factor(adjust.saturation));

    // Red and blue pulled in opposite directions; green is left alone.
    let warmth = (adjust.warmth != 0).then(|| {
        let shift = adjust.warmth as f64 / 100.0 * WARMTH_SPAN;
        let scale = |gain: f64| -> [u8; 256] {
            std::array::from_fn(|v| (v as f64 * gain).round().clamp(0.0, 255.0) as u8)
        };
        (scale(1.0 + shift), scale(1.0 - shift))
    });

    if contrast.is_none() && saturation.is_none() && warmth.is_none() {
        return;
    }
    for pixel in canvas.pixels_mut() {
        if let Some(lut) = &contrast {
            for c in 0..3 {
                pixel[c] = lut[pixel[c] as usize];
            }
        }
        // `ImageEnhance.Color`: a blend against the pixel's own grey.
        if let Some(f) = saturation {
            let grey = luma(pixel[0], pixel[1], pixel[2]) as f64;
            for c in 0..3 {
                pixel[c] = lerp(grey, pixel[c] as f64, f);
            }
        }
        if let Some((red, blue)) = &warmth {
            pixel[0] = red[pixel[0] as usize];
            pixel[2] = blue[pixel[2] as usize];
        }
    }
}

/// The box radius for *blur* on this region: 1 at the very least once it is on.
fn blur_radius(blur: i32, region: &Region) -> usize {
    let short = region.width.min(region.height) as f64;
    let radius = (blur as f64 / 100.0 * BLUR_SPAN * short).round() as usize;
    radius.max(1)
}

/// Three box blurs in each direction — close to a Gaussian, and linear in the
/// radius rather than quadratic, so a heavy blur costs what a light one does. The
/// edge is clamped, not wrapped and not black, so a monitor's border does not darken.
///
/// Each pass is split into horizontal bands across [`MAX_WORKERS`] threads. Rows are
/// independent for the horizontal pass, and for the vertical one each band seeds its
/// own running sums from the rows above it, so the bands never write to one another.
fn blur_region(canvas: &mut RgbImage, region: &Region, radius: usize) {
    let (w, h) = (region.width as usize, region.height as usize);
    let canvas_stride = canvas.width() as usize * 3;
    let (x0, y0) = (region.x as usize * 3, region.y as usize);
    let stride = w * 3;

    let mut buf = Vec::with_capacity(stride * h);
    for y in 0..h {
        let start = (y0 + y) * canvas_stride + x0;
        buf.extend_from_slice(&canvas.as_raw()[start..start + stride]);
    }

    let mut scratch = vec![0u8; buf.len()];
    for _ in 0..3 {
        in_bands(&mut scratch, stride, h, |out, first| {
            for (k, row) in out.chunks_mut(stride).enumerate() {
                let y = first + k;
                blur_row(&buf[y * stride..(y + 1) * stride], row, w, radius);
            }
        });
        in_bands(&mut buf, stride, h, |out, first| {
            blur_columns(&scratch, out, first, stride, h, radius);
        });
    }

    let raw: &mut [u8] = canvas;
    for y in 0..h {
        let start = (y0 + y) * canvas_stride + x0;
        raw[start..start + stride].copy_from_slice(&buf[y * stride..(y + 1) * stride]);
    }
}

/// Run *work* over *buf* split into bands of whole rows, one thread per band. It is
/// handed each band and the index of its first row.
fn in_bands(buf: &mut [u8], stride: usize, h: usize, work: impl Fn(&mut [u8], usize) + Sync) {
    let rows = h.div_ceil(MAX_WORKERS).max(1);
    std::thread::scope(|scope| {
        for (index, band) in buf.chunks_mut(rows * stride).enumerate() {
            let work = &work;
            scope.spawn(move || work(band, index * rows));
        }
    });
}

/// One row of a horizontal box blur, all three channels on one running sum each.
fn blur_row(row: &[u8], out: &mut [u8], w: usize, radius: usize) {
    let span = (2 * radius + 1) as u32;
    let last = w - 1;
    let mut sum = [0u32; 3];
    for dx in -(radius as isize)..=radius as isize {
        let x = dx.clamp(0, last as isize) as usize;
        for c in 0..3 {
            sum[c] += row[x * 3 + c] as u32;
        }
    }
    for x in 0..w {
        let (add, sub) = ((x + radius + 1).min(last), x.saturating_sub(radius));
        for c in 0..3 {
            out[x * 3 + c] = ((sum[c] + span / 2) / span) as u8;
            sum[c] = sum[c] + row[add * 3 + c] as u32 - row[sub * 3 + c] as u32;
        }
    }
}

/// The vertical box blur for the rows of *out*, which start at row *first* of *src*.
/// A whole row of running sums at a time, so the reads stay sequential.
fn blur_columns(src: &[u8], out: &mut [u8], first: usize, stride: usize, h: usize, radius: usize) {
    let span = (2 * radius + 1) as u32;
    let row = |y: isize| {
        let y = y.clamp(0, h as isize - 1) as usize;
        &src[y * stride..(y + 1) * stride]
    };
    let mut sums = vec![0u32; stride];
    for dy in -(radius as isize)..=radius as isize {
        for (sum, &v) in sums.iter_mut().zip(row(first as isize + dy)) {
            *sum += v as u32;
        }
    }
    for (k, out_row) in out.chunks_mut(stride).enumerate() {
        let y = (first + k) as isize;
        for (o, &sum) in out_row.iter_mut().zip(&sums) {
            *o = ((sum + span / 2) / span) as u8;
        }
        let (add, sub) = (row(y + radius as isize + 1), row(y - radius as isize));
        for ((sum, &a), &s) in sums.iter_mut().zip(add).zip(sub) {
            *sum = *sum + a as u32 - s as u32;
        }
    }
}

/// Darken toward the corners of one monitor's rectangle.
///
/// Distance is measured on the rectangle's own proportions, so the falloff follows
/// the screen's shape rather than being a circle cut off by its short side.
fn vignette_region(canvas: &mut RgbImage, region: &Region, vignette: i32) {
    let strength = vignette as f64 / 100.0 * VIGNETTE_STRENGTH;
    let (half_w, half_h) = (region.width as f64 / 2.0, region.height as f64 / 2.0);
    let canvas_stride = canvas.width() as usize * 3;
    let raw: &mut [u8] = canvas;
    for y in 0..region.height as usize {
        let dy = (y as f64 + 0.5 - half_h) / half_h;
        let start = (region.y as usize + y) * canvas_stride + region.x as usize * 3;
        let row = &mut raw[start..start + region.width as usize * 3];
        for (x, pixel) in row.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let dx = (x as f64 + 0.5 - half_w) / half_w;
            // 0 at the centre, 1 in the corners.
            let d = ((dx * dx + dy * dy) / 2.0).sqrt();
            let t = ((d - VIGNETTE_START) / (1.0 - VIGNETTE_START)).clamp(0.0, 1.0);
            if t <= 0.0 {
                continue;
            }
            let keep = 1.0 - strength * t * t * (3.0 - 2.0 * t);
            for value in pixel {
                *value = (*value as f64 * keep) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(w, h, image::Rgb(rgb))
    }

    fn noisy(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([
                ((x * 37 + y * 11) % 256) as u8,
                ((x * 13 + y * 71) % 256) as u8,
                ((x * 91 + y * 7) % 256) as u8,
            ])
        })
    }

    fn only(key: &str, value: i32) -> Adjustments {
        Adjustments::from_config(&json!({ "display": { key: value } }))
    }

    fn monitor(index: usize, x: i32, y: i32, width: i32, height: i32) -> Monitor {
        Monitor {
            index,
            x,
            y,
            width,
            height,
            name: String::new(),
        }
    }

    /// The guarantee that keeps the effect goldens meaningful: neutral is exact.
    #[test]
    fn neutral_changes_no_byte() {
        let canvas = noisy(40, 30);
        assert_eq!(apply(canvas.clone(), &Adjustments::default(), &[]), canvas);
        let cfg = json!({ "display": { "effect": "hdr" } });
        assert!(Adjustments::from_config(&cfg).is_identity());
    }

    #[test]
    fn config_values_are_clamped_rounded_and_forgiving() {
        let cfg = json!({ "display": {
            "brightness": 250, "contrast": -300, "saturation": 12.6,
            "warmth": "warm", "blur": -5, "vignette": 40,
        }});
        let adjust = Adjustments::from_config(&cfg);
        assert_eq!(
            adjust,
            Adjustments {
                brightness: 100,
                contrast: -100,
                saturation: 13,
                warmth: 0,
                blur: 0,
                vignette: 40
            }
        );
        assert!(Adjustments::from_config(&json!({})).is_identity());
    }

    #[test]
    fn brightness_runs_from_black_to_double() {
        let canvas = solid(2, 2, [100, 150, 200]);
        assert_eq!(
            apply(canvas.clone(), &only("brightness", -100), &[])
                .get_pixel(0, 0)
                .0,
            [0, 0, 0]
        );
        assert_eq!(
            apply(canvas.clone(), &only("brightness", 100), &[])
                .get_pixel(0, 0)
                .0,
            [200, 255, 255]
        );
        assert_eq!(
            apply(canvas, &only("brightness", -50), &[])
                .get_pixel(0, 0)
                .0,
            [50, 75, 100]
        );
    }

    /// −100 is `ImageEnhance.Contrast(0)`: every pixel becomes the mean grey.
    #[test]
    fn contrast_at_minus_100_is_flat_mean_grey() {
        let canvas = noisy(20, 20);
        let mean = mean_luma(&canvas);
        let out = apply(canvas, &only("contrast", -100), &[]);
        assert!(out.pixels().all(|p| p.0 == [mean, mean, mean]));
    }

    #[test]
    fn saturation_at_minus_100_is_greyscale() {
        let canvas = noisy(20, 20);
        let out = apply(canvas.clone(), &only("saturation", -100), &[]);
        for (a, b) in canvas.pixels().zip(out.pixels()) {
            let l = luma(a[0], a[1], a[2]);
            assert_eq!(b.0, [l, l, l]);
        }
    }

    #[test]
    fn warmth_moves_red_and_blue_apart_and_leaves_green() {
        let canvas = solid(1, 1, [100, 100, 100]);
        let warm = apply(canvas.clone(), &only("warmth", 100), &[])
            .get_pixel(0, 0)
            .0;
        assert!(warm[0] > 100 && warm[1] == 100 && warm[2] < 100, "{warm:?}");
        let cool = apply(canvas, &only("warmth", -100), &[]).get_pixel(0, 0).0;
        assert!(cool[0] < 100 && cool[1] == 100 && cool[2] > 100, "{cool:?}");
    }

    /// A flat picture stays flat: a clamped-edge blur must not darken the border.
    #[test]
    fn blur_keeps_a_flat_picture_flat() {
        let canvas = solid(64, 48, [90, 120, 30]);
        assert_eq!(apply(canvas.clone(), &only("blur", 100), &[]), canvas);
    }

    #[test]
    fn blur_softens_a_hard_edge() {
        let canvas = RgbImage::from_fn(100, 100, |x, _| {
            if x < 50 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            }
        });
        let out = apply(canvas, &only("blur", 100), &[]);
        let at_edge = out.get_pixel(50, 50)[0];
        assert!(at_edge > 0 && at_edge < 255, "{at_edge}");
        assert_eq!(out.get_pixel(0, 50)[0], 0);
        assert_eq!(out.get_pixel(99, 50)[0], 255);
    }

    /// The vertical pass runs in bands on separate threads. A seam between two bands
    /// would show as a step in an otherwise smooth ramp, so blur a horizontal edge
    /// and require every column to be the same, monotonic, gradient.
    #[test]
    fn blur_bands_leave_no_seam() {
        let canvas = RgbImage::from_fn(30, 200, |_, y| {
            if y < 100 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([240, 240, 240])
            }
        });
        let out = apply(canvas, &only("blur", 100), &[]);
        let column: Vec<u8> = (0..200).map(|y| out.get_pixel(0, y)[0]).collect();
        assert!(column.windows(2).all(|p| p[0] <= p[1]), "{column:?}");
        assert!(column[0] == 0 && column[199] == 240);
        for x in 1..30 {
            assert!((0..200).all(|y| out.get_pixel(x, y)[0] == column[y as usize]));
        }
    }

    /// Two screens side by side: one black, one white. Blurring must not carry
    /// either colour across the seam.
    #[test]
    fn blur_does_not_cross_monitors() {
        let monitors = [monitor(0, 0, 0, 50, 40), monitor(1, 50, 0, 50, 40)];
        let canvas = RgbImage::from_fn(100, 40, |x, _| {
            if x < 50 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            }
        });
        let out = apply(canvas.clone(), &only("blur", 100), &monitors);
        assert_eq!(out, canvas);
    }

    #[test]
    fn vignette_darkens_the_corners_and_spares_the_centre() {
        let canvas = solid(100, 60, [200, 200, 200]);
        let out = apply(canvas, &only("vignette", 100), &[]);
        assert_eq!(out.get_pixel(50, 30).0, [200, 200, 200]);
        assert!(out.get_pixel(0, 0)[0] < 120, "{:?}", out.get_pixel(0, 0));
        assert!(out.get_pixel(99, 59)[0] < 120);
    }

    /// Each screen gets its own vignette, so the seam between two is dark on both
    /// sides rather than being the bright middle of one big oval.
    #[test]
    fn each_monitor_gets_its_own_vignette() {
        let monitors = [monitor(0, 0, 0, 50, 40), monitor(1, 50, 0, 50, 40)];
        let canvas = solid(100, 40, [200, 200, 200]);
        let out = apply(canvas, &only("vignette", 100), &monitors);
        assert!(out.get_pixel(49, 0)[0] < 150);
        assert!(out.get_pixel(50, 0)[0] < 150);
        assert_eq!(out.get_pixel(25, 20).0, [200, 200, 200]);
        assert_eq!(out.get_pixel(75, 20).0, [200, 200, 200]);
    }

    /// Monitors at negative offsets map onto the canvas like the composer maps them.
    #[test]
    fn regions_follow_the_virtual_desktop_origin() {
        let canvas = solid(140, 70, [0, 0, 0]);
        let monitors = [monitor(0, 0, 0, 100, 60), monitor(1, 100, -10, 40, 30)];
        assert_eq!(
            regions(&canvas, &monitors),
            vec![
                Region {
                    x: 0,
                    y: 10,
                    width: 100,
                    height: 60
                },
                Region {
                    x: 100,
                    y: 0,
                    width: 40,
                    height: 30
                },
            ]
        );
    }
}
