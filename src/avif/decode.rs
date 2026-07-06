//! dav1d decoding and container probing (avif-parse for stills,
//! the isobmff walker as the animated fallback).

use super::*;

/// dav1d reports errors as negative errno values.
#[cfg(target_os = "linux")]
pub(super) const EAGAIN: std::os::raw::c_int = 11;
#[cfg(not(target_os = "linux"))]
pub(super) const EAGAIN: std::os::raw::c_int = 35;

thread_local! {
    /// Set while the deliberately unwind-caught avif-parse call runs, so
    /// the filtering panic hook stays silent for it: without this, every
    /// attacker-supplied malformed AVIF would print a crash-shaped trace
    /// (and, under RUST_BACKTRACE, serialize on the global backtrace
    /// lock) even though the request fails cleanly.
    static SUPPRESS_PANIC_LOG: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Install (once) a panic hook that skips logging for panics this
/// module catches on purpose and delegates to the previous hook for
/// everything else.
pub(super) fn install_quiet_panic_hook() {
    static HOOK: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    HOOK.get_or_init(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !SUPPRESS_PANIC_LOG.with(|s| s.get()) {
                prev(info);
            }
        }));
    });
}

/// avif-parse can panic on truncated/malformed containers (internal
/// parser-state assertions, observed in 2.1.0); malformed input must
/// surface as a parse error, not a crash, so the call is unwind-caught.
/// The crate is pure Rust and the input is a shared slice, so no state
/// can be left torn.
pub(super) fn read_avif_container(data: &[u8]) -> Result<avif_parse::AvifData> {
    install_quiet_panic_hook();
    struct Unsuppress;
    impl Drop for Unsuppress {
        fn drop(&mut self) {
            SUPPRESS_PANIC_LOG.with(|s| s.set(false));
        }
    }
    SUPPRESS_PANIC_LOG.with(|s| s.set(true));
    let _guard = Unsuppress;
    match std::panic::catch_unwind(|| avif_parse::read_avif(&mut std::io::Cursor::new(data))) {
        Ok(parsed) => parsed.context("parse AVIF container"),
        // Keep the assertion text: it identifies which upstream bug fired.
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic payload");
            Err(anyhow::anyhow!("AVIF container parse panicked: {msg}"))
        }
    }
}

// ---------------------------------------------------------------------
// ICC (`colr` of type `prof`) support. Neither avif-parse (2.1.0) nor
// avif-serialize (0.8.9) exposes ICC in any released version, so both
// directions run on a bounded ISOBMFF walk of our own: extraction
// resolves the primary item's property associations; embedding splices
// a `colr` property into an avif-serialize container and patches the
// affected box sizes and item locations, then proves the result by
/// Container-level probe: dimensions from the primary item's AV1
/// sequence header, no pixel decoding. Animated containers fall back
/// to the still primary item's `ispe`.
pub fn probe_avif(data: &[u8]) -> Result<(usize, usize)> {
    let avif = match read_avif_container(data) {
        Ok(a) => a,
        Err(e) => {
            if is_animated_brand(data)
                && let Some(dims) = primary_ispe(data)
            {
                return Ok(dims);
            }
            return Err(e);
        }
    };
    let meta = avif
        .primary_item_metadata()
        .context("parse AV1 sequence header")?;
    Ok((
        meta.max_frame_width.get() as usize,
        meta.max_frame_height.get() as usize,
    ))
}

/// Decode an AVIF file via dav1d to interleaved RGB8 or, when the file
/// carries an alpha auxiliary image, straight-alpha RGBA8. Handles
/// 4:0:0/4:2:0/4:2:2/4:4:4 at 8/10/12 bits, identity/BT.601/BT.709/
/// BT.2020-NCL matrices, and both color ranges; premultiplied alpha is
/// converted to straight alpha. Returns (pixels, width, height, channels).
pub fn decode_avif(data: &[u8]) -> Result<(Vec<u8>, usize, usize, usize)> {
    let mut out = Vec::new();
    let (w, h, channels) = decode_avif_into(data, &mut out)?;
    Ok((out, w, h, channels))
}

/// Like [`decode_avif`], but reuses `out` as the pixel buffer.
pub fn decode_avif_into(data: &[u8], out: &mut Vec<u8>) -> Result<(usize, usize, usize)> {
    let avif = match read_avif_container(data) {
        Ok(a) => a,
        Err(e) => {
            // Animated AVIF: avif-parse rejects sequences outright, but
            // MIAF requires them to carry a valid still primary item —
            // decode that (first-frame rendering, like other image
            // proxies). Alpha tracks are not decoded.
            if is_animated_brand(data)
                && let Some(item) = primary_item_bytes(data)
            {
                let (w, h) = with_decoded_picture(&item, |pic| picture_to_rgb(pic, out))?;
                return Ok((w, h, 3));
            }
            return Err(e);
        }
    };
    let (w, h) = with_decoded_picture(&avif.primary_item, |pic| picture_to_rgb(pic, out))?;
    let Some(alpha_item) = avif.alpha_item.as_deref() else {
        return Ok((w, h, 3));
    };

    let alpha = with_decoded_picture(alpha_item, |pic| picture_to_alpha(pic, w, h))
        .context("decode alpha item")?;
    // Expand RGB to RGBA in place, back to front (writes at i*4.. never
    // overlap reads at j*3..j*3+3 for j < i); every output position is
    // written, so growth does not need to re-zero.
    if out.len() < w * h * 4 {
        out.resize(w * h * 4, 0);
    }
    out.truncate(w * h * 4);
    for i in (0..w * h).rev() {
        let (r, g, b) = (out[i * 3], out[i * 3 + 1], out[i * 3 + 2]);
        let a = alpha[i];
        let (r, g, b) = if avif.premultiplied_alpha && a != 255 {
            if a == 0 {
                (0, 0, 0)
            } else {
                let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
                (un(r), un(g), un(b))
            }
        } else {
            (r, g, b)
        };
        out[i * 4] = r;
        out[i * 4 + 1] = g;
        out[i * 4 + 2] = b;
        out[i * 4 + 3] = a;
    }
    Ok((w, h, 4))
}

/// dav1d worker threads. The default is architecture-aware: on x86-64,
/// two threads ride the second SMT sibling of the request's core (the
/// same rationale as libwebp's two-thread decode, which libvips also
/// ships), improving both latency and saturated throughput. aarch64
/// server cores have no SMT, so a second thread costs a full core:
/// wall latency improves at light load but saturated throughput drops
/// (measured -6% requests/s on Graviton3) — single-threaded decoding
/// is the default there. OXIMG_AVIF_DECODE_THREADS overrides.
pub(super) fn dav1d_threads() -> std::os::raw::c_int {
    crate::config::config().avif_decode_threads
}

/// Run one dav1d session over a single-frame AV1 stream and hand the
/// decoded picture to `f`.
// SAFETY: FFI cluster. `settings`/`data`/`pic` are zeroed C structs passed as
// live stack locals (dav1d_default_settings fills `settings` before use); the
// wrapped `av1` borrow outlives every decoder reference to it — all refs drop
// via the guards below, in reverse declaration order, before this fn returns.
pub(super) fn with_decoded_picture<T>(
    av1: &[u8],
    f: impl FnOnce(&dav1d_sys::Dav1dPicture) -> Result<T>,
) -> Result<T> {
    use dav1d_sys as d;
    unsafe {
        let mut settings: d::Dav1dSettings = std::mem::zeroed();
        d::dav1d_default_settings(&mut settings);
        settings.n_threads = dav1d_threads();
        settings.max_frame_delay = 1;

        let mut ctx: *mut d::Dav1dContext = std::ptr::null_mut();
        ensure!(d::dav1d_open(&mut ctx, &settings) == 0, "dav1d_open");
        struct Ctx(*mut d::Dav1dContext);
        impl Drop for Ctx {
            fn drop(&mut self) {
                // SAFETY: self.0 was returned by a successful dav1d_open (checked above) and
                // is closed exactly once — the guard is a local that never escapes.
                unsafe { d::dav1d_close(&mut self.0) }
            }
        }
        let _ctx_guard = Ctx(ctx);

        // Borrow the OBU buffer; it outlives the decoder, so the free
        // callback (which dav1d requires to be non-null) is a no-op.
        // SAFETY: nothing to uphold — deliberately a no-op; the wrapped bytes are a
        // Rust borrow released by its owner, never by dav1d.
        unsafe extern "C" fn no_free(_buf: *const u8, _cookie: *mut std::ffi::c_void) {}
        let mut data: d::Dav1dData = std::mem::zeroed();
        ensure!(
            d::dav1d_data_wrap(
                &mut data,
                av1.as_ptr(),
                av1.len(),
                Some(no_free),
                std::ptr::null_mut()
            ) == 0,
            "dav1d_data_wrap"
        );
        struct Data(*mut d::Dav1dData);
        impl Drop for Data {
            fn drop(&mut self) {
                // SAFETY: self.0 points at the enclosing fn's `data`, a stack local this guard
                // (declared after it) predeceases. dav1d empties `data` (nulling .data) once
                // send_data fully consumes it, so the check unrefs a held reference exactly once.
                unsafe {
                    if !(*self.0).data.is_null() {
                        d::dav1d_data_unref(self.0);
                    }
                }
            }
        }
        let _data_guard = Data(&mut data);

        let mut pic: d::Dav1dPicture = std::mem::zeroed();
        loop {
            if data.sz > 0 {
                let res = d::dav1d_send_data(ctx, &mut data);
                ensure!(res == 0 || res == -EAGAIN, "dav1d_send_data: {res}");
            }
            let res = d::dav1d_get_picture(ctx, &mut pic);
            if res == 0 {
                break;
            }
            ensure!(
                res == -EAGAIN && data.sz > 0,
                "dav1d_get_picture: {res} (no picture in stream)"
            );
        }
        struct Pic(*mut d::Dav1dPicture);
        impl Drop for Pic {
            fn drop(&mut self) {
                // SAFETY: this guard is created only after dav1d_get_picture returned 0, so `pic`
                // (a stack local that outlives the guard) holds a real reference; unref runs once.
                unsafe { d::dav1d_picture_unref(self.0) }
            }
        }
        let _pic_guard = Pic(&mut pic);

        f(&pic)
    }
}

/// Extract the alpha plane (the luma of the auxiliary image) as u8.
// SAFETY (`seq_hdr` deref): `pic` is always a picture filled by a successful
// dav1d_get_picture (with_decoded_picture is this module's only producer), which
// sets `seq_hdr` non-null and keeps the header alive for the picture's lifetime.
pub(super) fn picture_to_alpha(
    pic: &dav1d_sys::Dav1dPicture,
    w: usize,
    h: usize,
) -> Result<Vec<u8>> {
    ensure!(
        (pic.p.w as usize, pic.p.h as usize) == (w, h),
        "alpha dimensions do not match the color image"
    );
    let bpc = pic.p.bpc as u32;
    ensure!(matches!(bpc, 8 | 10 | 12), "unsupported bit depth {bpc}");
    let seq = unsafe { &*pic.seq_hdr };
    // The spec requires full range for alpha; honor the flag regardless.
    let max = ((1u32 << bpc) - 1) as f32;
    let scale8 = (1u32 << (bpc - 8)) as f32;
    let (a_mul, a_off) = if seq.color_range != 0 {
        (255.0 / max, 0.0)
    } else {
        (255.0 / (219.0 * scale8), 16.0 * scale8)
    };

    let hbd = bpc > 8;
    let mut alpha = vec![0u8; w * h];
    for y in 0..h {
        let row = &mut alpha[y * w..(y + 1) * w];
        let src = plane_row(pic, 0, y, w, hbd);
        match src {
            Row::B8(s) if seq.color_range != 0 => row.copy_from_slice(s),
            src => yuv::alpha_row(src, a_off, a_mul, row),
        }
    }
    Ok(alpha)
}

/// Borrow one row of a dav1d plane as typed samples. Plane 0 uses the
/// luma stride; planes 1/2 share the chroma stride. Strides are bytes.
// SAFETY (raw row borrow): callers must derive `y`/`len` from pic's own dims and
// layout (both callers do) so ptr + y*stride + len stays inside the plane, with
// hbd == (bpc > 8). dav1d's default allocator — in use, the settings are defaults —
// gives 64-byte-aligned planes and even byte strides at >8 bpc, so the u16 cast
// and stride/2 are exact.
pub(super) fn plane_row(
    pic: &dav1d_sys::Dav1dPicture,
    plane: usize,
    y: usize,
    len: usize,
    hbd: bool,
) -> Row<'_> {
    let (ptr, stride) = if plane == 0 {
        (pic.data[0], pic.stride[0] as usize)
    } else {
        (pic.data[plane], pic.stride[1] as usize)
    };
    unsafe {
        if hbd {
            Row::B16(std::slice::from_raw_parts(
                (ptr as *const u16).add(y * (stride / 2)),
                len,
            ))
        } else {
            Row::B8(std::slice::from_raw_parts(
                (ptr as *const u8).add(y * stride),
                len,
            ))
        }
    }
}

/// Convert a decoded dav1d picture (planar YUV) to interleaved RGB8.
// SAFETY (`seq_hdr` deref): same argument as picture_to_alpha — `pic` comes from
// a successful dav1d_get_picture, which keeps a non-null `seq_hdr` alive for the
// picture's lifetime.
pub(super) fn picture_to_rgb(
    pic: &dav1d_sys::Dav1dPicture,
    out: &mut Vec<u8>,
) -> Result<(usize, usize)> {
    use dav1d_sys as d;
    let (w, h) = (pic.p.w as usize, pic.p.h as usize);
    let bpc = pic.p.bpc as u32;
    ensure!(matches!(bpc, 8 | 10 | 12), "unsupported bit depth {bpc}");
    let seq = unsafe { &*pic.seq_hdr };
    let full_range = seq.color_range != 0;
    let monochrome = pic.p.layout == d::DAV1D_PIXEL_LAYOUT_I400;
    let (sx, sy) = match pic.p.layout {
        d::DAV1D_PIXEL_LAYOUT_I420 => (1u32, 1u32),
        d::DAV1D_PIXEL_LAYOUT_I422 => (1, 0),
        _ => (0, 0),
    };

    let hbd = bpc > 8;
    let max = ((1u32 << bpc) - 1) as f32;
    let center = ((1u32 << bpc) / 2) as f32;
    let scale8 = (1u32 << (bpc - 8)) as f32;
    // Normalize to the 0..255 scale: full range divides by the sample
    // maximum; limited range maps 16..235 (luma) / 16..240 (chroma).
    let (y_mul, y_off, c_mul) = if full_range {
        (255.0 / max, 0.0, 255.0 / max)
    } else {
        (
            255.0 / (219.0 * scale8),
            16.0 * scale8,
            255.0 / (224.0 * scale8),
        )
    };

    // Matrix coefficients from the sequence header. Unspecified and exotic
    // matrices fall back to BT.601 so a slightly mistagged file still
    // serves a reasonable image instead of a 5xx.
    let identity = seq.mtrx == 0 && !monochrome;
    if identity {
        ensure!(
            pic.p.layout == d::DAV1D_PIXEL_LAYOUT_I444,
            "identity matrix requires 4:4:4"
        );
    }
    let (kr, kb) = match seq.mtrx {
        1 => (0.2126, 0.0722),     // BT.709
        9 => (0.2627, 0.0593),     // BT.2020 NCL
        _ => (0.299f32, 0.114f32), // BT.601 and fallback
    };
    let kg = 1.0 - kr - kb;

    // Subsampled chroma is upsampled with the separable center-sited
    // bilinear kernel (9:3:3:1) that libyuv and libjpeg's "fancy
    // upsampling" use, so output matches avifdec instead of showing
    // nearest-neighbor chroma blocking.
    let cw = if sx == 1 { w.div_ceil(2) } else { w };
    let ch = if sy == 1 { h.div_ceil(2) } else { h };
    let mut cb_mid = vec![0f32; cw];
    let mut cr_mid = vec![0f32; cw];
    let mut cb_row = vec![0f32; w];
    let mut cr_row = vec![0f32; w];

    out.clear();
    out.resize(w * h * 3, 0);
    let csc = yuv::Csc {
        y_off,
        y_mul,
        center,
        c_mul,
        kr,
        kb,
        kg,
    };
    for y in 0..h {
        if !monochrome {
            // Vertical pass at chroma horizontal resolution.
            let (near, other) = if sy == 1 {
                let near = y >> 1;
                let other = if y & 1 == 1 {
                    (near + 1).min(ch - 1)
                } else {
                    near.saturating_sub(1)
                };
                (near, other)
            } else {
                (y, y)
            };
            for (plane, mid) in [(1usize, &mut cb_mid), (2, &mut cr_mid)] {
                if sy == 1 {
                    yuv::chroma_blend(
                        plane_row(pic, plane, near, cw, hbd),
                        plane_row(pic, plane, other, cw, hbd),
                        mid,
                    );
                } else {
                    yuv::chroma_widen(plane_row(pic, plane, near, cw, hbd), mid);
                }
            }
            // Horizontal pass to full resolution.
            if sx == 1 {
                yuv::chroma_upsample_h(&cb_mid, &mut cb_row);
                yuv::chroma_upsample_h(&cr_mid, &mut cr_row);
            } else {
                cb_row.copy_from_slice(&cb_mid);
                cr_row.copy_from_slice(&cr_mid);
            }
        }

        let y_row = plane_row(pic, 0, y, w, hbd);
        let row = &mut out[y * w * 3..(y + 1) * w * 3];
        if !monochrome && !identity {
            yuv::yuv_row_to_rgb(y_row, &cb_row, &cr_row, &csc, row);
            continue;
        }
        for (x, px) in row.chunks_exact_mut(3).enumerate() {
            let yf = (y_row.at(x) - y_off) * y_mul;
            let (r, g, b) = if monochrome {
                (yf, yf, yf)
            } else {
                // Identity: G=Y, B=U, R=V, chroma is not centered.
                (cr_row[x] * y_mul, yf, cb_row[x] * y_mul)
            };
            px[0] = (r + 0.5).clamp(0.0, 255.0) as u8;
            px[1] = (g + 0.5).clamp(0.0, 255.0) as u8;
            px[2] = (b + 0.5).clamp(0.0, 255.0) as u8;
        }
    }
    out.truncate(w * h * 3);
    Ok((w, h))
}
