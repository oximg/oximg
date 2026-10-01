//! The fused decode-overlap workers: decode ∥ resize (plus the
//! incremental encode, the YUV conversion, or the session preheat,
//! per variant) on a scoped worker thread — all byte-identical to
//! their serial fallbacks. One driver owns the concurrency scaffolding
//! (channels, spawn fallback, decode loop, error priority); the
//! variants are row consumers.

use super::*;

/// The SIMD row kernel driving the fused path on this architecture.
#[cfg(target_arch = "aarch64")]
pub(super) type FuseKernel = crate::resize_neon::Neon;
#[cfg(target_arch = "x86_64")]
pub(super) type FuseKernel = crate::resize_avx2::Avx2;

/// Raw YCbCr decoding (libjpeg's raw data mode): the layout of one
/// iMCU row of planes, for sources whose chroma is 2x horizontally
/// subsampled and decoded unscaled. The worker's kernel then fuses the
/// chroma replication, the color conversion and the linear staging
/// ([`crate::resize_kernel::StreamResize::push_row_ycc`]).
#[derive(Clone, Copy)]
pub(super) struct YccLayout {
    /// Luma rows per chroma row: 2 for 4:2:0, 1 for 4:2:2.
    pub(super) v: usize,
    /// Bytes per luma / chroma plane row (whole DCT blocks).
    pub(super) y_stride: usize,
    pub(super) c_stride: usize,
}

/// One decoded chunk: RGB rows in plane 0, or one iMCU row of Y, Cb and
/// Cr planes; plus its (image) row count.
type Chunk = ([Vec<u8>; 3], usize);

/// One decoded source row, as the fused variants push it.
pub(super) enum Row<'a> {
    Rgb(&'a [u8]),
    /// A luma row, with its chroma rows when they changed.
    Ycc(&'a [u8], Option<(&'a [u8], &'a [u8])>),
}

impl Row<'_> {
    pub(super) fn push<K: crate::resize_kernel::RowKernel>(
        self,
        resizer: &mut crate::resize_kernel::StreamResize<K>,
        lut: &[f32; 256],
        emit: impl FnMut(usize, &[u16]),
    ) {
        match self {
            Row::Rgb(src) => resizer.push_row_u8(src, lut, emit),
            Row::Ycc(y, chroma) => resizer.push_row_ycc(y, chroma, lut, emit),
        }
    }
}

/// Where a [`FuseChunks`] gets its chunks.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
enum ChunkSource<'a> {
    /// From the decoder on the request thread; drained buffers flow
    /// back to it for reuse.
    Channel {
        rx: std::sync::mpsc::Receiver<Chunk>,
        recycle: std::sync::mpsc::Sender<[Vec<u8>; 3]>,
    },
    /// Straight from the decoder, on the calling thread (None once it
    /// is done): the spawn-failure fallback of a raw decode, which
    /// cannot hand the serial path a decoder started in raw mode.
    Inline(&'a mut dyn FnMut([Vec<u8>; 3]) -> Result<Option<Chunk>>),
}

/// The worker's end of the chunk pipeline: decoded row buffers arrive
/// in order.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub(super) struct FuseChunks<'a> {
    src: ChunkSource<'a>,
    row_bytes: usize,
    ycc: Option<YccLayout>,
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
impl FuseChunks<'_> {
    /// Drain decoded rows in order until the decoder finishes (or
    /// dies — completeness is the caller's `rows_emitted` check).
    fn for_each_row(mut self, mut f: impl FnMut(Row<'_>) -> Result<()>) -> Result<()> {
        let mut spare = <[Vec<u8>; 3]>::default();
        loop {
            let (bufs, rows) = match &mut self.src {
                ChunkSource::Channel { rx, .. } => match rx.recv() {
                    Ok(chunk) => chunk,
                    Err(_) => break,
                },
                ChunkSource::Inline(next) => match next(std::mem::take(&mut spare))? {
                    Some(chunk) => chunk,
                    None => break,
                },
            };
            match self.ycc {
                None => {
                    for r in 0..rows {
                        f(Row::Rgb(
                            &bufs[0][r * self.row_bytes..(r + 1) * self.row_bytes],
                        ))?;
                    }
                }
                Some(l) => {
                    for r in 0..rows {
                        let y = &bufs[0][r * l.y_stride..(r + 1) * l.y_stride];
                        let chroma = (r % l.v == 0).then(|| {
                            let c = r / l.v * l.c_stride..(r / l.v + 1) * l.c_stride;
                            (&bufs[1][c.clone()], &bufs[2][c])
                        });
                        f(Row::Ycc(y, chroma))?;
                    }
                }
            }
            match &self.src {
                ChunkSource::Channel { recycle, .. } => {
                    let _ = recycle.send(bufs);
                }
                ChunkSource::Inline(_) => spare = bufs,
            }
        }
        Ok(())
    }
}

/// Decode the next chunk into `bufs` (reused): up to `chunk_rows` RGB
/// rows into plane 0, or with `ycc` one iMCU row of Y, Cb and Cr planes.
/// Returns its row count, at most `remaining`.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn read_chunk<R: std::io::BufRead>(
    started: &mut jpeg_dec::DecompressStarted<R>,
    bufs: &mut [Vec<u8>; 3],
    remaining: usize,
    row_bytes: usize,
    chunk_rows: usize,
    ycc: Option<YccLayout>,
) -> Result<usize> {
    if let Some(l) = ycc {
        // One whole iMCU row; rows past the image height are padding.
        started.read_raw_chunk(bufs).context("decode failed")?;
        return Ok(remaining.min(l.v * 8));
    }
    let buf = &mut bufs[0];
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
    Ok(got / row_bytes)
}

/// Decode a raw-started image on this thread, handing each row to `f`:
/// the serial streamed path's reader for a raw decode.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub(super) fn raw_rows_inline<R: std::io::BufRead>(
    started: &mut jpeg_dec::DecompressStarted<R>,
    dec_h: usize,
    l: YccLayout,
    f: impl FnMut(Row<'_>) -> Result<()>,
) -> Result<()> {
    let mut remaining = dec_h;
    let mut next = |mut bufs: [Vec<u8>; 3]| -> Result<Option<Chunk>> {
        if remaining == 0 {
            return Ok(None);
        }
        let rows = read_chunk(started, &mut bufs, remaining, 0, 0, Some(l))?;
        remaining -= rows;
        Ok(Some((bufs, rows)))
    };
    FuseChunks {
        src: ChunkSource::Inline(&mut next),
        row_bytes: 0,
        ycc: Some(l),
    }
    .for_each_row(f)
}

#[cfg(test)]
thread_local! {
    /// Fail the next fused worker spawns on this thread (tests of the
    /// spawn-failure fallbacks).
    pub(super) static FAIL_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn spawn_blocked() -> bool {
    #[cfg(test)]
    return FAIL_SPAWN.with(std::cell::Cell::get);
    #[cfg(not(test))]
    false
}

/// The scaffolding every fused variant shares: this (request) thread
/// keeps the decoder at its serial-decode floor while `worker` runs on
/// a scoped thread consuming decoded chunks. Owns the bounded chunk
/// channel (`runway` slots — 2 when the worker starts consuming
/// immediately, 4 when a setup task occupies it first), the buffer
/// recycling, the spawn-failure fallback (Ok(None), decoder untouched,
/// caller takes the byte-identical serial path — or, for a raw decode,
/// the worker run inline on this thread), and the join logic
/// where a decode error outranks the worker's consequent
/// "incomplete image" error. Returns the decode-loop wall milliseconds
/// (the fused pipeline's floor) alongside the worker's value.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn fused_decode_loop<R: std::io::BufRead, T: Send>(
    started: &mut jpeg_dec::DecompressStarted<R>,
    dec_w: usize,
    dec_h: usize,
    runway: usize,
    ycc: Option<YccLayout>,
    worker: impl FnOnce(FuseChunks<'_>) -> Result<T> + Send,
) -> Result<Option<(f64, T)>> {
    let row_bytes = dec_w * 3;
    // Smaller chunks than the serial path's 256KB: granularity here
    // sets the post-decode tail (the last chunk's downstream work
    // cannot hide behind the decode), and per-chunk handoff is ~µs.
    let chunk_rows = (64 * 1024 / row_bytes).clamp(1, dec_h);
    let (chunk_tx, chunk_rx) = std::sync::mpsc::sync_channel::<Chunk>(runway);
    let (recycle_tx, recycle_rx) = std::sync::mpsc::channel::<[Vec<u8>; 3]>();
    // The worker waits here rather than moving into the thread: a failed
    // spawn drops its closure, and the raw fallback needs the worker back.
    let slot = std::sync::Mutex::new(Some(worker));
    let slot = &slot;
    let take_worker = || {
        slot.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("the worker runs once")
    };

    std::thread::scope(|sc| -> Result<Option<(f64, T)>> {
        let spawned = if spawn_blocked() {
            Err(std::io::Error::other("spawn blocked by test"))
        } else {
            std::thread::Builder::new()
                .name("oximg-fuse".into())
                .spawn_scoped(sc, move || {
                    // Built here, not sent: the inline variant is not Send.
                    take_worker()(FuseChunks {
                        src: ChunkSource::Channel {
                            rx: chunk_rx,
                            recycle: recycle_tx,
                        },
                        row_bytes,
                        ycc,
                    })
                })
        };
        // Spawn failure (thread limits, transient EAGAIN) leaves the
        // decoder untouched, exactly like a missing kernel — fall back
        // to the byte-identical serial path instead of failing. A raw
        // decode has already started the decoder in raw mode, which the
        // serial path cannot read; it runs the worker inline instead,
        // pulling chunks straight from the decoder (same bytes, no
        // overlap).
        let Ok(worker) = spawned else {
            if ycc.is_none() {
                return Ok(None);
            }
            let t_decode = std::time::Instant::now();
            let mut remaining = dec_h;
            let mut next = |mut bufs: [Vec<u8>; 3]| -> Result<Option<Chunk>> {
                if remaining == 0 {
                    return Ok(None);
                }
                let rows = read_chunk(started, &mut bufs, remaining, row_bytes, chunk_rows, ycc)?;
                remaining -= rows;
                Ok(Some((bufs, rows)))
            };
            let value = take_worker()(FuseChunks {
                src: ChunkSource::Inline(&mut next),
                row_bytes,
                ycc,
            })?;
            return Ok(Some((t_decode.elapsed().as_secs_f64() * 1e3, value)));
        };

        // Decode loop on the request thread: read a chunk, hand it to
        // the worker, reuse buffers the worker has drained.
        let t_decode = std::time::Instant::now();
        // Set when the loop stops *only* because the worker's receiver
        // was dropped (worker already failed/returned). In that case
        // the worker holds the root cause; a genuine decode error does
        // not set it and stays the root cause instead.
        let mut worker_gone = false;
        let decode_result = (|| -> Result<()> {
            let mut remaining = dec_h;
            while remaining > 0 {
                let mut bufs = recycle_rx.try_recv().unwrap_or_default();
                let rows = read_chunk(started, &mut bufs, remaining, row_bytes, chunk_rows, ycc)?;
                remaining -= rows;
                if chunk_tx.send((bufs, rows)).is_err() {
                    // Worker vanished; its join below carries the real
                    // (often ServerFault-marked) error. Backstop this
                    // sentinel as a ServerFault too, in case the worker
                    // somehow returned Ok.
                    worker_gone = true;
                    return Err(anyhow::anyhow!("fuse worker exited early").context(ServerFault));
                }
            }
            Ok(())
        })();
        let decode_ms = t_decode.elapsed().as_secs_f64() * 1e3;
        drop(chunk_tx);

        let worker_result = worker
            .join()
            .map_err(|_| anyhow::anyhow!("fuse worker panicked").context(ServerFault))?;
        // Error priority. When the decoder stopped only because the
        // worker vanished, the worker's error is the root cause and
        // must win — surfacing the generic "exited early" sentinel
        // there both hid the real message and dropped its ServerFault
        // marker (a 500 became a 422). A genuine decode error, on the
        // other hand, outranks the worker's consequent "incomplete
        // image".
        if worker_gone {
            let value = worker_result?;
            decode_result?; // only the send-fail sentinel reaches here
            Ok(Some((decode_ms, value)))
        } else {
            decode_result?;
            Ok(Some((decode_ms, worker_result?)))
        }
    })
}

/// The fused JPEG fast path: the worker converts each decoded chunk to
/// linear u16, streams it through the row-push resize kernel, and
/// feeds finished rows to an incremental jpegli encoder. Everything
/// downstream of the decoder hides behind the decode wall; the only
/// serial tail left is jpegli's entropy pass in `finish`.
///
/// Returns Ok(None) — with the decoder untouched — when no SIMD row
/// kernel exists for this CPU, so the caller falls back to the serial
/// path. Output bytes are identical to the serial jpegli path: the same
/// kernel produces the same u16 rows (streamed emission is bit-identical
/// to the full-frame schedule), and jpegli is deterministic for the same
/// scanlines and settings regardless of write granularity.
///
/// On success returns the encoded bytes and the decode-loop wall
/// milliseconds.
#[cfg_attr(
    not(any(target_arch = "aarch64", target_arch = "x86_64")),
    allow(unused_variables)
)]
#[allow(clippy::too_many_arguments)]
pub(super) fn fused_resize_encode<R: std::io::BufRead>(
    started: &mut jpeg_dec::DecompressStarted<R>,
    dec_w: usize,
    dec_h: usize,
    dst_w: usize,
    dst_h: usize,
    quality: f32,
    icc: Option<&[u8]>,
    ycc: Option<YccLayout>,
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
            // Unreachable for a raw decode: its start requires the kernel.
            anyhow::ensure!(ycc.is_none(), "raw decode without a fused kernel");
            return Ok(None);
        };
        // Borrowed, not moved: the resizer's Drop must run on this
        // long-lived blocking-pool thread so its kernel scratch returns
        // to this thread's pool instead of dying with the ephemeral
        // worker's TLS.
        let resizer = &mut resizer;
        let out = fused_decode_loop(started, dec_w, dec_h, 2, ycc, move |chunks| {
            let fwd = fwd_lut_f32();
            let back = back_lut();
            let mut row8 = vec![0u8; dst_w * 3];

            // Mirrors encode_jpegli (including the progressive knob).
            let mut enc = JpegliEncoder::new(dst_w, dst_h, quality, jpegli_progressive());
            // Same chunker, same position as encode_jpegli: the profile
            // precedes the scanlines, so fused output stays
            // byte-identical to the serial encoder.
            if let Some(icc) = icc {
                for chunk in icc_app2_chunks(icc) {
                    enc.write_marker(JPEG_APP2, &chunk);
                }
            }

            chunks.for_each_row(|row| {
                let mut enc_result = Ok(());
                row.push(resizer, fwd, |_, out| {
                    for (d, &v) in row8.iter_mut().zip(out) {
                        *d = back[v as usize];
                    }
                    if enc_result.is_ok() {
                        enc_result = enc.write_scanlines(&row8);
                    }
                });
                enc_result
                    .context("fused encode failed")
                    .context(ServerFault)
            })?;
            // Channel closed: either the decoder delivered everything or
            // it failed mid-image; only a complete image may be finished
            // into a JPEG.
            anyhow::ensure!(
                resizer.rows_emitted() == dst_h,
                "decode ended before the image was complete"
            );
            Ok(enc.finish())
        })?;
        Ok(out.map(|(decode_ms, bytes)| (bytes, decode_ms)))
    }
}

/// The cross-format sibling of [`fused_resize_encode`]: the worker
/// streams rows through the SIMD kernel straight into `out8` — the
/// exact writes the serial path performs inline, so pixels are
/// byte-identical to it. The (one-shot) target encoder runs after, on
/// the request thread; only the encode stays outside the decode wall,
/// which is as much overlap as WebP/AVIF/PNG's full-frame encode APIs
/// allow.
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
    started: &mut jpeg_dec::DecompressStarted<R>,
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
    ycc: Option<YccLayout>,
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
            // Unreachable for a raw decode: its start requires the kernel.
            anyhow::ensure!(ycc.is_none(), "raw decode without a fused kernel");
            return Ok(None);
        };
        // Borrowed, not moved — see fused_resize_encode.
        let resizer = &mut resizer;
        fused_decode_loop(started, dec_w, dec_h, runway, ycc, move |chunks| {
            let side_value = side()?;
            let fwd = fwd_lut_f32();
            let back = back_lut();
            chunks.for_each_row(|row| {
                row.push(resizer, fwd, |oy, out| {
                    for (d, &v) in out8[oy * dst_w * 3..(oy + 1) * dst_w * 3]
                        .iter_mut()
                        .zip(out)
                    {
                        *d = back[v as usize];
                    }
                });
                Ok(())
            })?;
            anyhow::ensure!(
                resizer.rows_emitted() == dst_h,
                "decode ended before the image was complete"
            );
            Ok(side_value)
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
    started: &mut jpeg_dec::DecompressStarted<R>,
    dec_w: usize,
    dec_h: usize,
    dst_w: usize,
    dst_h: usize,
    params: &crate::avif::AvifParams,
    y_plane: &mut [u16],
    cb_plane: &mut [u16],
    cr_plane: &mut [u16],
    ycc: Option<YccLayout>,
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
            // Unreachable for a raw decode: its start requires the kernel.
            anyhow::ensure!(ycc.is_none(), "raw decode without a fused kernel");
            return Ok(None);
        };
        // Borrowed, not moved — see fused_resize_encode. Runway 4: the
        // worker spends its first ~1ms creating the SVT session, and
        // four in-flight chunks let the decoder keep running instead of
        // stalling on the bounded channel meanwhile.
        let resizer = &mut resizer;
        let cw = dst_w.div_ceil(2);
        fused_decode_loop(started, dec_w, dec_h, 4, ycc, move |chunks| {
            // Encoder setup first: its ~1ms overlaps the decoder's
            // first chunks instead of the tail.
            let session = crate::avif::start_color_session(dst_w, dst_h, params)?;
            let fwd = fwd_lut_f32();
            let back = back_lut();
            let mut row8 = vec![0u8; dst_w * 3];
            // Chroma needs the row pair; even rows park here.
            let mut prev_row = vec![0u8; dst_w * 3];
            chunks.for_each_row(|row| {
                row.push(resizer, fwd, |oy, out| {
                    for (d, &v) in row8.iter_mut().zip(out) {
                        *d = back[v as usize];
                    }
                    crate::avif::luma_rows(&row8, 3, &mut y_plane[oy * dst_w..][..dst_w]);
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
                Ok(())
            })?;
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
        })
    }
}
