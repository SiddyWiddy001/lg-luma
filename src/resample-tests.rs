//! Unit tests for the one-time Lanczos resample that sizes wallpapers and oversized icons.

use super::*;

/// A flat colour has to come back out exactly: rounding the tap weights so they no longer sum to
/// a whole pixel would shift every surface by one level, which bands on a large plate.
fn flat(width: u32, height: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..width * height {
        pixels.extend_from_slice(&[128, 61, 199, 128]);
    }
    pixels
}

/// Each destination pixel's taps must sum to exactly one whole pixel in 16.16 fixed point.
#[test]
fn tap_weights_sum_to_one_pixel() {
    for (source, target) in [(2_560, 2_016), (1_707, 1_134), (512, 512), (3, 2), (1, 1)] {
        for index in 0..target {
            let sum: i32 = taps(index, source, target).iter().map(|tap| tap.weight).sum();
            assert_eq!(sum, SCALE as i32, "{source} -> {target} at {index}");
        }
    }
}

#[test]
fn resampling_a_flat_colour_keeps_it_exact() {
    for (source, target) in [(8, 8), (8, 5), (5, 8), (16, 1), (1, 16)] {
        let out = lanczos3_rgba(&flat(source.0, source.1), source.0, source.1, target.0, target.1);
        assert_eq!(out.len(), (target.0 * target.1 * 4) as usize);
        let shifted = out.chunks(4).find(|pixel| *pixel != [128, 61, 199, 128]);
        assert_eq!(shifted, None, "{source:?} -> {target:?} shifted a flat colour");
    }
}

/// Resampling an image to its own size must be a no-op, so an image that already fits is
/// untouched and a mismatched axis alone never resamples the other.
#[test]
fn identity_resampling_changes_nothing() {
    let pixels = flat(6, 4);
    assert_eq!(lanczos3_rgba(&pixels, 6, 4, 6, 4), pixels);
}

#[test]
fn screen_uploads_never_grow_an_image() {
    assert_eq!(screen_upload_size(2_560, 1_707, (2_048, 1_152)), (2_048, 1_152));
    assert_eq!(screen_upload_size(900, 600, (2_048, 1_152)), (900, 600));
    // One axis over the limit still leaves the other alone: the wallpaper is stretched to fill,
    // so clamping the axes separately is what keeps it covering the screen.
    assert_eq!(screen_upload_size(4_000, 600, (2_048, 1_152)), (2_048, 600));
    assert_eq!(screen_upload_size(4_000, 600, (0, 0)), (1, 1));
}

#[test]
fn artwork_uploads_keep_their_aspect_ratio_when_shrunk() {
    let (width, height) = artwork_upload_size(2_048, 1_024, (1_024, 1_024));
    assert_eq!((width, height), (1_024, 512));
    assert_eq!(artwork_upload_size(256, 256, (1_024, 1_024)), (256, 256));
    // Never scaled up, and never rounded away to nothing.
    assert_eq!(artwork_upload_size(1_024, 1_024, (1_200, 1_200)), (1_024, 1_024));
    assert_eq!(artwork_upload_size(8_192, 1, (1_024, 1_024)), (1_024, 1));
}

/// The caps the renderer uses must be at least as large as anything the UI draws an image at.
#[test]
fn caps_cover_the_largest_displayed_size() {
    let screen = largest_display(crate::raster::Size::HD);
    assert!(screen.0 >= 1_920 && screen.1 >= 1_080, "{screen:?}");
    // The wallpaper at its deepest Ken Burns zoom, and the launch zoom on the widest tile face.
    let zoomed = (1_920. * 1.05) as u32;
    assert!(screen.0 >= zoomed, "{screen:?} below {zoomed}");
    let artwork = largest_artwork_display(crate::raster::Size::HD);
    let launch = (270. * 1.05 * 2.5 / 0.68) as u32;
    assert!(artwork.0 >= launch, "{artwork:?} below {launch}");
}
