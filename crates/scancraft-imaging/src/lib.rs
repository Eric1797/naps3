//! Pixel transforms and codecs for ScanCraft (pure Rust).
//!
//! Port of NAPS2's `Transforms/` vocabulary (brightness, contrast,
//! grayscale, black-white, crop, rotate, flip, scale) plus the small BMP /
//! PNG / JPEG codec helpers used around it, reimplemented on top of the
//! `image` crate. See `naps2-in c++-master #For Reference/` at the repo root
//! for the original NAPS2 sources.
//!
//! Like NAPS2, this crate is GPL-2.0-or-later. No OS calls, no unsafe code.
//! Corrupt input maps to [`ScanError::Driver`]; encode failures map to
//! [`ScanError::Io`]. All arithmetic on dimensions and buffer offsets is
//! checked; nothing here panics on hostile input.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::f64::consts::PI;
use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ExtendedColorType, ImageFormat, RgbImage};
use scancraft_model::{BitDepth, PageSide};
use scancraft_scan_core::{RawPage, ScanError};

/// Hard cap for any encoded/decoded/resized side, in pixels.
const MAX_SIDE_PX: u32 = 20_000;
/// DPI stamped on decoded pages (container formats carry no reliable DPI here).
const DEFAULT_DPI: u32 = 200;
/// NAPS2 brightness/contrast range.
const LEVEL_LIMIT: i32 = 1000;
/// Luma threshold below which a sampled pixel counts as "dark" for deskew.
const DESKEW_DARK_THRESH: u8 = 128;
/// Minimum dark samples before the deskew search runs.
const DESKEW_MIN_DARK: usize = 16;

/// A pixel operation applied in place to a [`RawPage`]'s RGB8 buffer.
///
/// NAPS2 origin of each variant is noted on its doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    /// NAPS2: `BrightnessTransform`. Shift `-1000..1000`; offset added to
    /// every channel is `value * 255 / 1000`, clamped to `0..=255`.
    Brightness(i32),
    /// NAPS2: `ContrastTransform`. Gain `-1000..1000`; each channel is moved
    /// to `(v - 128) * (1000 + value) / 1000 + 128`, clamped to `0..=255`.
    Contrast(i32),
    /// NAPS2: `TrueGrayscaleTransform`. Rec.601 luma into all three channels;
    /// the buffer stays RGB8 but `bit_depth` becomes [`BitDepth::Grayscale`].
    Grayscale,
    /// NAPS2: `BlackWhiteTransform`. Rec.601 luma thresholded to `0`/`255`;
    /// `bit_depth` becomes [`BitDepth::BlackWhite`].
    BlackWhite {
        /// Luma at or above this becomes white (`255`), below becomes `0`.
        threshold: u8,
    },
    /// NAPS2: `CropTransform`. Rectangle is clamped to the page bounds; an
    /// origin outside the page (or an empty remainder) is a driver error.
    Crop {
        /// Left edge, in pixels.
        x: u32,
        /// Top edge, in pixels.
        y: u32,
        /// Requested width; clamped to the page.
        w: u32,
        /// Requested height; clamped to the page.
        h: u32,
    },
    /// NAPS2: `RotationTransform` (90 degrees). Clockwise.
    Rotate90,
    /// NAPS2: `RotationTransform` (180 degrees).
    Rotate180,
    /// NAPS2: `RotationTransform` (270 degrees). Counter-clockwise.
    Rotate270,
    /// NAPS2: `FlipTransform` (horizontal). Mirrors left-right.
    FlipH,
    /// NAPS2: `FlipTransform` (vertical). Mirrors top-bottom.
    FlipV,
    /// NAPS2: `ScaleTransform`. Nearest-neighbor resample to exactly `w` x
    /// `h`; sides must be `1..=20000`.
    Resize {
        /// Target width, in pixels.
        w: u32,
        /// Target height, in pixels.
        h: u32,
    },
}

/// Apply one [`Transform`] to a page in place, updating dimensions and
/// `bit_depth` as documented on each variant.
pub fn apply_transform(page: &mut RawPage, t: &Transform) -> Result<(), ScanError> {
    match *t {
        Transform::Brightness(v) => apply_brightness(page, v),
        Transform::Contrast(v) => apply_contrast(page, v),
        Transform::Grayscale => apply_grayscale(page),
        Transform::BlackWhite { threshold } => apply_black_white(page, threshold),
        Transform::Crop { x, y, w, h } => apply_crop(page, x, y, w, h),
        Transform::Rotate90 => apply_rotate_cw(page),
        Transform::Rotate180 => apply_rotate180(page),
        Transform::Rotate270 => apply_rotate_ccw(page),
        Transform::FlipH => apply_flip_h(page),
        Transform::FlipV => apply_flip_v(page),
        Transform::Resize { w, h } => apply_resize(page, w, h),
    }
}

/// Expected RGB8 byte count for `width` x `height`, with checked math.
fn expected_rgb8_len(width: u32, height: u32) -> Result<usize, ScanError> {
    let w = usize::try_from(width).map_err(|_| ScanError::Driver("page too wide".to_owned()))?;
    let h = usize::try_from(height).map_err(|_| ScanError::Driver("page too tall".to_owned()))?;
    w.checked_mul(h).and_then(|n| n.checked_mul(3)).ok_or_else(|| ScanError::Driver("page too large".to_owned()))
}

/// Reject zero-size pages and buffers whose length disagrees with the
/// dimensions. Returns `(width, height)` for convenience.
fn checked_dims(page: &RawPage) -> Result<(u32, u32), ScanError> {
    if page.width_px == 0 || page.height_px == 0 {
        return Err(ScanError::Driver("page has zero width or height".to_owned()));
    }
    let want = expected_rgb8_len(page.width_px, page.height_px)?;
    if page.pixels_rgb8.len() != want {
        return Err(ScanError::Driver("page pixel buffer has the wrong length".to_owned()));
    }
    Ok((page.width_px, page.height_px))
}

/// `usize` view of a dimension, or a driver error when it does not fit.
fn dim_usize(v: u32) -> Result<usize, ScanError> {
    usize::try_from(v).map_err(|_| ScanError::Driver("page too large".to_owned()))
}

/// Read one RGB triple with fully checked offsets (never panics).
fn get_pixel(buf: &[u8], stride: usize, x: usize, y: usize) -> Result<[u8; 3], ScanError> {
    let len_err = || ScanError::Driver("page pixel buffer has the wrong length".to_owned());
    let col = x.checked_mul(3).ok_or_else(len_err)?;
    let row = y.checked_mul(stride).ok_or_else(len_err)?;
    let base = row.checked_add(col).ok_or_else(len_err)?;
    let end = base.checked_add(3).ok_or_else(len_err)?;
    let px = buf.get(base..end).ok_or_else(len_err)?;
    <[u8; 3]>::try_from(px).map_err(|_| len_err())
}

/// Mirror `index` inside `0..len` (`len - 1 - index`) with checked math.
fn mirror_index(index: usize, len: usize) -> Result<usize, ScanError> {
    len.checked_sub(1).and_then(|m| m.checked_sub(index)).ok_or_else(|| ScanError::Driver("page too large".to_owned()))
}

/// Rec.601 luma, integer math (`(299r + 587g + 114b) / 1000`, max 255).
fn luma_rec601(r: u8, g: u8, b: u8) -> u8 {
    let y = u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114;
    (y / 1000) as u8
}

fn check_level(name: &str, v: i32) -> Result<(), ScanError> {
    if !(-LEVEL_LIMIT..=LEVEL_LIMIT).contains(&v) {
        return Err(ScanError::Driver(format!("{name} out of range -1000..1000")));
    }
    Ok(())
}

fn apply_brightness(page: &mut RawPage, amount: i32) -> Result<(), ScanError> {
    check_level("brightness", amount)?;
    checked_dims(page)?;
    let offset = amount as f32 * 255.0 / 1000.0;
    for byte in page.pixels_rgb8.iter_mut() {
        *byte = (f32::from(*byte) + offset).clamp(0.0, 255.0) as u8;
    }
    Ok(())
}

fn apply_contrast(page: &mut RawPage, amount: i32) -> Result<(), ScanError> {
    check_level("contrast", amount)?;
    checked_dims(page)?;
    let factor = (1000.0_f32 + amount as f32) / 1000.0;
    for byte in page.pixels_rgb8.iter_mut() {
        *byte = ((f32::from(*byte) - 128.0) * factor + 128.0).clamp(0.0, 255.0) as u8;
    }
    Ok(())
}

fn apply_grayscale(page: &mut RawPage) -> Result<(), ScanError> {
    checked_dims(page)?;
    for px in page.pixels_rgb8.as_chunks_mut::<3>().0 {
        let y = luma_rec601(px[0], px[1], px[2]);
        px[0] = y;
        px[1] = y;
        px[2] = y;
    }
    page.bit_depth = BitDepth::Grayscale;
    Ok(())
}

fn apply_black_white(page: &mut RawPage, threshold: u8) -> Result<(), ScanError> {
    checked_dims(page)?;
    for px in page.pixels_rgb8.as_chunks_mut::<3>().0 {
        let v = if luma_rec601(px[0], px[1], px[2]) >= threshold { 255 } else { 0 };
        px[0] = v;
        px[1] = v;
        px[2] = v;
    }
    page.bit_depth = BitDepth::BlackWhite;
    Ok(())
}

fn apply_crop(page: &mut RawPage, x: u32, y: u32, w: u32, h: u32) -> Result<(), ScanError> {
    let (width, height) = checked_dims(page)?;
    if x >= width || y >= height {
        return Err(ScanError::Driver("crop origin is outside the page".to_owned()));
    }
    // `x < width` / `y < height`, so these subtractions cannot underflow.
    let cw = w.min(width - x);
    let ch = h.min(height - y);
    if cw == 0 || ch == 0 {
        return Err(ScanError::Driver("crop region is empty".to_owned()));
    }
    let len_err = || ScanError::Driver("page pixel buffer has the wrong length".to_owned());
    let src_stride = dim_usize(width)?.checked_mul(3).ok_or_else(|| ScanError::Driver("page too large".to_owned()))?;
    let x0 = dim_usize(x)?;
    let y0 = dim_usize(y)?;
    let cw_n = dim_usize(cw)?;
    let ch_n = dim_usize(ch)?;
    let row_bytes = cw_n.checked_mul(3).ok_or_else(|| ScanError::Driver("page too large".to_owned()))?;
    let out_len = row_bytes.checked_mul(ch_n).ok_or_else(|| ScanError::Driver("page too large".to_owned()))?;
    let mut out = Vec::with_capacity(out_len);
    for row in 0..ch_n {
        let sy = y0.checked_add(row).ok_or_else(len_err)?;
        let xoff = x0.checked_mul(3).ok_or_else(|| ScanError::Driver("page too large".to_owned()))?;
        let base = sy.checked_mul(src_stride).and_then(|r| r.checked_add(xoff)).ok_or_else(len_err)?;
        let end = base.checked_add(row_bytes).ok_or_else(len_err)?;
        let bytes = page.pixels_rgb8.get(base..end).ok_or_else(len_err)?;
        out.extend_from_slice(bytes);
    }
    page.pixels_rgb8 = out;
    page.width_px = cw;
    page.height_px = ch;
    Ok(())
}

/// Remap every destination pixel through `f(dst_x, dst_y) -> (src_x, src_y)`
/// and replace the page buffer. `new_w`/`new_h` are the post-op dimensions.
fn remap_page(page: &mut RawPage, new_w: u32, new_h: u32, f: impl Fn(usize, usize, usize, usize) -> Option<(usize, usize)>) -> Result<(), ScanError> {
    let (width, height) = checked_dims(page)?;
    let w = dim_usize(width)?;
    let h = dim_usize(height)?;
    let nw = dim_usize(new_w)?;
    let nh = dim_usize(new_h)?;
    let src_stride = w.checked_mul(3).ok_or_else(|| ScanError::Driver("page too large".to_owned()))?;
    let out_len = expected_rgb8_len(new_w, new_h)?;
    let mut out = Vec::with_capacity(out_len);
    for dy in 0..nh {
        for dx in 0..nw {
            let (sx, sy) = f(dx, dy, w, h).ok_or_else(|| ScanError::Driver("page too large".to_owned()))?;
            if sx >= w || sy >= h {
                return Err(ScanError::Driver("page pixel buffer has the wrong length".to_owned()));
            }
            let px = get_pixel(&page.pixels_rgb8, src_stride, sx, sy)?;
            out.extend_from_slice(&px);
        }
    }
    page.pixels_rgb8 = out;
    page.width_px = new_w;
    page.height_px = new_h;
    Ok(())
}

fn apply_rotate_cw(page: &mut RawPage) -> Result<(), ScanError> {
    let (width, height) = checked_dims(page)?;
    // Clockwise: dst(x', y') = src(y', H - 1 - x').
    remap_page(page, height, width, |dx, dy, _w, h| {
        let sx = dy;
        let mx = h.checked_sub(1)?.checked_sub(dx)?;
        Some((sx, mx))
    })
}

fn apply_rotate180(page: &mut RawPage) -> Result<(), ScanError> {
    let (width, height) = checked_dims(page)?;
    remap_page(page, width, height, |dx, dy, w, h| {
        let sx = mirror_index(dx, w).ok()?;
        let sy = mirror_index(dy, h).ok()?;
        Some((sx, sy))
    })
}

fn apply_rotate_ccw(page: &mut RawPage) -> Result<(), ScanError> {
    let (width, height) = checked_dims(page)?;
    // Counter-clockwise: dst(x', y') = src(W - 1 - y', x').
    remap_page(page, height, width, |dx, dy, w, _h| {
        let my = w.checked_sub(1)?.checked_sub(dy)?;
        Some((my, dx))
    })
}

fn apply_flip_h(page: &mut RawPage) -> Result<(), ScanError> {
    let (width, height) = checked_dims(page)?;
    remap_page(page, width, height, |dx, dy, w, _h| {
        let sx = mirror_index(dx, w).ok()?;
        Some((sx, dy))
    })
}

fn apply_flip_v(page: &mut RawPage) -> Result<(), ScanError> {
    let (width, height) = checked_dims(page)?;
    remap_page(page, width, height, |dx, dy, _w, h| {
        let sy = mirror_index(dy, h).ok()?;
        Some((dx, sy))
    })
}

fn apply_resize(page: &mut RawPage, w: u32, h: u32) -> Result<(), ScanError> {
    if w == 0 || h == 0 {
        return Err(ScanError::Driver("resize target has zero width or height".to_owned()));
    }
    if w > MAX_SIDE_PX || h > MAX_SIDE_PX {
        return Err(ScanError::Driver("resize target exceeds the 20000px limit".to_owned()));
    }
    let (width, height) = checked_dims(page)?;
    let sw = u64::from(width);
    let sh = u64::from(height);
    let dw = u64::from(w);
    let dh = u64::from(h);
    remap_page(page, w, h, |dx, dy, _w, _h| {
        let sx = (dx as u64).saturating_mul(sw).checked_div(dw)? as usize;
        let sy = (dy as u64).saturating_mul(sh).checked_div(dh)? as usize;
        Some((sx, sy))
    })
}

// ---------------------------------------------------------------------------
// Codecs
// ---------------------------------------------------------------------------

fn decode_with_format(bytes: &[u8], format: ImageFormat) -> Result<RawPage, ScanError> {
    if bytes.is_empty() {
        return Err(ScanError::Driver("empty image buffer".to_owned()));
    }
    let dyn_img = image::load_from_memory_with_format(bytes, format).map_err(|e| ScanError::Driver(e.to_string()))?;
    let w = dyn_img.width();
    let h = dyn_img.height();
    if w == 0 || h == 0 {
        return Err(ScanError::Driver("decoded image is empty".to_owned()));
    }
    if w > MAX_SIDE_PX || h > MAX_SIDE_PX {
        return Err(ScanError::Driver("decoded image exceeds the 20000px limit".to_owned()));
    }
    let pixels = dyn_img.to_rgb8().into_raw();
    let want = expected_rgb8_len(w, h)?;
    if pixels.len() != want {
        return Err(ScanError::Driver("decoded image has the wrong length".to_owned()));
    }
    Ok(RawPage { width_px: w, height_px: h, dpi: DEFAULT_DPI, bit_depth: BitDepth::Color, side: PageSide::Front, pixels_rgb8: pixels })
}

/// Decode BMP bytes to an RGB8 [`RawPage`] (dpi defaults to 200).
pub fn decode_bmp(bytes: &[u8]) -> Result<RawPage, ScanError> {
    decode_with_format(bytes, ImageFormat::Bmp)
}

/// Decode PNG bytes to an RGB8 [`RawPage`] (dpi defaults to 200).
pub fn decode_png(bytes: &[u8]) -> Result<RawPage, ScanError> {
    decode_with_format(bytes, ImageFormat::Png)
}

/// Decode JPEG bytes to an RGB8 [`RawPage`] (dpi defaults to 200).
pub fn decode_jpeg(bytes: &[u8]) -> Result<RawPage, ScanError> {
    decode_with_format(bytes, ImageFormat::Jpeg)
}

/// Reject pages the encoders cannot represent. Encode-side failures are
/// [`ScanError::Io`], matching the workspace convention that validated
/// pipeline data failing to serialize is an I/O problem.
fn validate_for_encode(page: &RawPage) -> Result<(), ScanError> {
    if page.width_px == 0 || page.height_px == 0 {
        return Err(ScanError::Io("cannot encode a zero-size page".to_owned()));
    }
    if page.width_px > MAX_SIDE_PX || page.height_px > MAX_SIDE_PX {
        return Err(ScanError::Io("page dimensions exceed the 20000px encoder limit".to_owned()));
    }
    let w = usize::try_from(page.width_px).map_err(|_| ScanError::Io("page too wide".to_owned()))?;
    let h = usize::try_from(page.height_px).map_err(|_| ScanError::Io("page too tall".to_owned()))?;
    let want = w.checked_mul(h).and_then(|n| n.checked_mul(3)).ok_or_else(|| ScanError::Io("page too large".to_owned()))?;
    if page.pixels_rgb8.len() != want {
        return Err(ScanError::Io("page pixel buffer has the wrong length".to_owned()));
    }
    Ok(())
}

fn encode_with_format(page: &RawPage, format: ImageFormat) -> Result<Vec<u8>, ScanError> {
    validate_for_encode(page)?;
    let img = RgbImage::from_raw(page.width_px, page.height_px, page.pixels_rgb8.clone())
        .ok_or_else(|| ScanError::Io("failed to wrap page pixels".to_owned()))?;
    let dyn_img = DynamicImage::ImageRgb8(img);
    let mut buf = Vec::new();
    {
        let mut cursor = Cursor::new(&mut buf);
        dyn_img.write_to(&mut cursor, format).map_err(|e| ScanError::Io(e.to_string()))?;
    }
    Ok(buf)
}

/// Encode a page as PNG bytes.
pub fn encode_png(page: &RawPage) -> Result<Vec<u8>, ScanError> {
    encode_with_format(page, ImageFormat::Png)
}

/// Encode a page as BMP bytes.
pub fn encode_bmp(page: &RawPage) -> Result<Vec<u8>, ScanError> {
    encode_with_format(page, ImageFormat::Bmp)
}

/// Encode a page as JPEG bytes with quality `1..=100` (anything else is an
/// [`ScanError::Io`]).
pub fn encode_jpeg(page: &RawPage, quality: u8) -> Result<Vec<u8>, ScanError> {
    if quality == 0 || quality > 100 {
        return Err(ScanError::Io("jpeg quality must be 1..=100".to_owned()));
    }
    validate_for_encode(page)?;
    let mut buf = Vec::new();
    {
        let mut cursor = Cursor::new(&mut buf);
        let mut enc = JpegEncoder::new_with_quality(&mut cursor, quality);
        enc.encode(&page.pixels_rgb8, page.width_px, page.height_px, ExtendedColorType::Rgb8).map_err(|e| ScanError::Io(e.to_string()))?;
    }
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

/// NAPS2-style blank-page detection: the share of near-white pixels (all
/// three channels `>= white_thresh`) is compared against `coverage_percent`.
///
/// Returns `true` when `white_pixels * 100 >= coverage_percent * total`.
/// `coverage_percent == 0` (nothing required to be white... yet vacuously
/// disabled) and values `> 100` both return `false`; an empty or malformed
/// buffer also returns `false` rather than erroring.
pub fn is_blank(page: &RawPage, white_thresh: u8, coverage_percent: u8) -> bool {
    if coverage_percent == 0 || coverage_percent > 100 {
        return false;
    }
    let total = page.pixels_rgb8.len() / 3;
    if total == 0 {
        return false;
    }
    let mut white: u64 = 0;
    for px in page.pixels_rgb8.as_chunks::<3>().0 {
        if px[0] >= white_thresh && px[1] >= white_thresh && px[2] >= white_thresh {
            white = white.saturating_add(1);
        }
    }
    white.saturating_mul(100) >= u64::from(coverage_percent).saturating_mul(total as u64)
}

/// Coarse deskew estimate: projection-profile search over `-5..=5` degrees in
/// 1-degree steps. A downscaled grayscale copy (long side <= ~200px) is
/// thresholded to dark pixels; each candidate angle rotates the dark points
/// about the centre and scores the variance (sum of squared bin counts) of
/// the row histogram. The best angle wins; ties keep the smallest magnitude
/// (starting at `0.0`), and pages with almost no dark pixels return `0.0`.
///
/// This is intentionally coarse — a sub-degree Hough / Radon pass comes
/// later. Pure safe Rust, no allocation beyond small working buffers.
pub fn estimate_deskew_angle(page: &RawPage) -> f64 {
    const ORDER: [i32; 11] = [0, -1, 1, -2, 2, -3, 3, -4, 4, -5, 5];

    let w64 = u64::from(page.width_px);
    let h64 = u64::from(page.height_px);
    if w64 == 0 || h64 == 0 {
        return 0.0;
    }
    let want = match w64.checked_mul(h64).and_then(|n| n.checked_mul(3)) {
        Some(n) => n,
        None => return 0.0,
    };
    if want == 0 || page.pixels_rgb8.len() as u64 != want {
        return 0.0;
    }
    let buf = &page.pixels_rgb8;

    // Downscale stride so the long sampled side is <= ~200px.
    let stride = page.width_px.max(page.height_px).div_ceil(200).max(1);
    let gw = page.width_px.div_ceil(stride);
    let gh = page.height_px.div_ceil(stride);
    if gw == 0 || gh == 0 {
        return 0.0;
    }

    // Collect centred coordinates of dark samples.
    let cx = f64::from(gw) / 2.0;
    let cy = f64::from(gh) / 2.0;
    let mut dark: Vec<(f64, f64)> = Vec::new();
    for gy in 0..gh {
        for gx in 0..gw {
            let sx = u64::from(gx).saturating_mul(u64::from(stride));
            let sy = u64::from(gy).saturating_mul(u64::from(stride));
            if sx >= w64 || sy >= h64 {
                continue;
            }
            let base = sy.saturating_mul(w64).saturating_add(sx).saturating_mul(3);
            if base.saturating_add(2) >= want {
                continue;
            }
            let bi = base as usize;
            let (Some(&r), Some(&g), Some(&b)) = (buf.get(bi), buf.get(bi + 1), buf.get(bi + 2)) else {
                continue;
            };
            if luma_rec601(r, g, b) < DESKEW_DARK_THRESH {
                dark.push((f64::from(gx) - cx, f64::from(gy) - cy));
            }
        }
    }
    if dark.len() < DESKEW_MIN_DARK {
        return 0.0;
    }

    // Row-histogram bins cover the full diagonal just in case.
    let span = u64::from(gw).saturating_add(u64::from(gh));
    let bins_len = match span.checked_mul(2).and_then(|n| n.checked_add(3)).and_then(|n| usize::try_from(n).ok()) {
        Some(n) if n <= 100_000 => n,
        _ => return 0.0,
    };
    let shift = span as f64 + 1.0;
    let mut bins = vec![0u32; bins_len];
    let mut best_angle = 0.0_f64;
    let mut best_score: u64 = 0;
    for a in ORDER {
        for b in bins.iter_mut() {
            *b = 0;
        }
        let rad = f64::from(a) * PI / 180.0;
        let (sin, cos) = rad.sin_cos();
        for &(x, y) in &dark {
            let y_rot = x * sin + y * cos;
            let f = y_rot + shift;
            if !f.is_finite() {
                continue;
            }
            let idx = f.floor() as i64;
            if idx >= 0 && (idx as u64) < bins_len as u64 {
                let i = idx as usize;
                if let Some(slot) = bins.get_mut(i) {
                    *slot = slot.saturating_add(1);
                }
            }
        }
        let score: u64 = bins.iter().fold(0, |acc, &c| acc.saturating_add(u64::from(c).saturating_mul(u64::from(c))));
        if score > best_score {
            best_score = score;
            best_angle = f64::from(a);
        }
    }
    best_angle
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_page(w: u32, h: u32, r: u8, g: u8, b: u8) -> RawPage {
        let n = w as usize * h as usize;
        let mut px = Vec::with_capacity(n * 3);
        for _ in 0..n {
            px.push(r);
            px.push(g);
            px.push(b);
        }
        RawPage { width_px: w, height_px: h, dpi: 200, bit_depth: BitDepth::Color, side: PageSide::Front, pixels_rgb8: px }
    }

    #[test]
    fn brightness_shifts_channels() {
        let mut p = solid_page(2, 2, 100, 100, 100);
        apply_transform(&mut p, &Transform::Brightness(200)).unwrap();
        // offset = 200 * 255 / 1000 = 51
        assert_eq!(p.pixels_rgb8, vec![151u8; 12]);
        apply_transform(&mut p, &Transform::Brightness(-1000)).unwrap();
        assert_eq!(p.pixels_rgb8, vec![0u8; 12]);
    }

    #[test]
    fn brightness_out_of_range_is_driver_error() {
        let mut p = solid_page(1, 1, 10, 10, 10);
        assert!(matches!(apply_transform(&mut p, &Transform::Brightness(1001)), Err(ScanError::Driver(_))));
        assert!(matches!(apply_transform(&mut p, &Transform::Contrast(-1001)), Err(ScanError::Driver(_))));
    }

    #[test]
    fn contrast_zero_is_identity() {
        let mut p = solid_page(2, 1, 10, 140, 250);
        let before = p.pixels_rgb8.clone();
        apply_transform(&mut p, &Transform::Contrast(0)).unwrap();
        assert_eq!(p.pixels_rgb8, before);
    }

    #[test]
    fn crop_clamps_and_rejects_out_of_bounds() {
        let mut p = solid_page(4, 4, 9, 9, 9);
        apply_transform(&mut p, &Transform::Crop { x: 1, y: 1, w: 10, h: 10 }).unwrap();
        assert_eq!((p.width_px, p.height_px), (3, 3));
        assert_eq!(p.pixels_rgb8.len(), 27);
        let mut q = solid_page(4, 4, 9, 9, 9);
        assert!(matches!(apply_transform(&mut q, &Transform::Crop { x: 4, y: 0, w: 1, h: 1 }), Err(ScanError::Driver(_))));
    }

    #[test]
    fn rotate90_twice_equals_rotate180() {
        // 2x3 page with distinct pixels so orientation mistakes show up.
        let mut px = Vec::new();
        for v in 0u8..6 {
            px.extend_from_slice(&[v, v, v]);
        }
        let base = RawPage { width_px: 2, height_px: 3, dpi: 200, bit_depth: BitDepth::Color, side: PageSide::Front, pixels_rgb8: px };
        let mut a = base.clone();
        apply_transform(&mut a, &Transform::Rotate90).unwrap();
        assert_eq!((a.width_px, a.height_px), (3, 2));
        apply_transform(&mut a, &Transform::Rotate90).unwrap();
        let mut b = base.clone();
        apply_transform(&mut b, &Transform::Rotate180).unwrap();
        assert_eq!((b.width_px, b.height_px), (2, 3));
        assert_eq!(a.pixels_rgb8, b.pixels_rgb8);
    }

    #[test]
    fn grayscale_makes_channels_equal() {
        let mut p = solid_page(2, 1, 200, 100, 50);
        apply_transform(&mut p, &Transform::Grayscale).unwrap();
        assert_eq!(p.bit_depth, BitDepth::Grayscale);
        for px in p.pixels_rgb8.as_chunks::<3>().0 {
            assert_eq!(px[0], px[1]);
            assert_eq!(px[1], px[2]);
        }
    }

    #[test]
    fn blackwhite_only_0_or_255() {
        let mut p = solid_page(2, 1, 200, 100, 50);
        apply_transform(&mut p, &Transform::BlackWhite { threshold: 128 }).unwrap();
        assert_eq!(p.bit_depth, BitDepth::BlackWhite);
        for b in &p.pixels_rgb8 {
            assert!(*b == 0 || *b == 255);
        }
    }

    #[test]
    fn blank_detection_true_false_and_edges() {
        let white = solid_page(4, 4, 255, 255, 255);
        assert!(is_blank(&white, 250, 95));
        let mut mixed = solid_page(4, 4, 255, 255, 255);
        // Darken exactly one of the 16 pixels (15/16 white = 93.75%).
        if let Some(first) = mixed.pixels_rgb8.as_chunks_mut::<3>().0.first_mut() {
            first[0] = 0;
            first[1] = 0;
            first[2] = 0;
        }
        assert!(is_blank(&mixed, 250, 90));
        assert!(!is_blank(&mixed, 250, 100));
        let dark = solid_page(4, 4, 0, 0, 0);
        assert!(!is_blank(&dark, 250, 95));
        assert!(!is_blank(&white, 250, 0));
        assert!(!is_blank(&white, 250, 101));
    }

    #[test]
    fn bmp_roundtrip() {
        let p = solid_page(3, 2, 11, 22, 33);
        let bytes = encode_bmp(&p).unwrap();
        let q = decode_bmp(&bytes).unwrap();
        assert_eq!((q.width_px, q.height_px), (3, 2));
        assert_eq!(q.pixels_rgb8, p.pixels_rgb8);
        assert_eq!(q.bit_depth, BitDepth::Color);
    }

    #[test]
    fn png_magic_bytes_and_roundtrip() {
        let p = solid_page(3, 2, 44, 55, 66);
        let bytes = encode_png(&p).unwrap();
        assert_eq!(&bytes[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
        let q = decode_png(&bytes).unwrap();
        assert_eq!(q.pixels_rgb8, p.pixels_rgb8);
    }

    #[test]
    fn jpeg_roundtrip_magic_bytes_and_dims() {
        let p = solid_page(8, 8, 120, 130, 140);
        let bytes = encode_jpeg(&p, 80).unwrap();
        assert!(bytes.len() > 2);
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
        let q = decode_jpeg(&bytes).unwrap();
        assert_eq!((q.width_px, q.height_px), (8, 8));
        assert!(matches!(encode_jpeg(&p, 0), Err(ScanError::Io(_))));
    }

    #[test]
    fn deskew_of_blank_page_is_zero() {
        let white = solid_page(32, 32, 255, 255, 255);
        assert_eq!(estimate_deskew_angle(&white), 0.0);
    }

    #[test]
    fn flip_twice_is_identity_and_resize_rejects_zero() {
        let mut p = solid_page(3, 2, 7, 8, 9);
        let before = p.pixels_rgb8.clone();
        apply_transform(&mut p, &Transform::FlipH).unwrap();
        apply_transform(&mut p, &Transform::FlipH).unwrap();
        assert_eq!(p.pixels_rgb8, before);
        apply_transform(&mut p, &Transform::FlipV).unwrap();
        apply_transform(&mut p, &Transform::FlipV).unwrap();
        assert_eq!(p.pixels_rgb8, before);
        assert!(matches!(apply_transform(&mut p, &Transform::Resize { w: 0, h: 4 }), Err(ScanError::Driver(_))));
        apply_transform(&mut p, &Transform::Resize { w: 6, h: 4 }).unwrap();
        assert_eq!((p.width_px, p.height_px), (6, 4));
        assert_eq!(p.pixels_rgb8.len(), 72);
    }
}
