//! The fused decode-overlap workers: decode ∥ resize (plus the
//! incremental encode, the YUV conversion, or the session preheat,
//! per variant) on a scoped worker thread — all byte-identical to
//! their serial fallbacks.

use super::*;

/// The SIMD row kernel driving the fused path on this architecture.
#[cfg(target_arch = "aarch64")]
pub(super) type FuseKernel = crate::resize_neon::Neon;
#[cfg(target_arch = "x86_64")]
pub(super) type FuseKernel = crate::resize_avx2::Avx2;

/// The fused JPEG fast path: this (request) thread keeps the decoder at
/// its serial-decode floor while a worker thread converts each decoded
/// chunk to linear u16, streams it through the row-push resize kernel,
/// and feeds finished rows to an incremental jpegli encoder. Everything
/// downstream of the decoder hides behind the decode wall; the only
/// serial tail left is jpegli's entropy pass in `finish`.
///
/// Returns Ok(None) — with the decoder untouched — when no SIMD row
/// kernel exists for this CPU, so the caller falls back to the serial
/// path. Output bytes are identical to the serial jpegli path: the same
/// kernel produces the same u16 rows (streamed emission is bit-identical
/// to the full-frame schedule), and jpegli is deterministic for the same
/// scanlines and settings regardless of write granularity.
#[cfg_attr(
    not(any(target_arch = "aarch64", target_arch = "x86_64")),
    allow(unused_variables)
)]
/// On success returns the encoded bytes and the wall milliseconds the
/// decode loop took on this thread (the fused pipeline's floor).
pub(super) fn fused_resize_encode<R: std::io::BufRead>(
    started: &mut mozjpeg::decompress::DecompressStarted<R>,
    dec_w: usize,
    dec_h: usize,
    dst_w: usize,
    dst_h: usize,
    quality: f32,
    icc: Option<&[u8]>,
) -> Result<Option<(Vec<u8>, f64)>> {
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        Ok(None)
    }
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    {
        let Ok(mut resizer) =
            crate::resize_kernel::StreamResize::<FuseKernel>::new(dec_w, dec_h, dst_w, dst_h, 3)
        else {
            return Ok(None);
        };

        let row_bytes = dec_w * 3;
        // Smaller chunks than the serial path's 256KB: granularity here
        // sets the post-decode tail (the last chunk's convert+resize+
        // encode work cannot hide behind the decode), and per-chunk
        // handoff costs are ~µs.
        let chunk_rows = (64 * 1024 / row_bytes).clamp(1, dec_h);
        // Decoded chunks flow A -> B; drained buffers flow back for reuse.
        // Capacity 2 gives the decoder one chunk of runway without letting
        // buffers pile up.
        let (chunk_tx, chunk_rx) = std::sync::mpsc::sync_channel::<(Vec<u8>, usize)>(2);
        let (recycle_tx, recycle_rx) = std::sync::mpsc::channel::<Vec<u8>>();

        std::thread::scope(|sc| -> Result<Option<(Vec<u8>, f64)>> {
            // Borrowed, not moved: the resizer's Drop must run on this
            // long-lived blocking-pool thread so its kernel scratch
            // returns to this thread's pool instead of dying with the
            // ephemeral worker's TLS.
            let resizer = &mut resizer;
            let spawned = std::thread::Builder::new()
                .name("oximg-fuse".into())
                .spawn_scoped(sc, move || -> Result<Vec<u8>> {
                    let fwd = fwd_lut_f32();
                    let back = back_lut();
                    let mut row8 = vec![0u8; dst_w * 3];

                    let mut comp = jpegli::Compress::new(jpegli::ColorSpace::JCS_RGB);
                    comp.set_size(dst_w, dst_h);
                    comp.set_quality(quality);
                    // Mirrors encode_jpegli (including the progressive knob).
                    if jpegli_progressive() {
                        comp.set_progressive_mode();
                    }
                    let mut enc = comp.start_compress(Vec::with_capacity(64 * 1024))?;
                    // Same chunker, same position as encode_jpegli: the
                    // profile precedes the scanlines, so fused output
                    // stays byte-identical to the serial encoder.
                    if let Some(icc) = icc {
                        for chunk in icc_app2_chunks(icc) {
                            enc.write_marker(jpegli::Marker::APP(2), &chunk);
                        }
                    }

                    while let Ok((buf, rows)) = chunk_rx.recv() {
                        for r in 0..rows {
                            let src = &buf[r * row_bytes..(r + 1) * row_bytes];
                            let mut enc_result = Ok(());
                            resizer.push_row_u8(src, fwd, |_, out| {
                                for (d, &v) in row8.iter_mut().zip(out) {
                                    *d = back[v as usize];
                                }
                                if enc_result.is_ok() {
                                    enc_result = enc.write_scanlines(&row8);
                                }
                            });
                            enc_result.context("fused encode failed")?;
                        }
                        let _ = recycle_tx.send(buf);
                    }
                    // Channel closed: either the decoder delivered everything
                    // or it failed mid-image; only a complete image may be
                    // finished into a JPEG.
                    anyhow::ensure!(
                        resizer.rows_emitted() == dst_h,
                        "decode ended before the image was complete"
                    );
                    enc.finish().context("fused encode finish failed")
                });
            // Spawn failure (thread limits, transient EAGAIN) leaves the
            // decoder untouched, exactly like a missing kernel — fall
            // back to the byte-identical serial path instead of failing.
            let Ok(worker) = spawned else {
                return Ok(None);
            };

            // Decode loop on the request thread: read a chunk, hand it to
            // the worker, reuse buffers the worker has drained.
            let t_decode = std::time::Instant::now();
            let decode_result = (|| -> Result<()> {
                let mut remaining = dec_h;
                while remaining > 0 {
                    let mut buf = recycle_rx.try_recv().unwrap_or_default();
                    let want = remaining.min(chunk_rows) * row_bytes;
                    if buf.len() < want {
                        buf.resize(want, 0);
                    }
                    let got = started
                        .read_scanlines_into(&mut buf[..want])
                        .context("decode failed")?
                        .len();
                    anyhow::ensure!(
                        got > 0 && got % row_bytes == 0,
                        "decoder returned a partial row"
                    );
                    let rows = got / row_bytes;
                    remaining -= rows;
                    if chunk_tx.send((buf, rows)).is_err() {
                        // Worker died; its join below reports the real error.
                        anyhow::bail!("fuse worker exited early");
                    }
                }
                Ok(())
            })();
            let decode_ms = t_decode.elapsed().as_secs_f64() * 1e3;
            drop(chunk_tx);

            let encoded = worker
                .join()
                .map_err(|_| anyhow::anyhow!("fuse worker panicked"))?;
            // A decode error is the root cause; report it over the worker's
            // consequent "incomplete image" error.
            decode_result?;
            Ok(Some((encoded?, decode_ms)))
        })
    }
}

/// The cross-format sibling of [`fused_resize_encode`]: the request
/// thread runs the same decode loop while a scoped worker streams rows
/// through the SIMD kernel straight into `out8` — the exact writes the
/// serial path performs inline, so pixels are byte-identical to it.
/// The (one-shot) target encoder runs after, on the request thread;
/// only the encode stays outside the decode wall, which is as much
/// overlap as WebP/AVIF/PNG's full-frame encode APIs allow.
///
/// Returns Ok(None) — decoder untouched — when no SIMD row kernel
/// exists for this CPU; on success returns the decode-loop wall
/// milliseconds, with `out8` fully written.
#[cfg_attr(
    not(any(target_arch = "aarch64", target_arch = "x86_64")),
    allow(unused_variables)
)]
#[allow(clippy::too_many_arguments)]
pub(super) fn fused_resize_pixels<R: std::io::BufRead, T: Send>(
    started: &mut mozjpeg::decompress::DecompressStarted<R>,
    dec_w: usize,
    dec_h: usize,
    dst_w: usize,
    dst_h: usize,
    out8: &mut [u8],
    // Chunk-channel capacity: 2 suffices when the worker starts
    // resizing immediately; callers whose side task occupies the
    // worker first (session preheat, ~1ms) pass 4 so the decoder keeps
    // running through that window, mirroring fused_resize_yuv.
    runway: usize,
    // Runs on the worker before the resize loop — extra setup (e.g.
    // the oriented-AVIF session preheat) that should hide behind the
    // decode wall alongside the resize.
    side: impl FnOnce() -> Result<T> + Send,
) -> Result<Option<(f64, T)>> {
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = side;
        Ok(None)
    }
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    {
        let Ok(mut resizer) =
            crate::resize_kernel::StreamResize::<FuseKernel>::new(dec_w, dec_h, dst_w, dst_h, 3)
        else {
            return Ok(None);
        };

        let row_bytes = dec_w * 3;
        // Chunking and channel shapes mirror fused_resize_encode.
        let chunk_rows = (64 * 1024 / row_bytes).clamp(1, dec_h);
        let (chunk_tx, chunk_rx) = std::sync::mpsc::sync_channel::<(Vec<u8>, usize)>(runway);
        let (recycle_tx, recycle_rx) = std::sync::mpsc::channel::<Vec<u8>>();

        std::thread::scope(|sc| -> Result<Option<(f64, T)>> {
            // The worker borrows the resizer instead of consuming it, so
            // its Drop runs on this (long-lived blocking-pool) thread and
            // the kernel scratch returns to this thread's pool — dropping
            // it on the ephemeral worker would leak the pool entry into
            // that thread's dying TLS.
            let resizer = &mut resizer;
            let spawned = std::thread::Builder::new()
                .name("oximg-fuse".into())
                .spawn_scoped(sc, move || -> Result<T> {
                    let side_value = side()?;
                    let fwd = fwd_lut_f32();
                    let back = back_lut();
                    while let Ok((buf, rows)) = chunk_rx.recv() {
                        for r in 0..rows {
                            let src = &buf[r * row_bytes..(r + 1) * row_bytes];
                            resizer.push_row_u8(src, fwd, |oy, out| {
                                for (d, &v) in out8[oy * dst_w * 3..(oy + 1) * dst_w * 3]
                                    .iter_mut()
                                    .zip(out)
                                {
                                    *d = back[v as usize];
                                }
                            });
                        }
                        let _ = recycle_tx.send(buf);
                    }
                    anyhow::ensure!(
                        resizer.rows_emitted() == dst_h,
                        "decode ended before the image was complete"
                    );
                    Ok(side_value)
                });
            // Spawn failure (thread limits, transient EAGAIN) leaves the
            // decoder untouched, exactly like a missing kernel — fall
            // back to the byte-identical serial path instead of failing.
            let Ok(worker) = spawned else {
                return Ok(None);
            };

            let t_decode = std::time::Instant::now();
            let decode_result = (|| -> Result<()> {
                let mut remaining = dec_h;
                while remaining > 0 {
                    let mut buf = recycle_rx.try_recv().unwrap_or_default();
                    let want = remaining.min(chunk_rows) * row_bytes;
                    if buf.len() < want {
                        buf.resize(want, 0);
                    }
                    let got = started
                        .read_scanlines_into(&mut buf[..want])
                        .context("decode failed")?
                        .len();
                    anyhow::ensure!(
                        got > 0 && got % row_bytes == 0,
                        "decoder returned a partial row"
                    );
                    let rows = got / row_bytes;
                    remaining -= rows;
                    if chunk_tx.send((buf, rows)).is_err() {
                        // Worker died; its join below reports the real error.
                        anyhow::bail!("fuse worker exited early");
                    }
                }
                Ok(())
            })();
            let decode_ms = t_decode.elapsed().as_secs_f64() * 1e3;
            drop(chunk_tx);

            let worker_result = worker
                .join()
                .map_err(|_| anyhow::anyhow!("fuse worker panicked"))?;
            // A decode error is the root cause; report it over the
            // worker's consequent "incomplete image" error.
            decode_result?;
            Ok(Some((decode_ms, worker_result?)))
        })
    }
}

/// The AVIF sibling of [`fused_resize_pixels`]: the worker converts
/// each resized row straight into the 10-bit 4:2:0 planes (luma per
/// row, chroma per row pair via the same row API the full-frame
/// conversion uses, so the planes are bit-identical to converting
/// `out8` afterwards) — both the resize and the RGB→YUV conversion hide
/// behind the decode wall, and the resized frame never exists as an
/// interleaved RGB copy. Only the one-shot SVT encode remains outside.
///
/// Returns Ok(None) — decoder untouched — when no SIMD row kernel
/// exists; on success returns the decode-loop wall milliseconds with
/// all three planes fully written.
#[cfg(feature = "avif")]
#[cfg_attr(
    not(any(target_arch = "aarch64", target_arch = "x86_64")),
    allow(unused_variables)
)]
#[allow(clippy::too_many_arguments)]
pub(super) fn fused_resize_yuv<R: std::io::BufRead>(
    started: &mut mozjpeg::decompress::DecompressStarted<R>,
    dec_w: usize,
    dec_h: usize,
    dst_w: usize,
    dst_h: usize,
    params: &crate::avif::AvifParams,
    y_plane: &mut [u16],
    cb_plane: &mut [u16],
    cr_plane: &mut [u16],
) -> Result<Option<(f64, crate::avif::SvtSession)>> {
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        Ok(None)
    }
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    {
        let Ok(mut resizer) =
            crate::resize_kernel::StreamResize::<FuseKernel>::new(dec_w, dec_h, dst_w, dst_h, 3)
        else {
            return Ok(None);
        };

        let row_bytes = dec_w * 3;
        // Chunking mirrors fused_resize_encode, but with double the
        // channel runway: the worker spends its first ~1ms creating the
        // SVT session, and four in-flight chunks let the decoder keep
        // running instead of stalling on the bounded channel meanwhile.
        let chunk_rows = (64 * 1024 / row_bytes).clamp(1, dec_h);
        let (chunk_tx, chunk_rx) = std::sync::mpsc::sync_channel::<(Vec<u8>, usize)>(4);
        let (recycle_tx, recycle_rx) = std::sync::mpsc::channel::<Vec<u8>>();

        std::thread::scope(|sc| -> Result<Option<(f64, crate::avif::SvtSession)>> {
            // Borrowed, not moved — see fused_resize_pixels.
            let resizer = &mut resizer;
            let cw = dst_w.div_ceil(2);
            let spawned = std::thread::Builder::new()
                .name("oximg-fuse".into())
                .spawn_scoped(sc, move || -> Result<crate::avif::SvtSession> {
                    // Encoder setup first: its ~1ms overlaps the
                    // decoder's first chunks instead of the tail.
                    let session = crate::avif::start_color_session(dst_w, dst_h, params)?;
                    let fwd = fwd_lut_f32();
                    let back = back_lut();
                    let mut row8 = vec![0u8; dst_w * 3];
                    // Chroma needs the row pair; even rows park here.
                    let mut prev_row = vec![0u8; dst_w * 3];
                    while let Ok((buf, rows)) = chunk_rx.recv() {
                        for r in 0..rows {
                            let src = &buf[r * row_bytes..(r + 1) * row_bytes];
                            resizer.push_row_u8(src, fwd, |oy, out| {
                                for (d, &v) in row8.iter_mut().zip(out) {
                                    *d = back[v as usize];
                                }
                                crate::avif::luma_rows(
                                    &row8,
                                    3,
                                    &mut y_plane[oy * dst_w..][..dst_w],
                                );
                                if oy % 2 == 1 {
                                    let cy = oy / 2;
                                    crate::avif::chroma_row_pair(
                                        &prev_row,
                                        Some(&row8),
                                        dst_w,
                                        3,
                                        &mut cb_plane[cy * cw..][..cw],
                                        &mut cr_plane[cy * cw..][..cw],
                                    );
                                } else {
                                    prev_row.copy_from_slice(&row8);
                                }
                            });
                        }
                        let _ = recycle_tx.send(buf);
                    }
                    anyhow::ensure!(
                        resizer.rows_emitted() == dst_h,
                        "decode ended before the image was complete"
                    );
                    // Odd height: the last row's chroma has no partner.
                    if dst_h % 2 == 1 {
                        let cy = dst_h / 2;
                        crate::avif::chroma_row_pair(
                            &prev_row,
                            None,
                            dst_w,
                            3,
                            &mut cb_plane[cy * cw..][..cw],
                            &mut cr_plane[cy * cw..][..cw],
                        );
                    }
                    Ok(session)
                });
            // Spawn failure leaves the decoder untouched — fall back to
            // the byte-identical serial path instead of failing.
            let Ok(worker) = spawned else {
                return Ok(None);
            };

            let t_decode = std::time::Instant::now();
            let decode_result = (|| -> Result<()> {
                let mut remaining = dec_h;
                while remaining > 0 {
                    let mut buf = recycle_rx.try_recv().unwrap_or_default();
                    let want = remaining.min(chunk_rows) * row_bytes;
                    if buf.len() < want {
                        buf.resize(want, 0);
                    }
                    let got = started
                        .read_scanlines_into(&mut buf[..want])
                        .context("decode failed")?
                        .len();
                    anyhow::ensure!(
                        got > 0 && got % row_bytes == 0,
                        "decoder returned a partial row"
                    );
                    let rows = got / row_bytes;
                    remaining -= rows;
                    if chunk_tx.send((buf, rows)).is_err() {
                        // Worker died; its join below reports the real error.
                        anyhow::bail!("fuse worker exited early");
                    }
                }
                Ok(())
            })();
            let decode_ms = t_decode.elapsed().as_secs_f64() * 1e3;
            drop(chunk_tx);

            let worker_result = worker
                .join()
                .map_err(|_| anyhow::anyhow!("fuse worker panicked"))?;
            // A decode error is the root cause; report it over the
            // worker's consequent "incomplete image" error.
            decode_result?;
            let session = worker_result?;
            Ok(Some((decode_ms, session)))
        })
    }
}
