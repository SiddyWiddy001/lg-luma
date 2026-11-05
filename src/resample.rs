//! One-time image resampling, used when an image file is bigger than the largest size it is
//! ever drawn at. Shrinking it before the upload means the GPU samples it one texel per screen
//! pixel instead of downscaling an oversized texture on every frame.
//!
//! The weights are computed in float - a few thousand of them, once per resample - but the
//! per-pixel accumulation is integer, because the ARM build has no vector unit and a
//! full-screen wallpaper is millions of pixels: float arithmetic per pixel would take seconds.
//! Weights are 16.16 fixed point, so a destination pixel's taps sum to 65 536.

/// Lanczos3's window: three sinc lobes either side of a destination pixel's centre.
const WINDOW: f32 = 3.;
/// Fixed-point scale of the tap weights.
const SCALE: f32 = 65_536.;

fn sinc(x: f32) -> f32 {
    if x == 0. {
        1.
    } else {
        let product = std::f32::consts::PI * x;
        product.sin() / product
    }
}

/// The Lanczos3 kernel: `sinc(x) * sinc(x / 3)`, zero outside the window.
fn kernel(x: f32) -> f32 {
    if x <= -WINDOW || x >= WINDOW {
        0.
    } else {
        sinc(x) * sinc(x / 3.)
    }
}

/// One source sample to read, and its weight.
struct Tap {
    index: usize,
    weight: i32,
}

/// The taps of a destination index. Samples outside the source clamp to the nearest edge rather
/// than being dropped, so edges neither darken nor lose a lobe, and the weights are then
/// normalised so the pixel is neither brightened nor dimmed.
fn taps(index: u32, source_len: u32, target_len: u32) -> Vec<Tap> {
    let step = source_len as f32 / target_len as f32;
    let centre = (index as f32 + 0.5) * step;
    let half = (WINDOW * step).ceil() as i64;
    let first = centre.floor() as i64 - half;
    let last = centre.floor() as i64 + half;
    // Keep the raw float weights while the lobe is summed, then round each to 16.16 once its
    // normalised share of `total` is known.
    let mut weighted = Vec::new();
    let mut total = 0.;
    for offset in first..=last {
        let weight = kernel((offset as f32 + 0.5 - centre) / step);
        if weight == 0. {
            continue;
        }
        total += weight;
        weighted.push((offset.clamp(0, i64::from(source_len) - 1) as usize, weight));
    }
    if total == 0. {
        // A source with a single sample, or a window that fell entirely between two.
        return vec![Tap {
            index: 0,
            weight: SCALE as i32,
        }];
    }
    let mut taps: Vec<Tap> = weighted
        .into_iter()
        .map(|(index, weight)| Tap {
            index,
            weight: (weight / total * SCALE).round() as i32,
        })
        .collect();
    // Rounding each weight separately can leave the taps summing to a few units short of a whole
    // pixel, which would shift a flat area down by one level: a visible banding step on a plate.
    // The difference goes to the strongest tap, so a flat colour maps back to itself exactly.
    let rounded: i32 = taps.iter().map(|tap| tap.weight).sum();
    let residual = SCALE as i32 - rounded;
    if residual != 0
        && let Some(strongest) = taps
            .iter()
            .enumerate()
            .max_by_key(|(_, tap)| tap.weight.abs())
            .map(|(index, _)| index)
    {
        taps[strongest].weight += residual;
    }
    taps
}

/// Four channels of one pixel, blended from `taps` over `pixels` where successive samples are
/// `step` bytes apart and the pixel being built starts at `offset`.
fn blend_pixel(taps: &[Tap], pixels: &[u8], step: usize, offset: usize) -> [u8; 4] {
    let mut sum = [0; 4];
    for tap in taps {
        let at = tap.index * step + offset;
        let sample = &pixels[at..at + 4];
        for (channel, total) in sum.iter_mut().enumerate() {
            *total += i32::from(sample[channel]) * tap.weight;
        }
    }
    std::array::from_fn(|channel| ((sum[channel] + SCALE as i32 / 2) >> 16).clamp(0, 255) as u8)
}

/// Rounds a cap up to the next 256 pixels. Resizing a desktop window changes the drawable
/// continuously, and without this every step of a drag would re-decode and resample the wallpaper;
/// the quantised cap only changes every 256 pixels, and the extra headroom is at most one step.
fn quantise(value: u32) -> u32 {
    value.div_ceil(256) * 256
}

/// The largest physical drawable size a window can report, used to cap uploads: an image wider
/// or taller than the screen has more texels than it is ever drawn with, and the largest size a
/// wallpaper can be displayed at is already its Ken Burns zoom (1.05) and the largest size an app
/// icon can be drawn at is already its focused card plus launch zoom. Both exceed this, so the
/// screen is the only extra factor needed.
pub fn largest_display(size: crate::raster::Size) -> (u32, u32) {
    let scale = size.scale();
    (
        quantise((1_920. * scale).ceil() as u32),
        quantise((1_080. * scale).ceil() as u32),
    )
}

/// The largest physical box an app icon or plate artwork is ever drawn in. Tiles grow to 1.05×
/// while focused, the launch zoom grows that face to 2.5×, and the plate's artwork is sampled a
/// fifth wider than the plate itself because its backdrop bleed reads past the plate's edges.
/// Widest tile face first, then those factors; icons from webOS are far below this, so this cap
/// only ever shrinks an oversized file.
pub fn largest_artwork_display(size: crate::raster::Size) -> (u32, u32) {
    let face = 270. * 1.05 * 2.5;
    let footprint = face / 0.68;
    let side = quantise((size.scale() * footprint).ceil() as u32);
    (side, side)
}

/// The size to upload a full-screen image at: its own dimensions when they already fit inside
/// `limit`, otherwise `limit` per axis. Each axis is clamped on its own rather than fitting the
/// aspect ratio, because the wallpaper is stretched to fill the screen and an aspect-preserving
/// fit would make it narrower than the screen it covers. The texture is never made bigger.
pub fn screen_upload_size(width: u32, height: u32, limit: (u32, u32)) -> (u32, u32) {
    (width.min(limit.0).max(1), height.min(limit.1).max(1))
}

/// The size to upload app artwork at: its own dimensions when they already fit inside `limit`,
/// otherwise scaled down by the same factor on both axes, because the draw code fits a texture
/// into its box by its own aspect ratio and changing that ratio would move the artwork.
pub fn artwork_upload_size(width: u32, height: u32, limit: (u32, u32)) -> (u32, u32) {
    let scale = (limit.0 as f32 / width as f32)
        .min(limit.1 as f32 / height as f32)
        .min(1.);
    if scale >= 1. {
        return (width, height);
    }
    (
        (width as f32 * scale).round().max(1.) as u32,
        (height as f32 * scale).round().max(1.) as u32,
    )
}

/// Resamples interleaved RGBA8 to `target_width` by `target_height` with Lanczos3, separably:
/// rows first, so the intermediate keeps the source width - the smaller of the two orders for a
/// landscape wallpaper. Only ever called to shrink: resampling up cannot add detail.
pub fn lanczos3_rgba(
    source: &[u8],
    width: u32,
    height: u32,
    target_width: u32,
    target_height: u32,
) -> Vec<u8> {
    let stride = width as usize * 4;
    let rows: Vec<Vec<Tap>> = (0..target_height)
        .map(|row| taps(row, height, target_height))
        .collect();
    let mut middle = vec![0; stride * target_height as usize];
    for (row, row_taps) in rows.iter().enumerate() {
        for column in 0..width as usize {
            let pixel = blend_pixel(row_taps, source, stride, column * 4);
            let at = row * stride + column * 4;
            middle[at..at + 4].copy_from_slice(&pixel);
        }
    }
    // The column taps are the same for every row, so they are computed once. Here successive
    // samples are neighbouring pixels in one row, four bytes apart, and the tap index already
    // names the source column, so the offset is only the row's start.
    let columns: Vec<Vec<Tap>> = (0..target_width)
        .map(|column| taps(column, width, target_width))
        .collect();
    let target_stride = target_width as usize * 4;
    let mut out = vec![0; target_stride * target_height as usize];
    for row in 0..target_height as usize {
        for (column, taps) in columns.iter().enumerate() {
            let pixel = blend_pixel(taps, &middle, 4, row * stride);
            let at = row * target_stride + column * 4;
            out[at..at + 4].copy_from_slice(&pixel);
        }
    }
    out
}

#[cfg(test)]
#[path = "resample-tests.rs"]
mod tests;
