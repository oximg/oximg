//! CMYK→RGB conversion for 4-component JPEG sources (print-workflow
//! assets). libjpeg hands back the stored samples, which are
//! Adobe-inverted (0 = full ink); with a parseable CMYK-class ICC
//! profile the conversion is color-managed through moxcms, otherwise
//! it falls back to the browser-standard naive composite.

use super::*;

/// Compact the raw CMYK samples in `chunk8[..w*h*4]` to RGB in place.
/// Pixel i writes 3i..3i+3 after reading 4i..4i+4, so the forward
/// pass never clobbers unread input (the same in-place trick as
/// `flatten_alpha_in_out8`; the ICC path's row writes end at
/// 3·(y+1)·w ≤ 4·(y+1)·w, below every unread row for the same
/// reason).
///
/// With an embedded CMYK-class profile the pixels go through moxcms
/// at relative colorimetric — the vips/imgproxy default intent, and
/// effectively moxcms's only one (it applies no intent-specific PCS
/// adjustments); measured within max|Δ|=2 of an lcms2 ground truth
/// on a real print profile. A profile that fails to parse, is not
/// CMYK-class, or refuses a transform falls back to the naive
/// composite `r = c'·k'/255` on the inverted samples — the rendering
/// Chrome, Firefox, and djpeg use for profile-less CMYK, +127-rounded
/// to match djpeg byte-for-byte.
pub(super) fn cmyk_to_rgb_in_chunk8(s: &mut Scratch, w: usize, h: usize, icc: Option<&[u8]>) {
    if let Some(profile) = icc
        && icc_convert_in_chunk8(s, w, h, profile)
    {
        return;
    }
    for i in 0..w * h {
        naive_px(&mut s.chunk8, i);
    }
}

/// One pixel of the naive composite (reads 4i..4i+4, writes 3i..3i+3).
fn naive_px(chunk8: &mut [u8], i: usize) {
    let px: [u8; 4] = chunk8[i * 4..i * 4 + 4].try_into().unwrap();
    let k = px[3] as u32;
    for (c, &v) in px[..3].iter().enumerate() {
        chunk8[i * 3 + c] = ((v as u32 * k + 127) / 255) as u8;
    }
}

/// Color-managed CMYK→sRGB through the embedded profile. `false`
/// means "not attempted" — nothing was written and the caller runs
/// the naive pass instead. Row buffers stage the transform because
/// moxcms needs separate input/output slices and true-ink samples
/// (255 − stored); per-request transform creation is fine for
/// sources this rare.
fn icc_convert_in_chunk8(s: &mut Scratch, w: usize, h: usize, profile: &[u8]) -> bool {
    let Ok(src) = moxcms::ColorProfile::new_from_slice(profile) else {
        return false;
    };
    // Never feed a mislabeled (non-CMYK-class) profile to a 4-channel
    // transform; an RGB profile on a CMYK source is bad metadata, not
    // a conversion recipe.
    if src.color_space != moxcms::DataColorSpace::Cmyk {
        return false;
    }
    let opts = moxcms::TransformOptions {
        rendering_intent: moxcms::RenderingIntent::RelativeColorimetric,
        ..Default::default()
    };
    // CMYK8 shares RGBA8's interleaved 4-channel layout in moxcms.
    let Ok(transform) = src.create_transform_8bit(
        moxcms::Layout::Rgba,
        &moxcms::ColorProfile::new_srgb(),
        moxcms::Layout::Rgb,
        opts,
    ) else {
        return false;
    };
    let mut ink = vec![0u8; w * 4];
    let mut rgb = vec![0u8; w * 3];
    for y in 0..h {
        for (d, &v) in ink.iter_mut().zip(&s.chunk8[y * w * 4..(y + 1) * w * 4]) {
            *d = 255 - v;
        }
        if transform.transform(&ink, &mut rgb).is_ok() {
            s.chunk8[y * w * 3..(y + 1) * w * 3].copy_from_slice(&rgb);
        } else {
            // Unreachable by construction (both slices are exactly
            // sized) — keep the row consistent instead of poisoning
            // the frame if moxcms ever grows another error path.
            for i in y * w..(y + 1) * w {
                naive_px(&mut s.chunk8, i);
            }
        }
    }
    true
}
