use super::*;

/// A resolved snapshot for a plain fit box — the shape most decode
/// tests use (every knob at the env-configured behavior, like the
/// old `&Params::default()` argument).
fn rbox(max_w: u32, max_h: u32) -> Resolved {
    Resolved::new(&Params {
        max_width: max_w,
        max_height: max_h,
        ..Params::default()
    })
}

/// Deterministic RGB test frame encoded to a real JPEG source.
fn make_test_jpeg(w: usize, h: usize, gray: bool) -> Vec<u8> {
    make_jpeg(w, h, gray, false)
}

/// [`make_test_jpeg`]'s pixels in a single scan: mozjpeg's defaults
/// write progressive, which libjpeg decodes by buffering every
/// coefficient first.
fn make_baseline_jpeg(w: usize, h: usize, gray: bool) -> Vec<u8> {
    make_jpeg(w, h, gray, true)
}

fn make_jpeg(w: usize, h: usize, gray: bool, baseline: bool) -> Vec<u8> {
    let ch = if gray { 1 } else { 3 };
    let mut seed = 0x9E3779B9u32;
    let mut px = Vec::with_capacity(w * h * ch);
    for y in 0..h {
        for x in 0..w {
            for c in 0..ch {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let noise = (seed >> 24) as usize;
                px.push(((x * 200 / w + y * 40 / h + c * 5 + noise / 4).min(255)) as u8);
            }
        }
    }
    let mut comp = Compress::new(if gray {
        ColorSpace::JCS_GRAYSCALE
    } else {
        ColorSpace::JCS_RGB
    });
    if baseline {
        comp.set_fastest_defaults();
    }
    comp.set_size(w, h);
    comp.set_quality(90.0);
    let mut started = comp.start_compress(Vec::new()).unwrap();
    started.write_scanlines(&px).unwrap();
    started.finish().unwrap()
}

fn run_jpeg(jpeg: &[u8], fuse_quality: Option<f32>) -> Vec<u8> {
    run_jpeg_icc(jpeg, fuse_quality, None)
}

fn run_jpeg_icc(jpeg: &[u8], fuse_quality: Option<f32>, icc: Option<&[u8]>) -> Vec<u8> {
    let p = Params {
        max_width: 320,
        max_height: 320,
        quality: 80.0,
        encoder: Encoder::Jpegli,
        parallel: 1,
        ..Params::default()
    };
    let fuse = match fuse_quality {
        Some(quality) => Fuse::Jpegli { quality },
        None => Fuse::Off,
    };
    let mut s = Scratch::default();
    let dec = Decompress::new_mem(jpeg).unwrap();
    match decode_resize(
        &mut s,
        dec,
        &rbox(320, 320),
        crate::meta::Orientation::UPRIGHT,
        fuse,
        icc,
        None,
    )
    .unwrap()
    {
        Decoded::Encoded(out) => {
            assert!(fuse_quality.is_some(), "fused output without fuse request");
            out
        }
        Decoded::Pixels { dst_w, dst_h } => {
            assert!(
                fuse_quality.is_none(),
                "fused path was requested but not taken"
            );
            encode_with_icc(
                &s.out8[..dst_w * dst_h * 3],
                dst_w,
                dst_h,
                &Resolved::new(&p),
                icc,
            )
            .unwrap()
        }
        #[cfg(feature = "avif")]
        Decoded::YuvPlanes { .. } => panic!("yuv fuse was not requested"),
        #[cfg(feature = "avif")]
        Decoded::PixelsSession { .. } => panic!("preheat fuse was not requested"),
    }
}

/// The fused jpegli encoder writes the ICC chain ahead of its
/// scanlines exactly like the serial encoder does — profiled
/// same-format JPEG keeps its bytes fuse-independent.
#[test]
fn fused_jpegli_bytes_match_serial_with_icc() {
    if !fuse_kernel_available() {
        return;
    }
    let jpeg = make_test_jpeg(799, 601, false);
    let icc: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    let serial = run_jpeg_icc(&jpeg, None, Some(&icc));
    let fused = run_jpeg_icc(&jpeg, Some(80.0), Some(&icc));
    assert_eq!(serial, fused, "profiled fused/serial parity");
    // 70KB spans two APP2 chunks; both must survive intact.
    let mut chunks: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut i = 2;
    while i + 4 <= fused.len() && fused[i] == 0xFF {
        let m = fused[i + 1];
        if m == 0xDA || m == 0xD9 {
            break;
        }
        let len = u16::from_be_bytes([fused[i + 2], fused[i + 3]]) as usize;
        let body = &fused[i + 4..i + 2 + len];
        if m == 0xE2 && body.starts_with(b"ICC_PROFILE\0") {
            chunks.push((body[12], body[14..].to_vec()));
        }
        i += 2 + len;
    }
    chunks.sort_by_key(|(seq, _)| *seq);
    let got: Vec<u8> = chunks.into_iter().flat_map(|(_, d)| d).collect();
    assert_eq!(got, icc, "profile reassembles from the fused output");
}

/// Resized RGB pixels via the given fuse mode (Off = the serial
/// streamed kernel path, Pixels = the cross-format fused worker).
fn run_jpeg_pixels(jpeg: &[u8], fuse: Fuse) -> Vec<u8> {
    let mut s = Scratch::default();
    let dec = Decompress::new_mem(jpeg).unwrap();
    match decode_resize(
        &mut s,
        dec,
        &rbox(320, 320),
        crate::meta::Orientation::UPRIGHT,
        fuse,
        None,
        None,
    )
    .unwrap()
    {
        Decoded::Pixels { dst_w, dst_h } => s.out8[..dst_w * dst_h * 3].to_vec(),
        Decoded::Encoded(_) => panic!("pixel run must not encode"),
        #[cfg(feature = "avif")]
        Decoded::YuvPlanes { .. } => panic!("yuv fuse was not requested"),
        #[cfg(feature = "avif")]
        Decoded::PixelsSession { .. } => panic!("preheat fuse was not requested"),
    }
}

#[cfg(feature = "avif")]
fn test_avif_params() -> crate::avif::AvifParams {
    crate::avif::AvifParams {
        quality: 55,
        alpha_quality: 55,
        ..Default::default()
    }
}

/// The fused AVIF path converts rows (and creates the encoder
/// session) during the decode overlap; its planes — and therefore
/// the encoded bytes — must match the serial path's full-frame
/// conversion of the same pixels exactly.
#[cfg(feature = "avif")]
fn run_jpeg_avif(jpeg: &[u8], yuv_fuse: bool) -> Vec<u8> {
    run_jpeg_avif_icc(jpeg, yuv_fuse, None)
}

#[cfg(feature = "avif")]
fn run_jpeg_avif_icc(jpeg: &[u8], yuv_fuse: bool, icc: Option<&[u8]>) -> Vec<u8> {
    let params = test_avif_params();
    let fuse = if yuv_fuse {
        Fuse::Yuv { params }
    } else {
        Fuse::Off
    };
    let mut s = Scratch::default();
    let dec = Decompress::new_mem(jpeg).unwrap();
    match decode_resize(
        &mut s,
        dec,
        &rbox(320, 320),
        crate::meta::Orientation::UPRIGHT,
        fuse,
        None,
        None,
    )
    .unwrap()
    {
        Decoded::Pixels { dst_w, dst_h } => {
            crate::avif::encode_avif(&s.out8[..dst_w * dst_h * 3], dst_w, dst_h, 3, &params, icc)
                .unwrap()
        }
        Decoded::YuvPlanes { session } => {
            crate::avif::encode_avif_with_session(session, &s.y16, &s.cb16, &s.cr16, icc).unwrap()
        }
        Decoded::PixelsSession { .. } => panic!("preheat fuse was not requested"),
        Decoded::Encoded(_) => panic!("avif run must not hit the jpegli fuse"),
    }
}

/// Oriented AVIF targets preheat their session on the fused
/// worker; the bytes must match the fully serial path (rotate on
/// out8, then encode_avif) exactly.
#[cfg(feature = "avif")]
#[test]
fn preheated_session_bytes_match_serial_oriented_avif() {
    if !fuse_kernel_available() {
        return;
    }
    let params = test_avif_params();
    let orientation = crate::meta::Orientation::from_rot_mirror(1, None); // 90° CCW
    let jpeg = make_test_jpeg(799, 601, false);
    let run = |fuse: Fuse| -> Vec<u8> {
        let mut s = Scratch::default();
        let dec = Decompress::new_mem(&jpeg).unwrap();
        match decode_resize(&mut s, dec, &rbox(320, 320), orientation, fuse, None, None).unwrap() {
            Decoded::PixelsSession {
                dst_w,
                dst_h,
                session,
            } => {
                let dims = crate::meta::apply_orientation(
                    &s.out8[..dst_w * dst_h * 3],
                    dst_w,
                    dst_h,
                    3,
                    orientation,
                    &mut s.chunk8,
                );
                std::mem::swap(&mut s.out8, &mut s.chunk8);
                crate::avif::encode_avif_rgb_with_session(
                    session,
                    &s.out8[..dims.0 * dims.1 * 3],
                    dims.0,
                    dims.1,
                    None,
                )
                .unwrap()
            }
            Decoded::Pixels { dst_w, dst_h } => {
                let dims = crate::meta::apply_orientation(
                    &s.out8[..dst_w * dst_h * 3],
                    dst_w,
                    dst_h,
                    3,
                    orientation,
                    &mut s.chunk8,
                );
                std::mem::swap(&mut s.out8, &mut s.chunk8);
                crate::avif::encode_avif(
                    &s.out8[..dims.0 * dims.1 * 3],
                    dims.0,
                    dims.1,
                    3,
                    &params,
                    None,
                )
                .unwrap()
            }
            _ => panic!("unexpected decode result"),
        }
    };
    let serial = run(Fuse::Off);
    let preheated = run(Fuse::PixelsPreheat { params });
    assert_eq!(serial, preheated, "preheat must not change a byte");
}

#[cfg(feature = "avif")]
#[test]
fn fused_yuv_bytes_match_serial_avif() {
    if !fuse_kernel_available() {
        return;
    }
    // Odd dimensions exercise chunk boundaries, scalar tails, and
    // the odd-height final chroma row.
    for (w, h, gray) in [(799, 601, false), (400, 300, true), (321, 243, false)] {
        let jpeg = make_test_jpeg(w, h, gray);
        assert_eq!(
            run_jpeg_avif(&jpeg, false),
            run_jpeg_avif(&jpeg, true),
            "{w}x{h} gray={gray}"
        );
    }
    // With a profile the parity must hold too — the fused session
    // path and the one-shot path splice the identical colr.
    let jpeg = make_test_jpeg(321, 243, false);
    let icc: Vec<u8> = (0..500u32).map(|i| (i % 251) as u8).collect();
    let serial = run_jpeg_avif_icc(&jpeg, false, Some(&icc));
    let fused = run_jpeg_avif_icc(&jpeg, true, Some(&icc));
    assert_eq!(serial, fused, "profiled fused/serial parity");
    assert_eq!(crate::avif::extract_icc(&fused).as_deref(), Some(&icc[..]));
}

#[cfg(feature = "avif")]
#[test]
fn fused_yuv_survives_truncated_sources() {
    let jpeg = make_test_jpeg(799, 601, false);
    let cut = &jpeg[..jpeg.len() * 3 / 5];
    let mut s = Scratch::default();
    if let Ok(dec) = Decompress::new_mem(cut) {
        let _ = decode_resize(
            &mut s,
            dec,
            &rbox(320, 320),
            crate::meta::Orientation::UPRIGHT,
            Fuse::Yuv {
                params: test_avif_params(),
            },
            None,
            None,
        );
    }
}

#[test]
fn serial_jpeg_path_produces_valid_output() {
    let jpeg = make_test_jpeg(400, 300, false);
    let out = run_jpeg(&jpeg, None);
    assert!(out.starts_with(&[0xFF, 0xD8]), "not a JPEG");
}

fn fuse_kernel_available() -> bool {
    use crate::resize_kernel::RowKernel;
    if FuseKernel::detect() {
        true
    } else {
        eprintln!("skipping: no SIMD row kernel on this host");
        false
    }
}

/// The serial path streams rows through the same SIMD kernel the
/// fused path runs on its worker thread, so the bytes must match
/// exactly on every architecture.
#[test]
fn fused_path_bytes_match_serial_jpegli() {
    if !fuse_kernel_available() {
        return;
    }
    // Odd dimensions exercise chunk boundaries and scalar tails.
    let jpeg = make_test_jpeg(799, 601, false);
    assert_eq!(run_jpeg(&jpeg, None), run_jpeg(&jpeg, Some(80.0)));
}

/// `make_test_jpeg`'s frame, color, with luma sampling `samp` (h, v)
/// over full-resolution chroma: (2, 2) is 4:2:0, (2, 1) 4:2:2.
fn make_sampled_jpeg(w: usize, h: usize, samp: (i32, i32)) -> Vec<u8> {
    let mut seed = 0x9E3779B9u32;
    let px: Vec<u8> = (0..w * h * 3)
        .map(|i| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let (x, y, c) = (i / 3 % w, i / 3 / w, i % 3);
            ((x * 200 / w + y * 40 / h + c * 5 + (seed >> 26) as usize).min(255)) as u8
        })
        .collect();
    let mut comp = Compress::new(ColorSpace::JCS_RGB);
    comp.set_size(w, h);
    comp.set_quality(90.0);
    for (i, c) in comp.components_mut().iter_mut().enumerate() {
        (c.h_samp_factor, c.v_samp_factor) = if i == 0 { samp } else { (1, 1) };
    }
    let mut started = comp.start_compress(Vec::new()).unwrap();
    started.write_scanlines(&px).unwrap();
    started.finish().unwrap()
}

/// The YCbCr staging kernels' reference, `ycc_to_rgb` over replicated
/// chroma, is libjpeg's own conversion: every pixel of the raw planes
/// converts to exactly what the replicating RGB decode outputs, for
/// both samplings the raw path takes and odd sizes. Also pins the raw
/// reader's chunking: one iMCU row per call, then UnexpectedEof.
#[test]
fn ycc_reference_matches_libjpeg_replication() {
    for samp in [(2, 1), (2, 2)] {
        for (w, h) in [(1, 1), (17, 9), (64, 33), (799, 601)] {
            let jpeg = make_sampled_jpeg(w, h, samp);
            let mut dec = Decompress::new_mem(&jpeg).unwrap();
            dec.do_fancy_upsampling(false);
            let mut started = dec.rgb().unwrap();
            let mut rgb = vec![0u8; w * h * 3];
            started.read_scanlines_into(&mut rgb).unwrap();
            started.finish().unwrap();

            let mut started = Decompress::new_mem(&jpeg).unwrap().raw().unwrap();
            let ((v, ys), (cv, cs)) = (started.raw_plane(0), started.raw_plane(1));
            assert_eq!((v, cv), (samp.1 as usize, 1));
            let (mut y, mut cb, mut cr) = (Vec::new(), Vec::new(), Vec::new());
            let mut planes = <[Vec<u8>; 3]>::default();
            for _ in 0..h.div_ceil(v * 8) {
                started.read_raw_chunk(&mut planes).unwrap();
                y.extend_from_slice(&planes[0][..v * 8 * ys]);
                cb.extend_from_slice(&planes[1][..8 * cs]);
                cr.extend_from_slice(&planes[2][..8 * cs]);
            }
            let past = started.read_raw_chunk(&mut planes).unwrap_err();
            assert_eq!(past.kind(), std::io::ErrorKind::UnexpectedEof);
            started.finish().unwrap();
            for (i, px) in rgb.as_chunks::<3>().0.iter().enumerate() {
                let (x, r) = (i % w, i / w);
                let c = r / v * cs + x / 2;
                let want = crate::resize_kernel::ycc_to_rgb(y[r * ys + x], cb[c], cr[c]);
                assert_eq!(*px, want, "{samp:?} {w}x{h} pixel ({x}, {r})");
            }
        }
    }
}

/// Every kernel path matches the RGB-row decode byte for byte across
/// the samplings: serial and fused, raw-decoded (4:2:0 and 4:2:2 on
/// kernels with YCbCr staging) or not, and the raw path's spawn-failure
/// fallback, which runs the fused worker inline.
#[test]
fn kernel_paths_match_rgb_rows_across_samplings() {
    use crate::resize_kernel::RowKernel;
    if !fuse_kernel_available() {
        return;
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FAIL_SPAWN.with(|f| f.set(false));
            NO_RAW.with(|f| f.set(false));
        }
    }
    let _reset = Reset;
    for samp in [(1, 1), (2, 1), (1, 2), (2, 2)] {
        for (w, h) in [(799, 601), (1023, 677), (333, 999)] {
            let jpeg = make_sampled_jpeg(w, h, samp);
            NO_RAW.with(|f| f.set(true));
            let (want, want_px) = (run_jpeg(&jpeg, None), run_jpeg_pixels(&jpeg, Fuse::Off));
            NO_RAW.with(|f| f.set(false));
            // Only a raw decode survives a failed spawn fused.
            let raw = matches!(samp, (2, 1) | (2, 2)) && FuseKernel::ycc_h2();
            for blocked in [false, true].into_iter().take(1 + usize::from(raw)) {
                FAIL_SPAWN.with(|f| f.set(blocked));
                let what = format!("{samp:?} {w}x{h} spawn blocked: {blocked}");
                assert_eq!(want, run_jpeg(&jpeg, None), "serial jpegli {what}");
                assert_eq!(want, run_jpeg(&jpeg, Some(80.0)), "fused jpegli {what}");
                let px = run_jpeg_pixels(&jpeg, Fuse::Off);
                assert_eq!(want_px, px, "serial pixels {what}");
                let px = run_jpeg_pixels(&jpeg, Fuse::Pixels);
                assert_eq!(want_px, px, "fused pixels {what}");
            }
            FAIL_SPAWN.with(|f| f.set(false));
        }
    }
}

#[test]
fn fused_path_is_deterministic_and_valid() {
    if !fuse_kernel_available() {
        return;
    }
    let jpeg = make_test_jpeg(799, 601, false);
    let a = run_jpeg(&jpeg, Some(80.0));
    let b = run_jpeg(&jpeg, Some(80.0));
    assert!(a.starts_with(&[0xFF, 0xD8]), "not a JPEG");
    assert_eq!(a, b, "fused output must not vary run to run");
    let (fmt, w, h) = probe(&a).unwrap();
    assert_eq!(fmt, ImageFormat::Jpeg);
    assert_eq!((w, h), (320, 241));
}

#[test]
fn fused_path_handles_grayscale_sources() {
    if !fuse_kernel_available() {
        return;
    }
    let jpeg = make_test_jpeg(400, 300, true);
    let fused = run_jpeg(&jpeg, Some(80.0));
    assert!(fused.starts_with(&[0xFF, 0xD8]), "not a JPEG");
    assert_eq!(run_jpeg(&jpeg, None), fused);
}

/// The cross-format fused worker writes the same rows the serial
/// streamed path writes inline, so out8 must match byte for byte.
#[test]
fn fused_pixels_match_serial_pixels() {
    if !fuse_kernel_available() {
        return;
    }
    // Odd dimensions exercise chunk boundaries and scalar tails.
    for (w, h, gray) in [(799, 601, false), (400, 300, true)] {
        let jpeg = make_test_jpeg(w, h, gray);
        assert_eq!(
            run_jpeg_pixels(&jpeg, Fuse::Off),
            run_jpeg_pixels(&jpeg, Fuse::Pixels),
            "{w}x{h} gray={gray}"
        );
    }
}

#[test]
fn fused_pixels_survive_truncated_sources() {
    let jpeg = make_test_jpeg(799, 601, false);
    let cut = &jpeg[..jpeg.len() * 3 / 5];
    let mut s = Scratch::default();
    if let Ok(dec) = Decompress::new_mem(cut) {
        let _ = decode_resize(
            &mut s,
            dec,
            &rbox(320, 320),
            crate::meta::Orientation::UPRIGHT,
            Fuse::Pixels,
            None,
            None,
        );
    }
}

#[test]
fn fused_path_survives_truncated_sources() {
    // Truncation mid-scan must neither hang the worker handoff nor
    // panic; libjpeg may error out or complete with fill data
    // depending on where the cut lands — both are acceptable here.
    let jpeg = make_test_jpeg(799, 601, false);
    let cut = &jpeg[..jpeg.len() * 3 / 5];
    let p = Params {
        max_width: 320,
        max_height: 320,
        quality: 80.0,
        encoder: Encoder::Jpegli,
        parallel: 1,
        ..Params::default()
    };
    let mut s = Scratch::default();
    if let Ok(dec) = Decompress::new_mem(cut) {
        let _ = decode_resize(
            &mut s,
            dec,
            &rbox(320, 320),
            crate::meta::Orientation::UPRIGHT,
            Fuse::Jpegli { quality: p.quality },
            None,
            None,
        );
    }
}

#[test]
fn fit_dims_shrinks_proportionally() {
    assert_eq!(fit_dims(7360, 4912, 500, 500), (500, 334));
    assert_eq!(fit_dims(4912, 7360, 500, 500), (334, 500));
}

#[test]
fn fit_dims_never_enlarges() {
    assert_eq!(fit_dims(300, 200, 500, 500), (300, 200));
}

#[test]
fn probe_display_matches_probe_on_unrotated_jpeg() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/photo.jpg"
    ))
    .unwrap();
    let a = probe(&bytes).unwrap();
    let b = probe_display(&bytes).unwrap();
    assert_eq!(a, b);
}

/// Counts what `probe_reader` pulled from its source.
struct CountingReader<R> {
    inner: R,
    read: usize,
}

impl<R: std::io::Read> std::io::Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n;
        Ok(n)
    }
}

fn probe_counting(bytes: &[u8]) -> (Result<(ImageFormat, usize, usize), Error>, usize) {
    let mut r = CountingReader {
        inner: bytes,
        read: 0,
    };
    let got = probe_reader(&mut r);
    (got, r.read)
}

/// Both probes' answers, comparable: an error by kind and full message.
fn probe_outcome(
    r: Result<(ImageFormat, usize, usize), Error>,
) -> Result<(ImageFormat, usize, usize), String> {
    r.map_err(|e| format!("{:?}: {:#}", e.kind(), e))
}

#[test]
fn probe_reader_agrees_with_probe_on_every_fixture() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let mut seen = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "jpg" | "png" | "webp" | "gif" | "avif") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        let (got, read) = probe_counting(&bytes);
        assert_eq!(
            probe_outcome(got),
            probe_outcome(probe(&bytes)),
            "{}",
            path.display()
        );
        // A header that fits the first read is all that gets read,
        // except an AVIF's, which is read through EOF.
        if bytes.len() > PROBE_FIRST_READ && probe(&bytes[..PROBE_FIRST_READ]).is_ok() {
            let want = if ext == "avif" {
                bytes.len()
            } else {
                PROBE_FIRST_READ
            };
            assert_eq!(read, want, "{}", path.display());
        }
        seen += 1;
    }
    assert!(seen >= 30, "only {seen} fixtures probed");
}

/// photo.jpg with `segments` maximum-size APP2 segments ahead of its
/// frame header and `trailing` bytes after its EOI.
fn jpeg_with_padding(segments: usize, trailing: usize) -> Vec<u8> {
    let src = read_fixture("photo.jpg");
    let mut out = src[..2].to_vec();
    for _ in 0..segments {
        out.extend_from_slice(&[0xFF, 0xE2, 0xFF, 0xFF]);
        out.resize(out.len() + 0xFFFF - 2, 0);
    }
    out.extend_from_slice(&src[2..]);
    out.resize(out.len() + trailing, 0);
    out
}

#[test]
fn probe_reader_stops_at_the_first_read_that_holds_the_header() {
    let bytes = jpeg_with_padding(0, 4 << 20);
    let (got, read) = probe_counting(&bytes);
    assert_eq!(got.unwrap(), (ImageFormat::Jpeg, 200, 150));
    assert_eq!(read, PROBE_FIRST_READ);
}

#[test]
fn probe_reader_reads_past_metadata_larger_than_the_first_read() {
    // ~131 KB of APP2 ahead of the frame header: 64 and 128 KiB fall
    // short, 256 KiB holds it, and the 4 MiB behind it stay unread.
    let bytes = jpeg_with_padding(2, 4 << 20);
    let (got, read) = probe_counting(&bytes);
    assert_eq!(got.unwrap(), (ImageFormat::Jpeg, 200, 150));
    assert_eq!(read, 4 * PROBE_FIRST_READ);
}

#[test]
fn probe_reader_reports_the_error_of_the_whole_source() {
    // A short prefix of a good JPEG and a corrupt one fail alike, so a
    // failure is final only at EOF, and then it is the full file's.
    let mut bytes = jpeg_with_padding(0, 1 << 20);
    bytes[2..200].fill(0x55);
    let (got, read) = probe_counting(&bytes);
    assert_eq!(read, bytes.len());
    assert_eq!(probe_outcome(got), probe_outcome(probe(&bytes)));
    assert!(probe(&bytes).is_err());
}

/// An animated AVIF whose `ispe` answers on the first 64 KiB, while a
/// malformed box after them makes `probe` fail on the whole file: the
/// size-0 `ipco` runs to EOF, so on a prefix its walk ends cleanly at
/// the prefix's end, and on the full bytes it reaches the bad box.
#[cfg(feature = "avif")]
#[test]
fn probe_reader_reads_an_avif_through_eof() {
    fn bx(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + payload.len()).unwrap();
        [&size.to_be_bytes()[..], typ, payload].concat()
    }
    // Header of a box that extends to the end of its parent.
    fn to_end(typ: &[u8; 4]) -> Vec<u8> {
        [&[0u8; 4][..], typ].concat()
    }
    let mut bytes = [
        bx(b"ftyp", b"avis\0\0\0\0avis"),
        to_end(b"meta"),
        vec![0; 4], // FullBox version/flags
        bx(b"pitm", &[0, 0, 0, 0, 0, 1]),
        to_end(b"iprp"),
        // Item 1 → property 1 (the `ispe`), ahead of the size-0 `ipco`.
        bx(b"ipma", &[0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 1]),
        to_end(b"ipco"),
        bx(b"ispe", &[0, 0, 0, 0, 0, 0, 0, 64, 0, 0, 0, 48]),
    ]
    .concat();
    let pad = (64 << 10) - bytes.len();
    bytes.extend(bx(b"free", &vec![0; pad - 8]));
    // A box declaring 4 bytes cannot frame its own 8-byte header.
    bytes.extend([&4u32.to_be_bytes()[..], b"free"].concat());

    assert_eq!(
        probe(&bytes[..64 << 10]).unwrap(),
        (ImageFormat::Avif, 64, 48)
    );
    let whole = probe(&bytes);
    assert!(whole.is_err(), "{whole:?}");
    let (got, read) = probe_counting(&bytes);
    assert_eq!(probe_outcome(got), probe_outcome(whole));
    assert_eq!(read, bytes.len());
}

#[test]
fn probe_reader_classifies_a_read_failure() {
    use std::io::ErrorKind as Io;
    /// Serves `ok` bytes of a valid JPEG prefix, then fails, so the
    /// failure lands both on the first read and on a later one.
    struct Failing {
        bytes: Vec<u8>,
        ok: usize,
        kind: Io,
    }
    impl std::io::Read for Failing {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.ok == 0 {
                return Err(std::io::Error::new(self.kind, "reader detail"));
            }
            let n = out.len().min(self.ok);
            let start = self.bytes.len() - self.ok;
            out[..n].copy_from_slice(&self.bytes[start..start + n]);
            self.ok -= n;
            Ok(n)
        }
    }
    // Header behind 131 KB of APP2, so the first 64 KiB parse fails and
    // the reader is asked again.
    let bytes = jpeg_with_padding(2, 0);
    for (kind, want) in [
        (Io::PermissionDenied, ErrorKind::SourceUnreadable),
        (Io::Other, ErrorKind::SourceUnreadable),
        (Io::TimedOut, ErrorKind::SourceUnreadable),
        (Io::ConnectionReset, ErrorKind::SourceUnreadable),
        (Io::UnexpectedEof, ErrorKind::SourceUnreadable),
        (Io::NotFound, ErrorKind::SourceNotFound),
        (Io::FileTooLarge, ErrorKind::SourceTooLarge),
    ] {
        for ok in [0, 64 << 10] {
            let err = probe_reader(Failing {
                bytes: bytes.clone(),
                ok,
                kind,
            })
            .unwrap_err();
            assert_eq!(err.kind(), want, "{kind:?} after {ok} bytes");
            // The reader's own error survives as the cause.
            let cause = std::error::Error::source(&err)
                .and_then(|c| c.downcast_ref::<std::io::Error>())
                .unwrap_or_else(|| panic!("{kind:?}: no io cause in {err:#}"));
            assert_eq!(cause.kind(), kind);
        }
    }
}

#[test]
fn band_resize_matches_single_thread() {
    // Synthetic gradient image; verify 2/3-band parallel resize is
    // byte-identical to the single-threaded output.
    let (sw, sh, dw, dh) = (317usize, 211usize, 123usize, 81usize);
    let src: Vec<u8> = (0..sw * sh * 3).map(|i| ((i * 7919) % 251) as u8).collect();
    let mut single = vec![0u8; dw * dh * 3];
    resize_bands(
        &src,
        sw,
        sh,
        &mut single,
        dw,
        dh,
        PixelType::U8x3,
        1,
        &mut None,
    )
    .unwrap();
    for threads in [2, 3] {
        let mut banded = vec![0u8; dw * dh * 3];
        resize_bands(
            &src,
            sw,
            sh,
            &mut banded,
            dw,
            dh,
            PixelType::U8x3,
            threads,
            &mut None,
        )
        .unwrap();
        assert_eq!(single, banded, "threads={threads} output differs");
    }
}

#[test]
fn band_resize_keeps_fractional_bottom_inside_source() {
    // Both shapes round the last band's bottom past the source height.
    for (sw, sh, dw, dh) in [(200, 150, 156, 117), (24, 1600, 16, 1067)] {
        for px in [
            PixelType::U8x3,
            PixelType::U8x4,
            PixelType::U16x3,
            PixelType::U16x4,
        ] {
            let src = vec![0x55; sw * sh * px.size()];
            let mut dst = vec![0; dw * dh * px.size()];
            resize_bands(&src, sw, sh, &mut dst, dw, dh, px, 4, &mut None).unwrap();
            assert!(dst.iter().all(|&v| v == 0x55), "{sw}x{sh} {px:?}");
        }
    }
}

#[test]
fn luts_roundtrip_every_srgb_value() {
    // back(fwd(v)) must be the identity for all 256 sRGB values, or
    // unresized regions would shift colors through the linear path.
    let (fwd, back) = (fwd_lut(), back_lut());
    for v in 0..=255u8 {
        assert_eq!(back[fwd[v as usize] as usize], v, "value {v}");
    }
}

#[test]
fn preset_parsing_maps_and_defaults() {
    assert_eq!(Encoder::from_preset("fast"), Encoder::MozFast);
    assert_eq!(Encoder::from_preset("small"), Encoder::MozSmall);
    assert_eq!(Encoder::from_preset("jpegli"), Encoder::Jpegli);
    assert_eq!(Encoder::from_preset(""), Encoder::Jpegli);
    assert_eq!(Encoder::from_preset("bogus"), Encoder::Jpegli);
}

#[test]
fn content_types_match_formats() {
    assert_eq!(ImageFormat::Jpeg.content_type(), "image/jpeg");
    assert_eq!(ImageFormat::Png.content_type(), "image/png");
    assert_eq!(ImageFormat::Webp.content_type(), "image/webp");
    assert_eq!(ImageFormat::Avif.content_type(), "image/avif");
    assert_eq!(ImageFormat::Gif.content_type(), "image/gif");
}

/// GIF decodes but never encodes, so it must not be reachable as an
/// output format: no token names it, and asking for one is a client
/// error rather than a silent substitution (`@gif` is rejected in
/// `split_format`, `format=gif` in `options`).
#[test]
fn gif_is_not_an_output_format() {
    assert_eq!(ImageFormat::from_token("gif"), None);
    assert_eq!(default_target(ImageFormat::Gif), ImageFormat::Webp);
    for f in [
        ImageFormat::Jpeg,
        ImageFormat::Png,
        ImageFormat::Webp,
        ImageFormat::Avif,
    ] {
        assert_eq!(default_target(f), f, "encodable formats keep themselves");
    }
    assert!(!target_supports_icc(ImageFormat::Gif));
}

#[test]
fn sniff_detects_formats_by_magic_bytes() {
    let jpeg = *b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01";
    assert_eq!(ImageFormat::sniff(&jpeg), Some(ImageFormat::Jpeg));
    let png = *b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0D";
    assert_eq!(ImageFormat::sniff(&png), Some(ImageFormat::Png));
    let webp = *b"RIFF\x00\x01\x00\x00WEBP";
    assert_eq!(ImageFormat::sniff(&webp), Some(ImageFormat::Webp));
    assert_eq!(
        ImageFormat::sniff(b"\x00\x00\x00\x1cftypavif"),
        Some(ImageFormat::Avif)
    );
    // Both signatures: GIF87a is still out there, and a decoder that
    // only knew GIF89a would reject a decade of files.
    assert_eq!(
        ImageFormat::sniff(b"GIF89a\x00\x00\x00\x00\x00\x00"),
        Some(ImageFormat::Gif)
    );
    assert_eq!(
        ImageFormat::sniff(b"GIF87a\x00\x00\x00\x00\x00\x00"),
        Some(ImageFormat::Gif)
    );
    assert_eq!(ImageFormat::sniff(b"GIF88a\x00\x00\x00\x00\x00\x00"), None);
}

/// Deterministic 4-component JPEG source: plain CMYK (Adobe APP14
/// transform 0) or YCCK (transform 2). The scanlines are written in
/// libjpeg's stored convention, which is Adobe-inverted (0 = full
/// ink, 255 = no ink).
fn make_cmyk_jpeg(w: usize, h: usize, ycck: bool) -> Vec<u8> {
    let mut px = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            // Smooth gradients survive DCT quantization better than
            // noise, keeping pixel-level assertions meaningful.
            px.push((x * 255 / w.max(1)) as u8);
            px.push((y * 255 / h.max(1)) as u8);
            px.push(((x + y) * 255 / (w + h)) as u8);
            px.push(255 - (y * 128 / h.max(1)) as u8);
        }
    }
    let mut comp = Compress::new(ColorSpace::JCS_CMYK);
    if ycck {
        comp.set_color_space(ColorSpace::JCS_YCCK);
    }
    comp.set_size(w, h);
    comp.set_quality(95.0);
    let mut started = comp.start_compress(Vec::new()).unwrap();
    started.write_scanlines(&px).unwrap();
    started.finish().unwrap()
}

/// CMYK and YCCK sources decode to RGB via the naive composite
/// (`r = c'·k'/255` on the stored Adobe-inverted samples). The
/// reference applies the same formula to the raw CMYK planes decoded
/// by libjpeg itself, pinning oximg's plumbing (YCCK normalization,
/// in-place 4→3 compaction, the exact-size resize join)
/// byte-for-byte; independent third-party ground truth lives in
/// tests/formats_cmyk.rs against committed djpeg references.
#[test]
fn cmyk_and_ycck_sources_decode_to_naive_rgb() {
    for ycck in [false, true] {
        let jpeg = make_cmyk_jpeg(64, 48, ycck);
        let (rgb, w, h) = decode_and_resize(&jpeg, 64, 48, 1).unwrap();
        assert_eq!((w, h), (64, 48), "ycck={ycck}");
        let dec = mozjpeg::Decompress::new_mem(&jpeg).unwrap();
        assert_eq!(
            dec.color_space(),
            if ycck {
                ColorSpace::JCS_YCCK
            } else {
                ColorSpace::JCS_CMYK
            }
        );
        let mut started = dec.to_colorspace(ColorSpace::JCS_CMYK).unwrap();
        let planes: Vec<u8> = started.read_scanlines().unwrap();
        started.finish().unwrap();
        let want: Vec<u8> = planes
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|px| {
                let k = px[3] as u32;
                [0, 1, 2].map(|c| ((px[c] as u32 * k + 127) / 255) as u8)
            })
            .collect();
        assert_eq!(rgb, want, "ycck={ycck}");
    }
}

/// Byte-compare a CMYK decode through the pipeline against the same
/// shared machinery fed a reference decode: raw planes from libjpeg
/// (same DCT scale the pipeline picks) + the naive composite +
/// `resize_pixels_to` at the same `parallel`. Same backend on both
/// sides, so the expectation is exact.
fn assert_cmyk_matches_reference(jpeg: &[u8], box_px: u32, parallel: usize) {
    let (got, w, h) = decode_and_resize(jpeg, box_px, box_px, parallel).unwrap();

    let mut dec = mozjpeg::Decompress::new_mem(jpeg).unwrap();
    let (src_w, src_h) = dec.size();
    // Mirrors the pipeline's own choice: these fixtures are CMYK, the
    // buffered arm, which keeps shrink-on-load by default because
    // there it is buying memory rather than costing quality.
    let margin = dct_margin().unwrap_or(BUFFERED_DCT_MARGIN);
    dec.scale(dct_scale_num(src_w, src_h, w, h, Some(margin)));
    let mut started = dec.to_colorspace(ColorSpace::JCS_CMYK).unwrap();
    let (dec_w, dec_h) = (started.width(), started.height());
    let planes: Vec<u8> = started.read_scanlines().unwrap();
    started.finish().unwrap();
    let mut s = Scratch::default();
    let chunk = scratch_u8(&mut s.chunk8, dec_w * dec_h * 3);
    for (d, px) in chunk
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .zip(planes.as_chunks::<4>().0)
    {
        let k = px[3] as u32;
        for (c, &v) in px[..3].iter().enumerate() {
            d[c] = ((v as u32 * k + 127) / 255) as u8;
        }
    }
    let p = Resolved::new(&Params {
        parallel,
        ..Params::default()
    });
    resize_pixels_to(&mut s, 3, dec_w, dec_h, w, h, &p).unwrap();
    assert_eq!(
        got,
        &s.out8[..w * h * 3],
        "box={box_px} parallel={parallel} decode={dec_w}x{dec_h} out={w}x{h}"
    );
}

/// A CMYK decode that engages DCT shrink-on-load (num < 8 — the
/// full-size fixture comparisons never do) must byte-match the
/// reference-fed machinery: pins chunk8 sizing, the in-place 4→3
/// compaction, and the resize join at scaled dims.
#[test]
fn cmyk_dct_scaled_decode_matches_reference() {
    // 256x192 in a 64x64 box → fit 64x48; the buffered arm's 1.7
    // margin wants ≥109x82 → num=4, a 128x96 scaled decode ahead of
    // the resize. CMYK is that arm, so this is the default path here
    // even though the streaming arm no longer shrinks at all.
    assert_eq!(
        dct_scale_num(256, 192, 64, 48, Some(BUFFERED_DCT_MARGIN)),
        4
    );
    assert_cmyk_matches_reference(&make_cmyk_jpeg(256, 192, true), 64, 1);
}

/// The band-parallel arm of the CMYK resize join, pinned the same
/// way. (parallel=1 and parallel>1 legitimately take different
/// resize backends — bytes are stable per config, not across
/// OXIMG_PAR values — so the reference runs at the same parallel.)
#[test]
fn cmyk_band_parallel_matches_reference() {
    let jpeg = make_cmyk_jpeg(97, 61, true);
    let (_, w, h) = decode_and_resize(&jpeg, 48, 48, 2).unwrap();
    assert_eq!((w, h), (48, 30));
    assert_cmyk_matches_reference(&jpeg, 48, 2);
}

#[test]
fn dct_scale_picks_smallest_sufficient() {
    // 7360 * 1/8 = 920 >= 500 -> num = 1
    assert_eq!(dct_scale_num(7360, 4912, 500, 334, Some(1.0)), 1);
    // 1000 * 4/8 = 500 >= 500, 667*4/8=334 >= 334 -> num = 4
    assert_eq!(dct_scale_num(1000, 667, 500, 334, Some(1.0)), 4);
    // already at target size -> no scaling
    assert_eq!(dct_scale_num(500, 334, 500, 334, Some(1.0)), 8);
}

/// The default: no margin means no shrink-on-load, whatever the
/// reduction. Against the linear-light reference, shrink only ever
/// costs quality (bench/quality/dct_sweep.py), so it is opt-in — and
/// 5.3x, where the old 1.7
/// default selected 3/8, is the ratio this pins.
#[test]
fn no_margin_decodes_at_full_size() {
    assert_eq!(dct_scale_num(4000, 2667, 750, 500, None), 8);
    assert_eq!(dct_scale_num(7360, 4912, 500, 334, None), 8);
    assert_eq!(dct_scale_num(500, 334, 500, 334, None), 8);
}

/// Issue #14: the output format's dimension ceiling is one more
/// constraint on the same fit box. WebP caps a side at 16383, so a
/// tall source must come back scaled to fit — the largest WebP that
/// keeps the source's shape — instead of failing at the encoder.
#[test]
fn webp_output_is_clamped_to_the_format_ceiling() {
    let webp = |max_w: u32, max_h: u32| {
        let p = Params {
            max_width: max_w,
            max_height: max_h,
            output: Some(ImageFormat::Webp),
            ..Params::default()
        };
        let c = clamp_to_format(Resolved::new(&p), ImageFormat::Webp);
        (c.max_width, c.max_height)
    };
    // The reporter's case: width=1920 on a 2000x19708 source. The box
    // tightens, and fit_dims then lands on the imgproxy answer.
    assert_eq!(webp(1920, u32::MAX), (1920, 16383));
    assert_eq!(fit_dims(2000, 19708, 1920, 16383), (1663, 16383));
    // An unconstrained request on a tall source: both axes capped.
    assert_eq!(webp(u32::MAX, u32::MAX), (16383, 16383));
    assert_eq!(fit_dims(2000, 19708, 16383, 16383), (1663, 16383));
    // Boxes already inside the ceiling are untouched, and sources
    // inside it never move.
    assert_eq!(webp(800, 600), (800, 600));
    assert_eq!(fit_dims(300, 200, 16383, 16383), (300, 200));
    // Exactly at the limit stays at the limit (the cap is inclusive).
    assert_eq!(fit_dims(2000, 16383, 16383, 16383), (2000, 16383));

    // Only WebP has a ceiling worth enforcing: the same tall request
    // in another format keeps its full box.
    for target in [ImageFormat::Jpeg, ImageFormat::Png] {
        let p = Params {
            max_width: 1920,
            max_height: u32::MAX,
            output: Some(target),
            ..Params::default()
        };
        let c = clamp_to_format(Resolved::new(&p), target);
        assert_eq!(
            (c.max_width, c.max_height),
            (1920, u32::MAX),
            "{target:?} must not be clamped"
        );
    }
}

/// At exact size the full-frame paths hand the decoded frame to `out8`
/// (issue #129): the staged frame is the only one, until a rotation
/// grows the swapped-in `chunk8` back to a second. A resize keeps its
/// own output frame and rotates into the frame-sized `chunk8`.
#[test]
fn decode_cost_prices_the_exact_size_hand_off() {
    let p = &Resolved::new(&Params::default());
    let rot = crate::meta::Orientation::from_rot_mirror(1, None);
    let up = crate::meta::Orientation::UPRIGHT;
    let frame = 400 * 300 * 3;
    let cost = |dst: (usize, usize), o| {
        DecodeCost::staged(400, 300, 3)
            .with_full_frame_resize((400, 300), dst, 3, p)
            .with_exact_size_rotation((400, 300), dst, 3, o)
    };
    assert_eq!(cost((400, 300), up).bytes(), frame);
    assert_eq!(cost((400, 300), rot).bytes(), 2 * frame);
    let resized = cost((200, 150), up).bytes();
    assert!(resized > frame, "a resize holds its own output");
    assert_eq!(cost((200, 150), rot).bytes(), resized);
}

/// Issue #17's core claim in arithmetic: at equal pixel counts the
/// decode cost varies by more than an order of magnitude, so a pixel
/// cap cannot bound memory while a byte estimate can. The shapes and
/// the measured peaks come from the field validation on the issue.
#[test]
fn decode_cost_separates_what_pixel_counts_cannot() {
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    let p = &Resolved::new(&Params::default());

    // Baseline JPEG, 3980x59828 (238 MP) at width=1920, whose WebP
    // output clamps to 16383 tall: streaming, so only the output side
    // is resident — `out8` and the WebP encoder. Measured peak 92 MiB.
    let webp = Some(ImageFormat::Webp);
    let baseline = DecodeCost::streaming()
        .with_streamed_output(1090, 16383, 3)
        .with_encode(webp, 1090, 16383, 3, p);
    let ratio = mib(baseline.bytes()) / 92.0;
    assert!(
        (1.0..3.0).contains(&ratio),
        "baseline JPEG: {:.0} MiB vs 92 measured ({ratio:.2}x)",
        mib(baseline.bytes())
    );

    // RGB PNG, 2250x26115 (58.8 MP) at width=1920 (output clamped to
    // 16383 tall): full frame, plus the linear-light u16 copy, plus the
    // output side. Measured peak 592 MiB — the case the first attempt
    // under-estimated 3.5x, which is the dangerous direction.
    let png = DecodeCost::staged(2250, 26115, 3)
        .with_full_frame_resize((2250, 26115), (1411, 16383), 3, p)
        .with_encode(webp, 1411, 16383, 3, p)
        .with_compressed(12 << 20);
    let ratio = mib(png.bytes()) / 592.0;
    assert!(
        (1.0..2.0).contains(&ratio),
        "PNG: {:.0} MiB vs 592 measured ({ratio:.2}x)",
        mib(png.bytes())
    );

    // Progressive CMYK 4:4:4, ~150 MP at width=1920: four-channel
    // staging at the shrink-on-load size, plus coefficient arrays no
    // output size reduces. Measured peak 1231 MiB.
    // Explicitly at the margin this case was *measured* under, not at
    // the current default: the 1231 MiB figure below came off a run
    // with shrink-on-load engaged, and the estimator is what is being
    // checked here, not the default.
    let num = dct_scale_num(12247, 12247, 1920, 1920, Some(1.7)) as usize;
    let (dw, dh) = ((12247 * num).div_ceil(8), (12247 * num).div_ceil(8));
    let prog = DecodeCost::staged(dw, dh, 4)
        .with_full_frame_resize((dw, dh), (1920, 1920), 3, p)
        .with_progressive_coefficients(12247, 12247, 4);
    let ratio = mib(prog.bytes()) / 1231.0;
    assert!(
        (1.0..2.5).contains(&ratio),
        "progressive CMYK: {:.0} MiB vs 1231 measured ({ratio:.2}x)",
        mib(prog.bytes())
    );

    // Every estimate must sit *above* its measured peak: under-
    // estimating is what gets a container OOM-killed while the cap
    // reports itself satisfied.
    for (name, est, peak) in [
        ("baseline", baseline.bytes(), 92.0),
        ("png", png.bytes(), 592.0),
        ("progressive", prog.bytes(), 1231.0),
    ] {
        assert!(mib(est) >= peak, "{name} under-estimates its peak");
    }

    // The progressive term is output-independent: a smaller output
    // barely helps, which is why pixel- and output-based reasoning
    // both fail on those sources.
    let smaller = DecodeCost::staged(dw / 2, dh / 2, 4)
        .with_full_frame_resize((dw / 2, dh / 2), (480, 480), 3, p)
        .with_progressive_coefficients(12247, 12247, 4);
    assert!(
        smaller.bytes() > prog.bytes() / 2,
        "progressive cost does not scale with the output"
    );

    // And the spread the issue is about: cost per *source* pixel across
    // these shapes spans an order of magnitude.
    let per_px = |c: &DecodeCost, src_px: f64| c.bytes() as f64 / src_px;
    let cheap = per_px(&baseline, 238e6);
    let dear = per_px(&prog, 150e6);
    assert!(
        dear / cheap > 10.0,
        "spread across shapes: {cheap:.3} vs {dear:.3} B per source pixel"
    );
}

/// Issue #130: the encoder's buffers count, priced by what decides
/// them — progressive vs sequential JPEG, quantized PNG, alpha WebP —
/// and nothing for a caller that keeps the pixels.
#[test]
fn encode_cost_follows_what_the_encoder_holds() {
    let p = |params: Params| Resolved::new(&params);
    let def = p(Params::default());
    let px = 4000 * 3000;

    // Below 16 MP jpegli holds every token (q80: 10 B/px); progressive
    // above OXIMG_JPEG_SEQUENTIAL_PIXELS (default 12000) adds the
    // coefficients, sequential below it does not.
    assert_eq!(encode_cost(ImageFormat::Jpeg, 4000, 3000, 3, &def), px * 17);
    assert_eq!(
        encode_cost(ImageFormat::Jpeg, 100, 100, 3, &def),
        100 * 100 * 10
    );
    // Tokens follow quality.
    let q = |quality| {
        p(Params {
            quality,
            ..Params::default()
        })
    };
    assert_eq!(
        encode_cost(ImageFormat::Jpeg, 100, 100, 3, &q(30.0)),
        100 * 100 * 4
    );
    assert_eq!(
        encode_cost(ImageFormat::Jpeg, 100, 100, 3, &q(100.0)),
        100 * 100 * 18
    );
    assert_eq!(
        encode_cost(ImageFormat::Jpeg, 4000, 3000, 3, &q(100.0)),
        px * 25
    );
    // Above OXIMG_JPEG_STREAMING_PIXELS (default 16 MP) the encoder
    // holds its output (up to twice it, by quality) and a few rows.
    let big = 5000 * 4000;
    for (quality, tenths) in [(30.0, 11), (50.0, 15), (80.0, 30), (90.0, 42), (100.0, 95)] {
        assert_eq!(
            encode_cost(ImageFormat::Jpeg, 5000, 4000, 3, &q(quality)),
            big * tenths / 10 + 5000 * 1024,
            "q{quality}"
        );
    }
    assert_eq!(
        encode_cost(ImageFormat::Jpeg, 4000, 4000, 3, &def),
        4000 * 4000 * 17,
        "16 MP exactly is not streamed"
    );
    // mozjpeg: the fastest profile flat, trellis by quality.
    for (encoder, per_px) in [(Encoder::MozFast, 10), (Encoder::MozSmall, 14)] {
        let moz = p(Params {
            encoder,
            ..Params::default()
        });
        assert_eq!(
            encode_cost(ImageFormat::Jpeg, 100, 100, 3, &moz),
            100 * 100 * per_px
        );
    }

    // PNG holds up to twice the raw frame plus its chunk buffer and three
    // rows; quantization (RGB only) adds quantette's working set.
    let rows = |ch: u64| 256 * 1024 + 3 * 4000 * ch;
    assert_eq!(
        encode_cost(ImageFormat::Png, 4000, 3000, 3, &def),
        px * 6 + rows(3)
    );
    assert_eq!(
        encode_cost(ImageFormat::Png, 4000, 3000, 4, &def),
        px * 8 + rows(4)
    );
    let quant = p(Params {
        png_quantize: Some(true),
        ..Params::default()
    });
    assert_eq!(
        encode_cost(ImageFormat::Png, 4000, 3000, 3, &quant),
        px * 20 + rows(3)
    );
    assert_eq!(
        encode_cost(ImageFormat::Png, 4000, 3000, 4, &quant),
        px * 8 + rows(4)
    );

    // WebP alpha is VP8L-coded, several times the RGB cost at any
    // quality; RGB follows quality (default 75). Both carry ~4 MiB that
    // does not scale.
    let fixed = 4u64 << 20;
    assert_eq!(
        encode_cost(ImageFormat::Webp, 4000, 3000, 3, &def),
        px * 8 + fixed
    );
    assert_eq!(
        encode_cost(ImageFormat::Webp, 4000, 3000, 4, &def),
        px * 44 + fixed
    );
    let webp = |webp_quality| {
        p(Params {
            webp_quality: Some(webp_quality),
            ..Params::default()
        })
    };
    assert_eq!(
        encode_cost(ImageFormat::Webp, 4000, 3000, 3, &webp(90.0)),
        px * 10 + fixed
    );
    assert_eq!(
        encode_cost(ImageFormat::Webp, 4000, 3000, 3, &webp(100.0)),
        px * 15 + fixed
    );
    assert_eq!(
        encode_cost(ImageFormat::Webp, 4000, 3000, 4, &webp(100.0)),
        px * 44 + fixed
    );

    // Nothing encodes GIF.
    assert_eq!(encode_cost(ImageFormat::Gif, 4000, 3000, 4, &def), 0);

    // `None` is a caller that returns pixels: no encoder runs.
    let cost = DecodeCost::streaming().with_encode(None, 4000, 3000, 3, &def);
    assert_eq!(cost.encode_bytes, 0);
    let cost = DecodeCost::streaming().with_encode(Some(ImageFormat::Webp), 4000, 3000, 3, &def);
    assert_eq!(cost.bytes(), px * 8 + fixed);
    assert!(
        cost.report("JPEG")
            .contains(&format!("encode {}", px * 8 + fixed))
    );
}

/// OXIMG_WEBP_EFFORT moves libwebp's working set more than quality
/// does: from method 3 RGB holds three times as much, and method 6's
/// lossless alpha search twice the rest.
#[test]
fn webp_cost_follows_the_method() {
    for method in 0..=2 {
        assert_eq!(webp_cost_per_px(3, 75.0, method), 8);
        assert_eq!(webp_cost_per_px(3, 90.0, method), 10);
        assert_eq!(webp_cost_per_px(3, 100.0, method), 15);
        assert_eq!(webp_cost_per_px(4, 100.0, method), 44);
    }
    for method in 3..=6 {
        assert_eq!(webp_cost_per_px(3, 50.0, method), 22);
        assert_eq!(webp_cost_per_px(3, 90.0, method), 29);
        assert_eq!(webp_cost_per_px(3, 100.0, method), 38);
    }
    for method in 3..=5 {
        assert_eq!(webp_cost_per_px(4, 75.0, method), 44);
        assert_eq!(webp_cost_per_px(4, 90.0, method), 50);
        assert_eq!(webp_cost_per_px(4, 100.0, method), 56);
    }
    assert_eq!(webp_cost_per_px(4, 75.0, 6), 110);
    assert_eq!(webp_fixed_cost(4, 6), 32 << 20);
    for method in 0..=5 {
        assert_eq!(webp_fixed_cost(4, method), 4 << 20);
    }
    for method in 0..=6 {
        assert_eq!(webp_fixed_cost(3, method), 4 << 20);
    }
}

/// SVT-AV1's working set grows as the preset slows and above quality
/// 90, and the alpha session adds half again; the fixed part does not
/// scale.
#[cfg(feature = "avif")]
#[test]
fn avif_encode_cost_follows_the_preset_and_quality() {
    let px = 2000 * 2000;
    let fixed = 16u64 << 20;
    let at = |speed, quality, alpha_quality| crate::avif::AvifParams {
        speed,
        quality,
        alpha_quality,
        ..Default::default()
    };
    assert_eq!(avif_encode_cost(px, 3, &at(8, 60, 60)), px * 32 + fixed);
    assert_eq!(avif_encode_cost(px, 3, &at(13, 60, 60)), px * 32 + fixed);
    assert_eq!(avif_encode_cost(px, 3, &at(4, 60, 60)), px * 44 + fixed);
    assert_eq!(avif_encode_cost(px, 3, &at(2, 60, 60)), px * 56 + fixed);
    assert_eq!(avif_encode_cost(px, 3, &at(0, 60, 60)), px * 56 + fixed);
    assert_eq!(avif_encode_cost(px, 4, &at(8, 60, 60)), px * 48 + fixed);
    // Above quality 90, a third more; for alpha, either quality counts.
    assert_eq!(avif_encode_cost(px, 3, &at(8, 100, 60)), px * 42 + fixed);
    assert_eq!(avif_encode_cost(px, 3, &at(8, 60, 100)), px * 32 + fixed);
    assert_eq!(avif_encode_cost(px, 4, &at(8, 60, 100)), px * 63 + fixed);
    // The resolved knobs, through the generic entry point.
    let def = Resolved::new(&Params::default());
    assert_eq!(
        encode_cost(ImageFormat::Avif, 2000, 2000, 3, &def),
        avif_encode_cost(px, 3, &def.avif_params())
    );
}

/// A GIF's one-bit alpha is priced on its own, but the RGB encode under
/// it follows the WebP quality like any other lossy encode.
#[test]
fn gif_webp_encode_follows_quality() {
    let at = |webp_quality| {
        Resolved::new(&Params {
            webp_quality: Some(webp_quality),
            ..Params::default()
        })
    };
    let px = 1000 * 1000;
    let fixed = 4 << 20;
    let same = ((1000, 1000), (1000, 1000));
    assert_eq!(
        gif_webp_encode_bytes(same.0, same.1, &at(75.0)),
        px * 12 + fixed
    );
    assert_eq!(
        gif_webp_encode_bytes(same.0, same.1, &at(100.0)),
        px * 19 + fixed
    );
    // A resize filters the one-bit alpha into 8-bit values: the general
    // RGBA price.
    assert_eq!(
        gif_webp_encode_bytes((2000, 2000), (1000, 1000), &at(75.0)),
        px * 44 + fixed
    );
}

/// The estimate the last `process` call on this thread recorded.
fn cost_of(src: &[u8], p: Params) -> DecodeCost {
    process(src, &p).unwrap();
    LAST_COST.get().expect("a decode just ran").0
}

fn sized(max_width: u32, max_height: u32) -> Params {
    Params {
        max_width,
        max_height,
        ..Params::default()
    }
}

/// fir's intermediate for a full-frame resize, when the dispatch takes
/// fir on this machine (it depends on the CPU and the knobs).
fn fir_bytes(px: PixelType, dst_h: usize, intermediate: u64) -> u64 {
    if resize_uses_fir(px, 1, dst_h) {
        intermediate
    } else {
        0
    }
}

/// Issue #130: every JPEG arm is priced by the buffers it holds. A
/// 320x240 source into a 200x200 box decodes at full size (a 1.6x
/// reduction engages no shrink-on-load) and resizes to 200x150.
#[test]
fn decode_cost_prices_each_jpeg_arm() {
    let jpeg = make_test_jpeg(320, 240, false);
    let (src, dst) = (320 * 240, 200 * 150);

    // Exact size to JPEG from a single scan: one band of rows goes from
    // the decoder to the encoder at a time.
    let c = cost_of(&make_baseline_jpeg(320, 240, false), sized(320, 240));
    assert_eq!(
        (c.staged_bytes, c.resize_input_bytes, c.output_bytes),
        (0, 0, 320 * 16 * 3)
    );
    // From a multi-scan source (this one), or to anything else, libjpeg
    // writes straight into `out8`.
    let c = cost_of(&jpeg, sized(320, 240));
    assert_eq!(
        (c.staged_bytes, c.resize_input_bytes, c.output_bytes),
        (0, 0, src * 3)
    );
    let png = Params {
        output: Some(ImageFormat::Png),
        ..sized(320, 240)
    };
    let c = cost_of(&jpeg, png);
    assert_eq!(
        (c.staged_bytes, c.resize_input_bytes, c.output_bytes),
        (0, 0, src * 3)
    );

    // The row kernel streams into `out8`: nothing full-frame on the
    // source side.
    if kernel_streams(&rbox(200, 200)) {
        let c = cost_of(&jpeg, sized(200, 200));
        assert_eq!(
            (c.staged_bytes, c.resize_input_bytes, c.output_bytes),
            (0, 0, dst * 3)
        );
    }

    // sRGB mode decodes the whole frame to u8 and resizes it there.
    let c = cost_of(
        &jpeg,
        Params {
            linear_light: Some(false),
            ..sized(200, 200)
        },
    );
    assert_eq!(c.staged_bytes, src * 3);
    assert_eq!(
        c.resize_input_bytes,
        fir_bytes(PixelType::U8x3, 150, 320 * 150 * 3)
    );
    assert_eq!(c.output_bytes, dst * 3);

    // Band-parallel takes the full-frame fallback: `src16` filled from
    // row chunks, every band's fir intermediate, `dst16` + `out8`.
    let c = cost_of(
        &jpeg,
        Params {
            parallel: 4,
            ..sized(200, 200)
        },
    );
    assert_eq!(c.staged_bytes, 0);
    assert_eq!(c.resize_input_bytes, src * 6 + 200 * 240 * 6);
    assert_eq!(c.output_bytes, dst * 9);

    // Rotated output: the kernel arm's `chunk8` held row chunks, so the
    // rotation lands in a second output frame. (Orientation 6 displays
    // 320x240 as 240x320; the box fits it to 150x200, stored 200x150.)
    let rotated = {
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&tiff_orientation(6));
        let mut comp = Compress::new(ColorSpace::JCS_RGB);
        comp.set_size(320, 240);
        comp.set_quality(90.0);
        let mut started = comp.start_compress(Vec::new()).unwrap();
        started.write_marker(mozjpeg::Marker::APP(1), &exif);
        started
            .write_scanlines(&vec![128u8; 320 * 240 * 3])
            .unwrap();
        started.finish().unwrap()
    };
    if kernel_streams(&rbox(200, 200)) {
        let c = cost_of(&rotated, sized(150, 200));
        assert_eq!(
            (c.staged_bytes, c.resize_input_bytes, c.output_bytes),
            (0, 0, dst * 3 * 2)
        );
    }

    // CMYK stages four channels (at its shrink-on-load size), compacts
    // to RGB in place, and resizes like the buffered formats.
    let cmyk = make_cmyk_jpeg(320, 240, false);
    let num = dct_scale_num(320, 240, 200, 150, Some(BUFFERED_DCT_MARGIN)) as usize;
    let (dw, dh) = ((320 * num).div_ceil(8), (240 * num).div_ceil(8));
    let c = cost_of(&cmyk, sized(200, 200));
    assert_eq!(c.staged_bytes, (dw * dh * 4) as u64);
    if (dw, dh) != (200, 150) {
        assert_eq!(
            c.resize_input_bytes,
            (dw * dh * 6) as u64 + fir_bytes(PixelType::U16x3, 150, (200 * dh * 6) as u64)
        );
        assert_eq!(c.output_bytes, dst * 9);
    }
}

/// A multi-scan source's coefficient arrays are priced from libjpeg's
/// own header read, so they are counted even where the header pre-scan
/// does not run (no rotation, no profile wanted): the term must not
/// depend on which knobs are on.
#[test]
fn decode_cost_prices_coefficients_without_the_header_scan() {
    let bare = |p: Params| Params {
        auto_rotate: Some(false),
        icc: Some(false),
        ..p
    };
    for (jpeg, multi) in [
        (make_test_jpeg(320, 240, false), true),
        (make_baseline_jpeg(320, 240, false), false),
    ] {
        for p in [sized(320, 240), sized(200, 200)] {
            let c = cost_of(&jpeg, bare(p));
            let want = if multi { 320 * 240 * 3 * 2 } else { 0 };
            assert_eq!(c.whole_source_bytes, want, "multi={multi}");
        }
    }
}

/// `decode_and_resize` hands back its pixels in a Vec of their own, a
/// second output frame next to `out8`; an encoding caller does not.
#[test]
fn decode_cost_counts_the_frame_a_pixel_caller_keeps() {
    let jpeg = make_test_jpeg(320, 240, false);
    let (px, w, h) = decode_and_resize(&jpeg, 320, 240, 1).unwrap();
    assert_eq!((px.len(), w, h), (320 * 240 * 3, 320, 240));
    let (c, _) = LAST_COST.get().expect("a decode just ran");
    assert_eq!(c.output_bytes, 320 * 240 * 3 * 2);
    assert_eq!(c.encode_bytes, 0, "nothing is encoded");
    // The same request encoding to PNG keeps `out8` alone (to JPEG it
    // streams; see `decode_cost_prices_each_jpeg_arm`).
    let png = Params {
        output: Some(ImageFormat::Png),
        ..sized(320, 240)
    };
    let c = cost_of(&jpeg, png);
    assert_eq!(c.output_bytes, 320 * 240 * 3);
}

/// A tall source squeezed to one row keeps a whole column in the row
/// kernel's ring: one 16-float-padded row per source row (issue #129).
/// A fitted per-side constant priced 40x32000 -> 1x1 at 4.6 MB against
/// a 6.1 MB ring; the estimate must cover the ring wherever it streams.
#[test]
fn decode_cost_covers_the_stream_ring_of_a_tall_source() {
    let tall = black_white_lines_png(40, 32_000, 3);
    let c = cost_of(&tall, sized(1, 1));
    if bands_use_stream_kernel(PixelType::U16x3, sized(1, 1).parallel, 1) {
        let ring = 32_000 * 16 * 3 * 4;
        assert!(
            c.resize_input_bytes > ring,
            "resize input {} must cover the {ring}-byte ring",
            c.resize_input_bytes
        );
    } else {
        // Staged: the whole linear-light frame.
        assert!(c.resize_input_bytes >= 40 * 32_000 * 6);
    }
}

/// Issue #130: the PNG arm is chosen from the header before the
/// estimate runs, so the estimate prices that arm — not a staged frame
/// the fused arm never allocates, nor a resize an exact-size request
/// never runs.
#[test]
fn decode_cost_prices_each_png_arm() {
    let (src, dst) = (320 * 240, 200 * 150);

    // RGB8 with a reduction: the fused arm. Where the row kernel runs
    // (aarch64, one band; issue #129) rows stream into it, one mapped
    // source row at a time, and `out8` is the only frame; elsewhere they
    // land in `src16` for a full-frame resize into `dst16`.
    let rgb = black_white_lines_png(320, 240, 3);
    let c = cost_of(&rgb, sized(200, 200));
    assert_eq!(c.staged_bytes, 0);
    if bands_use_stream_kernel(PixelType::U16x3, sized(200, 200).parallel, 150) {
        let kernel =
            crate::resize_kernel::StreamResize::<FuseKernel>::resident_bytes(320, 240, 200, 150, 3);
        assert_eq!(
            (c.resize_input_bytes, c.output_bytes),
            (320 * 6 + kernel, dst * 3)
        );
    } else {
        assert_eq!(
            c.resize_input_bytes,
            src * 6 + fir_bytes(PixelType::U16x3, 150, 200 * 240 * 6)
        );
        assert_eq!(c.output_bytes, dst * 9);
    }

    // Exact size: `chunk8` handed to `out8` (issue #129), no resize
    // buffers and no second frame.
    let c = cost_of(&rgb, sized(320, 240));
    assert_eq!(
        (c.staged_bytes, c.resize_input_bytes, c.output_bytes),
        (src * 3, 0, 0)
    );

    // RGBA takes the general arm: staged, premultiplied into `src16`.
    let rgba = black_white_lines_png(320, 240, 4);
    let c = cost_of(&rgba, sized(200, 200));
    assert_eq!(c.staged_bytes, src * 4);
    assert_eq!(
        c.resize_input_bytes,
        src * 8 + fir_bytes(PixelType::U16x4, 150, 200 * 240 * 8)
    );
    assert_eq!(c.output_bytes, dst * 12);

    // eXIf orientation 6 is read before the estimate: a 400x100 source
    // displays as 100x400, which fits a 120-wide box as it is. The
    // unoriented fit (120x30) would have priced a resize.
    let mut oriented = Vec::new();
    {
        let mut info = png::Info::with_size(400, 100);
        info.exif_metadata = Some(std::borrow::Cow::Owned(tiff_orientation(6)));
        info.color_type = png::ColorType::Rgb;
        info.bit_depth = png::BitDepth::Eight;
        let enc = png::Encoder::with_info(&mut oriented, info).unwrap();
        enc.write_header()
            .unwrap()
            .write_image_data(&vec![128u8; 400 * 100 * 3])
            .unwrap();
    }
    let c = cost_of(&oriented, sized(120, 10_000));
    assert_eq!(
        (c.staged_bytes, c.resize_input_bytes, c.output_bytes),
        (400 * 100 * 3, 0, 400 * 100 * 3)
    );
    // The encoder sees the rotated frame: its row allowance is three
    // rows of the displayed width (100), not the stored one (400).
    assert_eq!(c.encode_bytes, 400 * 100 * 6 + 256 * 1024 + 3 * 100 * 3);
}

/// A little-endian TIFF carrying only an Orientation tag.
fn tiff_orientation(o: u16) -> Vec<u8> {
    let mut t = b"II*\0\x08\0\0\0".to_vec();
    t.extend_from_slice(&1u16.to_le_bytes()); // one entry
    t.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    t.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    t.extend_from_slice(&1u32.to_le_bytes()); // count
    t.extend_from_slice(&o.to_le_bytes());
    t.extend_from_slice(&[0, 0]); // value padding
    t.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    t
}

/// Issue #130: libwebp's VP8L decoder holds the whole image as ARGB
/// words — lossless sources and the alpha plane of lossy ones — so
/// those are priced above an opaque lossy source at the same size.
#[test]
fn decode_cost_prices_webp_alpha_decoding() {
    let webp_of = |channels: usize| {
        // A real alpha ramp: libwebp drops an all-opaque alpha plane.
        let mut px = black_white_lines(320, 240, channels);
        if channels == 4 {
            for (i, a) in px.iter_mut().skip(3).step_by(4).enumerate() {
                *a = (i % 256) as u8;
            }
        }
        let mut png = Vec::new();
        let mut enc = png::Encoder::new(&mut png, 320, 240);
        enc.set_color(if channels == 4 {
            png::ColorType::Rgba
        } else {
            png::ColorType::Rgb
        });
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&px).unwrap();
        process(
            &png,
            &Params {
                output: Some(ImageFormat::Webp),
                ..sized(320, 240)
            },
        )
        .unwrap()
        .0
    };
    let src = 320 * 240;
    let c = cost_of(&webp_of(3), sized(320, 240));
    assert_eq!(c.staged_bytes, src * 3, "lossy RGB streams rows");
    let c = cost_of(&webp_of(4), sized(320, 240));
    assert_eq!(c.staged_bytes, src * 4 + src * 4, "the alpha plane is VP8L");
}

/// Issue #130: dav1d's pictures and frame state sit next to `chunk8`
/// (and stay resident through the resize), so an AVIF source is priced
/// above its RGBA frame alone.
#[cfg(feature = "avif")]
#[test]
fn decode_cost_prices_dav1d_next_to_the_frame() {
    let png = black_white_lines_png(320, 240, 3);
    let (avif, _) = process(
        &png,
        &Params {
            output: Some(ImageFormat::Avif),
            ..sized(320, 240)
        },
    )
    .unwrap();
    let c = cost_of(&avif, sized(320, 240));
    let src = 320 * 240;
    assert_eq!(c.staged_bytes, src * 4 + src * 12);
    // Exact size: the frame is handed to `out8` (issue #129).
    assert_eq!((c.resize_input_bytes, c.output_bytes), (0, 0));
}

/// Issue #22: a caller-held source buffer is resident for the whole
/// decode, so the in-memory entry point must count it in the estimate
/// — the streaming-JPEG path otherwise carries no compressed term at
/// all, and a buffered remote source would be under-counted by exactly
/// its own size (the direction that gets a container OOM-killed).
/// The streaming entry points must keep counting zero.
#[test]
fn caller_held_source_counts_in_the_decode_estimate() {
    let jpeg = make_test_jpeg(320, 240, false);
    let p = Params {
        max_width: 100,
        max_height: 100,
        ..Params::default()
    };

    process(&jpeg, &p).unwrap();
    let (cost, _) = LAST_COST.get().expect("a decode just ran");
    assert_eq!(
        cost.compressed_bytes,
        jpeg.len() as u64,
        "process(&bytes) must count the caller's buffer"
    );

    let dir = std::env::temp_dir().join(format!("oximg-held-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("held.jpg");
    std::fs::write(&path, &jpeg).unwrap();
    process_path(&path, &p).unwrap();
    let (cost, _) = LAST_COST.get().expect("a decode just ran");
    assert_eq!(
        cost.compressed_bytes, 0,
        "the streaming path holds no source buffer"
    );
    let transcode = |fmt: ImageFormat| {
        let (out, got) = process(
            &jpeg,
            &Params {
                output: Some(fmt),
                ..Params::default()
            },
        )
        .unwrap();
        assert_eq!(got, fmt);
        out
    };

    // PNG streams too (issue #129): the in-memory entry counts only the
    // caller's copy, and the file entry holds no source buffer at all.
    let png = transcode(ImageFormat::Png);
    process(&png, &p).unwrap();
    let (cost, _) = LAST_COST.get().expect("a decode just ran");
    assert_eq!(
        cost.compressed_bytes,
        png.len() as u64,
        "PNG decodes from the caller's slice: one copy, not two"
    );
    let path = dir.join("held.png");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&path, &png).unwrap();
    process_path(&path, &p).unwrap();
    let (cost, _) = LAST_COST.get().expect("a decode just ran");
    assert_eq!(
        cost.compressed_bytes, 0,
        "a streamed PNG file holds no source buffer"
    );
    let _ = std::fs::remove_dir_all(&dir);

    // A buffered format counts both copies: srcbuf and the caller's.
    let webp = transcode(ImageFormat::Webp);
    process(&webp, &p).unwrap();
    let (cost, _) = LAST_COST.get().expect("a decode just ran");
    assert_eq!(
        cost.compressed_bytes,
        2 * webp.len() as u64,
        "a buffered format holds the source twice: srcbuf + the caller's copy"
    );
}

/// One-pixel black and white lines, downscaled exactly 2:1, have a
/// known correct answer and need no reference image. Each output pixel
/// averages one black and one white pixel, so the result is linear
/// 0.5, which is `1.055 * 0.5^(1/2.4) - 0.055` in sRGB, or about 188.
/// Averaging the encoded values gives 128 instead. One source per
/// decode path, since each checks the flag on its own (issue #7).
#[test]
fn two_to_one_downscale_averages_in_linear_light() {
    // 64x64 keeps the source cheap while leaving plenty of interior
    // rows to average.
    const W: usize = 64;
    const H: usize = 64;
    // RGB and RGBA carry the same pattern; the channel count is what
    // sends them down different decode paths. PNG is lossless, so the
    // one-pixel lines reach the resize intact. JPEG at quality 100 is
    // close enough to land on the same numbers.
    let png_src = black_white_lines_png(W, H, 3);
    let rgba_src = black_white_lines_png(W, H, 4);
    let jpeg_src = {
        let mut c = Compress::new(ColorSpace::JCS_RGB);
        c.set_size(W, H);
        c.set_quality(100.0);
        let mut c = c.start_compress(Vec::new()).unwrap();
        c.write_scanlines(&black_white_lines(W, H, 3)).unwrap();
        c.finish().unwrap()
    };

    for (label, src) in [("png", &png_src), ("jpeg", &jpeg_src), ("rgba", &rgba_src)] {
        for (linear, want) in [(true, 188.0), (false, 128.0)] {
            let (out, _) = process(
                src,
                &Params {
                    max_width: (W / 2) as u32,
                    max_height: (H / 2) as u32,
                    output: Some(ImageFormat::Png),
                    linear_light: Some(linear),
                    ..Params::default()
                },
            )
            .unwrap();
            let mean = interior_mean(&out);
            assert!(
                (mean - want).abs() <= 2.0,
                "{label} source, linear_light={linear}: want ~{want}, got {mean:.1} \
                 (188 is the linear-light average, 128 the gamma-space one)"
            );
        }
    }
}

/// Rows of alternating black and white, one pixel tall, so a 2:1
/// shrink pairs one of each. `channels` is 3 for RGB or 4 for RGBA,
/// where alpha is fully opaque and premultiplying is a no-op.
fn black_white_lines(w: usize, h: usize, channels: usize) -> Vec<u8> {
    let mut px = Vec::with_capacity(w * h * channels);
    for y in 0..h {
        let v = if y % 2 == 0 { 0u8 } else { 255u8 };
        for _ in 0..w {
            px.extend_from_slice(&[v, v, v]);
            if channels == 4 {
                px.push(255);
            }
        }
    }
    px
}

/// [`black_white_lines`] encoded as a lossless PNG.
fn black_white_lines_png(w: usize, h: usize, channels: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
    enc.set_color(if channels == 4 {
        png::ColorType::Rgba
    } else {
        png::ColorType::Rgb
    });
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .unwrap()
        .write_image_data(&black_white_lines(w, h, channels))
        .unwrap();
    out
}

/// Mean of the red channel over the interior rows of a PNG.
///
/// The first and last two rows are excluded. A Lanczos window at the
/// edge falls partly outside the image, so those rows do not average
/// one black and one white pixel. On this fixture the edge rows read
/// 173/191 and 184/201 while every interior row reads exactly 188, so
/// the skew reaches two rows at each end.
fn interior_mean(png_bytes: &[u8]) -> f64 {
    let mut r = png::Decoder::new(std::io::Cursor::new(png_bytes))
        .read_info()
        .unwrap();
    let mut buf = vec![0u8; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut buf).unwrap();
    assert_eq!((info.width, info.height), (32, 32));
    // RGB or RGBA only: the loop below reads one byte per pixel as a
    // channel value. In an indexed PNG that byte is not a color.
    use png::ColorType::{Rgb, Rgba};
    assert!(
        matches!(info.color_type, Rgb | Rgba),
        "interior_mean needs RGB or RGBA, got {:?}",
        info.color_type
    );
    let stride = info.line_size;
    let channels = stride / info.width as usize;
    let (mut sum, mut n) = (0u64, 0u64);
    for y in 2..(info.height as usize - 2) {
        for x in 0..info.width as usize {
            sum += buf[y * stride + x * channels] as u64;
            n += 1;
        }
    }
    sum as f64 / n as f64
}

/// Sources across every decoder entry point: baseline, progressive,
/// CMYK and ICC-carrying JPEGs (the header pre-scan re-chains its
/// bytes in front of the stream), plus each other source format.
const SOURCE_FIXTURES: &[&str] = &[
    "photo.jpg",
    "tiny.jpg",
    "cmyk_prog.jpg",
    "cmyk_icc.jpg",
    "rgb.png",
    "interlaced.png",
    // PNG decodes straight from the reader (issue #129): every color
    // type and depth must come out the same through either entry.
    "rgba.png",
    "gray.png",
    "gray16.png",
    "graya.png",
    "palette.png",
    "rgb16.png",
    "photo.webp",
    "still.gif",
    "anim.gif",
    #[cfg(feature = "avif")]
    "photo.avif",
];

fn read_fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn in_memory_and_streaming_sources_encode_identically() {
    // `process` hands the decoder the caller's slice directly, while
    // `process_path` streams the file through a `BufReader`. How the
    // source is buffered must never reach the output bytes.
    let p = Params {
        max_width: 64,
        max_height: 64,
        ..Params::default()
    };
    let dir = std::env::temp_dir().join(format!("oximg-srcbuf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for name in SOURCE_FIXTURES {
        let bytes = read_fixture(name);
        let path = dir.join(name);
        std::fs::write(&path, &bytes).unwrap();
        let mem = process(&bytes, &p).unwrap_or_else(|e| panic!("{name}: {e}"));
        let file = process_path(&path, &p).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(mem.1, file.1, "{name}: output format");
        assert!(
            mem.0 == file.0,
            "{name}: in-memory and streaming bytes differ"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn jpeg_decode_is_independent_of_reader_chunking() {
    // libjpeg refills from whatever `fill_buf` returns, and the header
    // pre-scan consumes the same reader first. A reader that yields one
    // byte (or seven) at a time must decode to the same bytes as the
    // whole-slice reader `process` uses.
    let p = Params {
        max_width: 64,
        max_height: 64,
        ..Params::default()
    };
    for name in SOURCE_FIXTURES.iter().filter(|n| n.ends_with(".jpg")) {
        let bytes = read_fixture(name);
        let whole = process(&bytes, &p).unwrap_or_else(|e| panic!("{name}: {e}"));
        for cap in [1, 7] {
            let r = std::io::BufReader::with_capacity(cap, &bytes[..]);
            let chunked = process_reader(r, &p, bytes.len())
                .unwrap_or_else(|e| panic!("{name} at {cap}-byte reads: {e}"));
            assert!(
                whole.0 == chunked.0,
                "{name}: {cap}-byte reads changed the output"
            );
        }
    }
}

#[test]
fn truncated_in_memory_source_is_still_too_short() {
    // The slice reader must keep the 12-byte sniff's failure mode.
    let e = process(&[0xFF, 0xD8, 0xFF], &Params::default()).unwrap_err();
    assert!(
        format!("{e:#}").contains("source too short"),
        "unexpected error: {e:#}"
    );
}

/// The exact-size JPEG branch both ways: a JPEG target streams decoder
/// bands into the encoder (`Decoded::Encoded`), while the staged
/// reference leaves the frame in out8 for the separate encode that
/// `process_jpeg` used to run on every exact-size JPEG. Returns
/// (streamed, staged); either side's decode error comes back as Err.
fn exact_jpeg_both_ways(
    jpeg: &[u8],
    encoder: Encoder,
    orientation: crate::meta::Orientation,
    icc: Option<&[u8]>,
) -> (Result<Decoded>, Result<Vec<u8>>) {
    let p = Resolved::new(&Params {
        encoder,
        ..Params::default()
    });
    let run = |target: Option<ImageFormat>| -> Result<(Decoded, Scratch)> {
        crate::panic_guard::catch_unwind_as_error("test decode", || {
            let mut s = Scratch::default();
            let dec = Decompress::new_mem(jpeg)?;
            let d = decode_resize(&mut s, dec, &p, orientation, Fuse::Off, icc, target)?;
            Ok((d, s))
        })?
    };
    let staged = run(None).and_then(|(d, s)| match d {
        Decoded::Pixels { dst_w, dst_h } => {
            encode_with_icc(&s.out8[..dst_w * dst_h * 3], dst_w, dst_h, &p, icc)
        }
        _ => panic!("staged exact decode did not leave pixels"),
    });
    (run(Some(ImageFormat::Jpeg)).map(|(d, _)| d), staged)
}

fn encoded(d: Decoded) -> Vec<u8> {
    match d {
        Decoded::Encoded(out) => out,
        _ => panic!("exact-size JPEG did not stream"),
    }
}

/// Streaming the exact-size frame of a single-scan source into the
/// encoder writes the bytes the staged frame encodes to: every preset, sequential jpegli (100 x
/// 100 is under the default 12k-pixel sequential ceiling) and
/// progressive (799 x 601), a frame shorter than one band, RGB and
/// grayscale sources, and a profile spanning two APP2 chunks written
/// ahead of the first scanline.
#[test]
fn exact_size_jpeg_streams_the_staged_bytes() {
    let icc: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    for (w, h) in [(100, 100), (799, 601), (17, 1)] {
        for gray in [false, true] {
            let jpeg = make_baseline_jpeg(w, h, gray);
            for encoder in [Encoder::Jpegli, Encoder::MozFast, Encoder::MozSmall] {
                for icc in [None, Some(&icc[..])] {
                    let (streamed, staged) = exact_jpeg_both_ways(
                        &jpeg,
                        encoder,
                        crate::meta::Orientation::UPRIGHT,
                        icc,
                    );
                    let out = encoded(streamed.unwrap());
                    let case = format!("{w}x{h} gray={gray} {encoder:?} icc={}", icc.is_some());
                    assert!(out == staged.unwrap(), "{case}");
                    assert_eq!(
                        crate::meta::scan_jpeg_meta(&mut &out[..], &mut Vec::new(), true)
                            .icc
                            .as_deref(),
                        icc,
                        "{case}: profile"
                    );
                }
            }
        }
    }
}

/// What has work to do between decode and encode keeps the staged
/// frame: an oriented source (the rotation needs the whole frame) and
/// CMYK/YCCK (converted in a 4-channel buffer) come back as pixels
/// even with a JPEG target, and a resize is not the exact branch.
#[test]
fn exact_size_jpeg_streams_only_upright_rgb_frames() {
    let rotated = crate::meta::Orientation::from_rot_mirror(1, None);
    assert!(!rotated.is_upright());
    let jpeg = make_baseline_jpeg(64, 48, false);
    let (streamed, _) = exact_jpeg_both_ways(&jpeg, Encoder::Jpegli, rotated, None);
    assert!(matches!(
        streamed.unwrap(),
        Decoded::Pixels {
            dst_w: 64,
            dst_h: 48
        }
    ));
    for ycck in [false, true] {
        let cmyk = make_cmyk_jpeg(64, 48, ycck);
        let (streamed, _) = exact_jpeg_both_ways(
            &cmyk,
            Encoder::Jpegli,
            crate::meta::Orientation::UPRIGHT,
            None,
        );
        assert!(
            matches!(
                streamed.unwrap(),
                Decoded::Pixels {
                    dst_w: 64,
                    dst_h: 48
                }
            ),
            "ycck={ycck}"
        );
    }
    let mut s = Scratch::default();
    let dec = Decompress::new_mem(&jpeg).unwrap();
    let d = decode_resize(
        &mut s,
        dec,
        &rbox(32, 32),
        crate::meta::Orientation::UPRIGHT,
        Fuse::Off,
        None,
        Some(ImageFormat::Jpeg),
    )
    .unwrap();
    assert!(matches!(
        d,
        Decoded::Pixels {
            dst_w: 32,
            dst_h: 24
        }
    ));
}

/// A multi-scan (progressive) source keeps the staged frame for every
/// encoder that holds whole-image state, which is all of them below
/// OXIMG_JPEG_STREAMING_PIXELS: the decoder's coefficients would
/// otherwise overlap the encoder's. Staged, the bytes are the ones the
/// separate encode writes.
#[test]
fn exact_size_jpeg_stages_a_multi_scan_source() {
    let jpeg = make_test_jpeg(320, 240, false);
    let dec = Decompress::new_mem(&jpeg).unwrap();
    assert!(dec.has_multiple_scans());
    assert!(
        !Decompress::new_mem(&make_baseline_jpeg(320, 240, false))
            .unwrap()
            .has_multiple_scans()
    );
    for encoder in [Encoder::Jpegli, Encoder::MozFast, Encoder::MozSmall] {
        let (streamed, _) =
            exact_jpeg_both_ways(&jpeg, encoder, crate::meta::Orientation::UPRIGHT, None);
        assert!(
            matches!(
                streamed.unwrap(),
                Decoded::Pixels {
                    dst_w: 320,
                    dst_h: 240
                }
            ),
            "{encoder:?}"
        );
    }
}

/// A source cut mid-scan completes with libjpeg's fill data on both
/// paths, to the same bytes; one whose bytes after the scan hold a
/// malformed marker (read once the last row is decoded, so after the
/// streaming encoder has taken every row) fails on both with the
/// libjpeg message, and the streamed JPEG is discarded.
#[test]
fn exact_size_jpeg_streaming_keeps_truncation_and_error_behavior() {
    let jpeg = make_baseline_jpeg(799, 601, false);
    let cut = &jpeg[..jpeg.len() * 3 / 5];
    let (streamed, staged) = exact_jpeg_both_ways(
        cut,
        Encoder::Jpegli,
        crate::meta::Orientation::UPRIGHT,
        None,
    );
    assert!(encoded(streamed.unwrap()) == staged.unwrap(), "truncated");

    // A single-scan (baseline) source, so libjpeg decodes it lazily as
    // rows are read, with a DHT of impossible length (3) before EOI.
    let mut comp = Compress::new(ColorSpace::JCS_RGB);
    comp.set_fastest_defaults();
    comp.set_size(64, 48);
    let mut started = comp.start_compress(Vec::new()).unwrap();
    started.write_scanlines(&vec![128u8; 64 * 48 * 3]).unwrap();
    let mut bad = started.finish().unwrap();
    assert_eq!(bad[bad.len() - 2..], [0xFF, 0xD9]);
    let eoi = bad.len() - 2;
    bad.splice(eoi..eoi, [0xFF, 0xC4, 0x00, 0x03, 0x00]);
    let (streamed, staged) = exact_jpeg_both_ways(
        &bad,
        Encoder::Jpegli,
        crate::meta::Orientation::UPRIGHT,
        None,
    );
    for (side, e) in [
        ("streamed", streamed.err().expect("streamed must fail")),
        ("staged", staged.expect_err("staged must fail")),
    ] {
        assert!(
            format!("{e:#}").contains("libjpeg fatal error"),
            "{side}: {e:#}"
        );
    }
}

// ---- Streamed RGB8 PNG resize (issue #129) ----

/// Deterministic noisy RGB8 frame (gradients + LCG noise), as a PNG.
fn noisy_rgb_png(w: usize, h: usize) -> Vec<u8> {
    let mut seed = 0x2545F491u32;
    let mut px = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            for c in 0..3 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let g = (x * 255 / w + y * 97 / h + c * 40) % 256;
                px.push(((g + (seed >> 26) as usize) % 256) as u8);
            }
        }
    }
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&px).unwrap();
    out
}

/// PNG chunk CRC (ISO 3309), for hand-patched test streams.
fn png_crc(bytes: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in bytes {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// A well-formed PNG whose IHDR claims `h` rows while its (complete,
/// checksummed) image data holds only `h - missing`: the decoder runs
/// out of rows without a chunk-framing error.
fn short_rgb_png(w: usize, h: usize, missing: usize) -> Vec<u8> {
    let mut b = noisy_rgb_png(w, h - missing);
    // IHDR: length at 8, type at 12, data at 16 (height is its second
    // u32), CRC over type + 13 data bytes at 29.
    b[20..24].copy_from_slice(&(h as u32).to_be_bytes());
    let crc = png_crc(&b[12..29]);
    b[29..33].copy_from_slice(&crc.to_be_bytes());
    b
}

/// Run the hot-path helper on a PNG, from a fresh decoder positioned
/// the way `process_png` leaves it.
fn png_linear(
    png_bytes: &[u8],
    dst: (usize, usize),
    parallel: usize,
    stream: bool,
) -> Result<(Vec<u8>, bool)> {
    let mut d = png::Decoder::new(std::io::Cursor::new(png_bytes));
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = d.read_info()?;
    let src = (r.info().width as usize, r.info().height as usize);
    let (mut src16, mut dst16, mut out8, mut resizer) = (Vec::new(), Vec::new(), Vec::new(), None);
    let streamed = super::formats::png_rgb8_linear_resize(
        super::formats::LinearBufs {
            src16: &mut src16,
            dst16: &mut dst16,
            out8: &mut out8,
            resizer: &mut resizer,
        },
        &mut r,
        src,
        dst,
        parallel,
        stream,
    )?;
    out8.truncate(dst.0 * dst.1 * 3);
    Ok((out8, streamed))
}

/// Width, height.
type Dims = (usize, usize);

/// (src, dst) shapes for the streamed PNG resize: tiny sources, output
/// heights that do not divide the input, and extreme reductions down
/// to a single row or column (a 7000-row window into one output row).
const PNG_STREAM_SHAPES: &[(Dims, Dims)] = &[
    ((2, 2), (1, 1)),
    ((1, 3), (1, 2)),
    ((97, 61), (13, 8)),
    ((64, 48), (10, 7)),
    ((33, 33), (32, 31)),
    ((40, 7000), (3, 1)),
    ((3000, 2), (7, 1)),
    ((5, 7000), (1, 1)),
];

/// The streamed helper must produce exactly what the row kernel's
/// full-frame driver produces over the staged u16 frame — which is
/// what `resize_bands` runs for these requests on aarch64 — on every
/// architecture that has the kernel.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[test]
fn png_streamed_resize_matches_the_full_frame_kernel() {
    // The arch cfg does not promise the CPU features: an x86-64 host
    // without AVX2/FMA falls back to the staged path, correctly.
    if !fuse_kernel_available() {
        return;
    }
    for &((sw, sh), (dw, dh)) in PNG_STREAM_SHAPES {
        let png_bytes = noisy_rgb_png(sw, sh);
        let (got, streamed) = png_linear(&png_bytes, (dw, dh), 1, true).unwrap();
        assert!(streamed, "{sw}x{sh}->{dw}x{dh} did not stream");

        // Reference: stage the whole frame, resize it in one call.
        let mut d = png::Decoder::new(std::io::Cursor::new(&png_bytes[..]));
        d.set_transformations(png::Transformations::EXPAND);
        let mut r = d.read_info().unwrap();
        let mut px = vec![0u8; r.output_buffer_size().unwrap()];
        r.next_frame(&mut px).unwrap();
        let fwd = fwd_lut();
        let src16: Vec<u16> = px.iter().map(|&b| fwd[b as usize]).collect();
        let mut dst16 = vec![0u16; dw * dh * 3];
        crate::resize_kernel::resize_u16::<FuseKernel>(
            u16_as_bytes(&src16),
            sw,
            sh,
            u16_as_bytes_mut(&mut dst16),
            dw,
            dh,
            3,
        )
        .unwrap();
        let back = back_lut();
        let want: Vec<u8> = dst16.iter().map(|&v| back[v as usize]).collect();
        assert!(got == want, "{sw}x{sh}->{dw}x{dh}: streamed bytes differ");
    }
}

/// On aarch64 the streamed path replaces exactly the staged path it
/// skips: same request, same bytes (issue #129's identity contract).
#[cfg(target_arch = "aarch64")]
#[test]
fn png_streamed_resize_matches_the_staged_path() {
    for &((sw, sh), (dw, dh)) in PNG_STREAM_SHAPES {
        let png_bytes = noisy_rgb_png(sw, sh);
        let (staged, streamed) = png_linear(&png_bytes, (dw, dh), 1, false).unwrap();
        assert!(!streamed);
        let (got, _) = png_linear(&png_bytes, (dw, dh), 1, true).unwrap();
        assert!(got == staged, "{sw}x{sh}->{dw}x{dh}: streamed != staged");
    }
}

/// The stream gate is resize_bands' own kernel choice: only where it
/// would run the row kernel (aarch64, one band, no fir override).
#[test]
fn bands_stream_kernel_gate() {
    let streams = cfg!(target_arch = "aarch64") && !crate::config::config().fir_backend;
    assert_eq!(bands_use_stream_kernel(PixelType::U16x3, 1, 500), streams);
    // Too few rows to band: still one call.
    assert_eq!(bands_use_stream_kernel(PixelType::U16x3, 4, 7), streams);
    assert_eq!(bands_use_stream_kernel(PixelType::U16x4, 0, 10), streams);
    // Band-parallel requests stay staged (fir bands).
    assert!(!bands_use_stream_kernel(PixelType::U16x3, 4, 8));
    assert!(!bands_use_stream_kernel(PixelType::U16x3, 2, 500));
    // u8 pixels never reach the u16 kernel.
    assert!(!bands_use_stream_kernel(PixelType::U8x3, 1, 500));
}

/// Decode a PNG to its raw (expanded) samples.
fn decode_png_rgb(png_bytes: &[u8]) -> Vec<u8> {
    let mut d = png::Decoder::new(std::io::Cursor::new(png_bytes));
    d.set_transformations(png::Transformations::EXPAND);
    let mut r = d.read_info().unwrap();
    let mut px = vec![0u8; r.output_buffer_size().unwrap()];
    let n = r.next_frame(&mut px).unwrap().buffer_size();
    px.truncate(n);
    px
}

/// Band-parallel requests keep the staged path, and their output.
#[test]
fn png_band_parallel_stays_staged() {
    let png_bytes = noisy_rgb_png(97, 61);
    let params = Params {
        max_width: 40,
        max_height: 40,
        parallel: 2,
        ..Params::default()
    };
    let (dw, dh) = fit_dims(97, 61, 40, 40);
    let stream = bands_use_stream_kernel(PixelType::U16x3, params.parallel, dh);
    assert!(!stream, "two bands of {dh} rows must not stream");
    let (staged, streamed) = png_linear(&png_bytes, (dw, dh), params.parallel, stream).unwrap();
    assert!(!streamed);
    let (out, _) = process(&png_bytes, &params).unwrap();
    assert!(decode_png_rgb(&out) == staged);
}

/// A stream that ends early fails the request on both paths with the
/// same error — the streamed path has already emitted output rows by
/// then, but nothing is encoded. Covers a framing-valid stream short of
/// rows and a byte-truncated one.
#[test]
fn png_truncated_stream_fails_on_both_paths() {
    let short = short_rgb_png(9, 12, 3);
    let mut cut = noisy_rgb_png(64, 64);
    cut.truncate(cut.len() / 2);
    for (src, dst) in [(&short, (3, 4)), (&cut, (8, 8))] {
        let staged = png_linear(src, dst, 1, false).unwrap_err();
        let streamed = png_linear(src, dst, 1, true).unwrap_err();
        assert_eq!(format!("{staged:#}"), format!("{streamed:#}"));
        assert!(
            format!("{streamed:#}").starts_with("decode PNG"),
            "{streamed:#}"
        );
        // Through the pipeline: a client error, never an encoded image.
        let e = process(
            src,
            &Params {
                max_width: dst.0 as u32,
                max_height: dst.1 as u32,
                ..Params::default()
            },
        )
        .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Undecodable, "{e:#}");
    }
}

/// Tiny PNG sources through `process`: a 1x1 is never resized (the hot
/// path needs a size change), a 2x2 into a 1x1 box takes it.
#[test]
fn png_tiny_sources_resize() {
    for (w, h, bx) in [(1, 1, 1), (2, 2, 1), (1, 3, 2)] {
        let src = noisy_rgb_png(w, h);
        let (out, fmt) = process(
            &src,
            &Params {
                max_width: bx,
                max_height: bx,
                ..Params::default()
            },
        )
        .unwrap();
        assert_eq!(fmt, ImageFormat::Png);
        let (ew, eh) = fit_dims(w, h, bx, bx);
        assert_eq!(decode_png_rgb(&out).len(), ew * eh * 3, "{w}x{h} in {bx}");
    }
}

/// `ForwardOnly` answers position queries from its byte counter and
/// refuses every seek that would move the stream (issue #129).
#[test]
fn forward_only_answers_position_and_refuses_moving_seeks() {
    use super::formats::ForwardOnly;
    use std::io::{BufRead, Read, Seek, SeekFrom};
    let data: Vec<u8> = (0u8..64).collect();
    let mut r = ForwardOnly::new(std::io::BufReader::with_capacity(16, &data[..]));
    assert_eq!(r.stream_position().unwrap(), 0);

    // Both read paths advance the counter.
    let mut four = [0u8; 4];
    r.read_exact(&mut four).unwrap();
    assert_eq!(four, [0, 1, 2, 3]);
    let avail = r.fill_buf().unwrap().len();
    assert!(avail >= 3, "the BufReader still holds bytes");
    r.consume(3);
    assert_eq!(r.stream_position().unwrap(), 7);
    let mut next = [0u8; 1];
    r.read_exact(&mut next).unwrap();
    assert_eq!(next, [7], "consume moved the inner reader too");

    // Seeks that stay put are answered; everything else is refused.
    // The literal Current(0) is the point: it is the call a decoder
    // makes when it does not use `stream_position`.
    #[allow(clippy::seek_from_current)]
    let here = r.seek(SeekFrom::Current(0)).unwrap();
    assert_eq!(here, 8);
    assert_eq!(r.seek(SeekFrom::Start(8)).unwrap(), 8);
    for to in [
        SeekFrom::Start(0),
        SeekFrom::Start(9),
        SeekFrom::Current(-1),
        SeekFrom::Current(1),
        SeekFrom::End(0),
    ] {
        let e = r.seek(to).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::Unsupported, "{to:?}");
        assert!(e.to_string().contains("forward-only"), "{e}");
    }
    assert_eq!(
        r.stream_position().unwrap(),
        8,
        "a refused seek moves nothing"
    );
}

/// Every PNG shape the pipeline decodes, including the ones carrying
/// the ancillary chunks it reads (`eXIf`, `iCCP`).
fn png_shapes() -> Vec<(String, Vec<u8>)> {
    let mut shapes: Vec<(String, Vec<u8>)> = [
        "rgb.png",
        "rgba.png",
        "gray.png",
        "gray16.png",
        "graya.png",
        "palette.png",
        "interlaced.png",
        "rgb16.png",
    ]
    .iter()
    .map(|n| (n.to_string(), read_fixture(n)))
    .collect();
    let (w, h) = (40u32, 30u32);
    let px: Vec<u8> = (0..w * h * 3).map(|i| (i * 7 % 251) as u8).collect();
    let mut info = png::Info::with_size(w, h);
    // Raw little-endian TIFF, one IFD entry: orientation 6.
    info.exif_metadata = Some(std::borrow::Cow::Owned(vec![
        b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0,
    ]));
    info.icc_profile = Some(std::borrow::Cow::Owned(vec![0x5a; 600]));
    let mut out = Vec::new();
    let mut enc = png::Encoder::with_info(&mut out, info).unwrap();
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    writer.write_image_data(&px).unwrap();
    writer.finish().unwrap();
    shapes.push(("exif_iccp (generated)".into(), out));
    shapes
}

/// Everything the pipeline reads out of a PNG: header facts, the
/// ancillary chunks, the frame (once whole, once row by row, the two
/// decode arms of `process_png`), then the trailing chunks to IEND.
fn decode_png_fully<R: std::io::BufRead + std::io::Seek>(
    open: impl Fn() -> R,
) -> Result<(String, Vec<u8>, Vec<u8>), png::DecodingError> {
    let decoder = || {
        let mut d = png::Decoder::new(open());
        d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        d
    };
    let mut r = decoder().read_info()?;
    let info = r.info();
    let meta = format!(
        "{}x{} {:?} interlaced={} exif={:?} icc={:?}",
        info.width,
        info.height,
        info.color_type,
        info.interlaced,
        info.exif_metadata,
        info.icc_profile.as_ref().map(|c| c.len()),
    );
    let mut frame = vec![0u8; r.output_buffer_size().unwrap()];
    r.next_frame(&mut frame)?;
    r.finish()?;
    let mut r = decoder().read_info()?;
    let mut rows = Vec::new();
    while let Some(row) = r.next_row()? {
        rows.extend_from_slice(row.data());
    }
    r.finish()?;
    Ok((meta, frame, rows))
}

/// Pins the assumption `ForwardOnly` rests on (issue #129): png 0.18
/// bounds its decoder on `Seek` but never seeks. Should a png upgrade
/// start seeking, the refused seek surfaces here as a decode error —
/// at every buffer granularity, so a seek that only fires on a short
/// read cannot hide.
#[test]
fn png_never_seeks_through_forward_only() {
    use super::formats::ForwardOnly;
    for (name, bytes) in png_shapes() {
        let want = decode_png_fully(|| std::io::Cursor::new(&bytes[..]))
            .unwrap_or_else(|e| panic!("{name}: Cursor decode: {e}"));
        for cap in [1, 7, 8192] {
            let got = decode_png_fully(|| {
                ForwardOnly::new(std::io::BufReader::with_capacity(cap, &bytes[..]))
            })
            .unwrap_or_else(|e| panic!("{name} at {cap}-byte reads: png tried to seek? {e}"));
            assert!(
                got == want,
                "{name} at {cap}-byte reads decoded differently"
            );
        }
    }
}

/// Streaming the PNG must not change how a truncated or corrupt source
/// fails: the in-memory and file entry points agree, cut for cut, and
/// every failure is still a client-side `Undecodable`. Covers both
/// decode arms: the row-streamed RGB8 resize (rgb.png into a smaller
/// box) and the whole-frame arm (everything else).
#[test]
fn truncated_png_fails_alike_in_memory_and_from_a_file() {
    let dir = std::env::temp_dir().join(format!("oximg-pngcut-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = Params {
        max_width: 64,
        max_height: 64,
        ..Params::default()
    };
    let mut failures = 0;
    for (name, bytes) in png_shapes() {
        let n = bytes.len();
        // Just past IHDR's length (a cut inside the 8-byte signature
        // never reaches the decoder: the 12-byte sniff fails first),
        // inside IHDR's CRC, mid-IDAT, and the IEND chunk dropped; plus
        // one flipped byte mid-stream.
        let mut cuts: Vec<Vec<u8>> = [12, 30, n / 2, n - 12]
            .iter()
            .map(|&c| bytes[..c].to_vec())
            .collect();
        let mut flipped = bytes.clone();
        flipped[n / 2] ^= 0xff;
        cuts.push(flipped);
        for (i, src) in cuts.iter().enumerate() {
            let path = dir.join(format!("cut{i}.png"));
            std::fs::write(&path, src).unwrap();
            let mem = process(src, &p);
            let file = process_path(&path, &p);
            match (mem, file) {
                (Ok(a), Ok(b)) => assert!(a.0 == b.0, "{name} cut {i}: bytes differ"),
                (Err(a), Err(b)) => {
                    failures += 1;
                    assert_eq!(a.kind(), ErrorKind::Undecodable, "{name} cut {i}: {a:#}");
                    assert_eq!(b.kind(), ErrorKind::Undecodable, "{name} cut {i}: {b:#}");
                    assert_eq!(format!("{a:#}"), format!("{b:#}"), "{name} cut {i}");
                }
                (a, b) => panic!(
                    "{name} cut {i}: in-memory {:?} vs file {:?}",
                    a.map(|x| x.1),
                    b.map(|x| x.1)
                ),
            }
        }
    }
    assert!(failures > 0, "the cuts must exercise the error path");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Deterministic noise frame: no two neighbors alike, so a dropped,
/// shifted or stale pixel cannot pass a byte-for-byte comparison.
fn noise_px(w: usize, h: usize, channels: usize, seed: u32) -> Vec<u8> {
    let mut x = seed;
    (0..w * h * channels)
        .map(|_| {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            (x >> 24) as u8
        })
        .collect()
}

/// Lossless PNG of `px`, optionally carrying an eXIf orientation.
fn png_of(px: &[u8], w: usize, h: usize, channels: usize, orientation: Option<u16>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut info = png::Info::with_size(w as u32, h as u32);
    if let Some(o) = orientation {
        // Big-endian TIFF with a single Orientation (0x0112) entry.
        let mut tiff = b"MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01".to_vec();
        tiff.extend_from_slice(&o.to_be_bytes());
        tiff.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        info.exif_metadata = Some(std::borrow::Cow::Owned(tiff));
    }
    let mut enc = png::Encoder::with_info(&mut out, info).unwrap();
    enc.set_color(if channels == 4 {
        png::ColorType::Rgba
    } else {
        png::ColorType::Rgb
    });
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().unwrap();
    wr.write_image_data(px).unwrap();
    wr.finish().unwrap();
    out
}

/// (width, height, 8-bit samples) of a PNG the pipeline wrote.
fn png_pixels(bytes: &[u8]) -> (usize, usize, Vec<u8>) {
    let mut r = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .unwrap();
    let mut buf = vec![0u8; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    (info.width as usize, info.height as usize, buf)
}

/// PNG source through the full-frame path into a lossless PNG target.
fn png_to_png(s: &mut Scratch, src: &[u8], max: u32) -> (usize, usize, Vec<u8>) {
    let out = formats::process_png(s, src, ImageFormat::Png, &rbox(max, max)).unwrap();
    png_pixels(&out)
}

/// At exact size the decoded frame is handed to `out8` rather than
/// copied (issue #129): the pixels must come through untouched — alpha
/// included, colors under transparent pixels included, since nothing
/// premultiplies without a resize — and only one frame-sized buffer
/// may stay resident in scratch.
#[test]
fn exact_size_png_passes_pixels_through_without_a_second_frame() {
    let (w, h) = (97, 61);
    for channels in [3, 4] {
        let px = noise_px(w, h, channels, 7 + channels as u32);
        let mut s = Scratch::default();
        let (ow, oh, got) = png_to_png(&mut s, &png_of(&px, w, h, channels, None), 4096);
        assert_eq!((ow, oh), (w, h), "ch={channels}");
        assert!(got == px, "ch={channels}: exact size must be lossless");
        let frame = w * h * channels;
        assert!(s.out8.len() >= frame, "ch={channels}: out8 holds the frame");
        assert!(
            s.chunk8.len() < frame,
            "ch={channels}: chunk8 must not keep a second copy ({} bytes)",
            s.chunk8.len()
        );
    }
}

/// Exact size with an eXIf rotation: the handed-over frame rotates
/// into the short buffer swapped back into `chunk8`, which must grow
/// to fit. Orientation 6 displays stored (x, y) at (sh-1-y, x).
#[test]
fn exact_size_oriented_png_rotates_the_handed_over_frame() {
    let (sw, sh) = (53, 31);
    for channels in [3, 4] {
        let px = noise_px(sw, sh, channels, 11 + channels as u32);
        let mut s = Scratch::default();
        let (ow, oh, got) = png_to_png(&mut s, &png_of(&px, sw, sh, channels, Some(6)), 4096);
        assert_eq!((ow, oh), (sh, sw), "ch={channels}: axes swap");
        let mut want = vec![0u8; px.len()];
        for y in 0..sh {
            for x in 0..sw {
                let d = (x * ow + (sh - 1 - y)) * channels;
                let o = (y * sw + x) * channels;
                want[d..d + channels].copy_from_slice(&px[o..o + channels]);
            }
        }
        assert!(got == want, "ch={channels}: rotated pixels");
    }
}

/// Consecutive exact-size requests on one worker must not settle at
/// two frames: scratch is grow-only and kept per thread, so without
/// the request-start reclaim the second decode allocates a new frame
/// into `chunk8` while `out8` still holds the first one.
#[test]
fn consecutive_exact_size_requests_keep_one_frame() {
    let (w, h) = (300, 200);
    let frame = w * h * 3;
    let png = png_of(&noise_px(w, h, 3, 11), w, h, 3, None);
    let exact = Params {
        output: Some(ImageFormat::Png),
        ..sized(4096, 4096)
    };
    for i in 0..3 {
        process(&png, &exact).unwrap();
        SCRATCH.with(|s| {
            let s = s.borrow();
            let (c, o) = (s.chunk8.capacity(), s.out8.capacity());
            assert!(
                c.min(o) < frame,
                "request {i}: chunk8 {c} and out8 {o} both hold a {frame}-byte frame"
            );
        });
    }
}

/// The same for `decode_and_resize`, the other entry point that borrows
/// `SCRATCH`: a CMYK JPEG stages the whole frame and hands it over at
/// exact size.
#[test]
fn consecutive_exact_size_decode_and_resize_keeps_one_frame() {
    let jpeg = read_fixture("cmyk_icc.jpg");
    for i in 0..3 {
        let (_, w, h) = decode_and_resize(&jpeg, 4096, 4096, 1).unwrap();
        SCRATCH.with(|s| {
            let s = s.borrow();
            let (c, o) = (s.chunk8.capacity(), s.out8.capacity());
            assert!(
                c.min(o) < w * h * 3,
                "request {i}: chunk8 {c} and out8 {o} both hold a {w}x{h} frame"
            );
        });
    }
}

/// Scratch survives across requests on a worker thread, and after an
/// exact-size request `chunk8` is whatever `out8` held before — often
/// far shorter than the frame. Every later request must size it again
/// rather than trust its length: interleave exact, resized and
/// oriented requests on one scratch and compare each against a fresh
/// one.
#[test]
fn exact_size_handoff_leaves_scratch_reusable() {
    let rgb = png_of(&noise_px(120, 80, 3, 1), 120, 80, 3, None);
    let rgba = png_of(&noise_px(64, 90, 4, 2), 64, 90, 4, None);
    let oriented = png_of(&noise_px(70, 40, 3, 3), 70, 40, 3, Some(8));
    let requests: [(&[u8], u32); 6] = [
        (&rgb, 4096),
        (&rgba, 32),
        (&oriented, 4096),
        (&rgba, 4096),
        (&rgb, 50),
        (&rgb, 4096),
    ];
    let mut shared = Scratch::default();
    for (i, (src, max)) in requests.iter().enumerate() {
        let got = png_to_png(&mut shared, src, *max);
        let want = png_to_png(&mut Scratch::default(), src, *max);
        assert!(
            got == want,
            "request {i} (max {max}) differs on reused scratch"
        );
    }
}

/// Decode a PNG to its 8-bit pixels and list its IDAT chunk lengths.
fn png_pixels_and_idats(b: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let mut r = png::Decoder::new(std::io::Cursor::new(b))
        .read_info()
        .unwrap();
    let mut px = vec![0; r.output_buffer_size().unwrap()];
    let n = r.next_frame(&mut px).unwrap().buffer_size();
    px.truncate(n);
    let (mut i, mut idats) = (8, Vec::new());
    while i + 8 <= b.len() {
        let len = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize;
        if &b[i + 4..i + 8] == b"IDAT" {
            idats.push(len);
        }
        i += 12 + len;
    }
    (px, idats)
}

/// The PNG encoder streams deflate output into 256 KiB IDAT chunks
/// (issue #133) instead of one IDAT built in a second buffer: the same
/// pixels, and the same deflate stream `write_image_data` produced, so
/// the file differs only by the extra chunk framing.
#[test]
fn png_streams_its_idat_chunks() {
    let def = Resolved::new(&Params::default());
    let (w, h) = (900, 700);
    for channels in [3, 4] {
        // A gradient under four bits of noise: compressible, but to
        // more than one chunk.
        let mut seed = 0x9E3779B9u32;
        let px: Vec<u8> = (0..w * h * channels)
            .map(|i| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                ((i % (w * channels)) * 255 / (w * channels)) as u8 ^ (seed >> 28) as u8
            })
            .collect();
        let out = encode_png(&px, w, h, channels, None, &def).unwrap();
        let (decoded, idats) = png_pixels_and_idats(&out);
        assert!(decoded == px, "{channels} channels: pixels changed");
        assert!(idats.len() > 1, "{channels} channels: {idats:?}");
        let (last, full) = idats.split_last().unwrap();
        assert!(full.iter().all(|&n| n == 256 * 1024), "{idats:?}");
        assert!(*last <= 256 * 1024);

        let mut whole = Vec::new();
        let mut enc = png::Encoder::new(&mut whole, w as u32, h as u32);
        enc.set_color(if channels == 4 {
            png::ColorType::Rgba
        } else {
            png::ColorType::Rgb
        });
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(&px).unwrap();
        writer.finish().unwrap();
        let framing = 12 * (idats.len() - 1);
        assert_eq!(out.len(), whole.len() + framing, "{channels} channels");
    }
}

/// fdeflate expands noise; like `write_image_data`, the encoder then
/// falls back to stored blocks rather than ship the expansion, and
/// zlib's levels hold to the stored size by themselves.
#[test]
fn incompressible_png_falls_back_to_stored_blocks() {
    let (w, h): (usize, usize) = (600, 400);
    let mut seed = 0x2545F491u32;
    let px: Vec<u8> = (0..w * h * 3)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed >> 24) as u8
        })
        .collect();
    let raw = (w * 3 + 1) * h;
    let stored = 2 + raw + 5 * raw.div_ceil(65535) + 4;
    for effort in [PngEffort::Fastest, PngEffort::Fast, PngEffort::Balanced] {
        let p = Resolved::new(&Params {
            png_effort: Some(effort),
            ..Params::default()
        });
        let out = encode_png(&px, w, h, 3, None, &p).unwrap();
        let (decoded, idats) = png_pixels_and_idats(&out);
        assert!(decoded == px, "{effort:?}: pixels changed");
        let idat: usize = idats.iter().sum();
        // flate2's stored blocks run ~0.01% over the 65535-byte ideal.
        assert!(
            idat <= stored + stored / 1000,
            "{effort:?}: {idat} > stored {stored}"
        );
    }
}

/// A capped attempt reports itself wherever the cap falls, including in
/// the last bytes fdeflate writes from its `finish`, which unwraps them:
/// the writer must drop past the cap, not fail.
#[test]
fn a_capped_png_attempt_never_panics() {
    // The 256 KiB chunk buffer flushes into the capped writer only when
    // it fills, so the crossing lands in `finish` when the deflate stream
    // ends 1-12 bytes past a chunk boundary. Search noise sizes for one.
    let fast = png::Compression::Fast;
    let rgb = png::ColorType::Rgb;
    let mut seed = 0x2545F491u32;
    let noise: Vec<u8> = (0..400 * 400 * 3)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed >> 24) as u8
        })
        .collect();
    let (w, h, full) = (200usize..400)
        .flat_map(|w| (1..=400).map(move |h| (w, h)))
        .filter(|&(w, h)| {
            // fdeflate expands noise: ~1.36 B per filtered raw byte.
            let est = (w * 3 + 1) * h * 136 / 100;
            est.abs_diff(256 * 1024) < 4096
        })
        .find_map(|(w, h)| {
            let mut full = Vec::new();
            let px = &noise[..w * h * 3];
            write_png(&mut full, usize::MAX, w, h, None, rgb, None, px, fast).unwrap();
            let (_, idats) = png_pixels_and_idats(&full);
            (idats.len() == 2 && (1..=12).contains(&idats[1])).then_some((w, h, full))
        })
        .expect("a size whose stream ends just past a chunk");
    let px = &noise[..w * h * 3];
    for cap in full.len() - 40..full.len() {
        let mut out = Vec::new();
        let fits = write_png(&mut out, cap, w, h, None, rgb, None, px, fast).unwrap();
        assert!(!fits, "cap {cap} of {}", full.len());
        assert!(out.len() <= cap);
    }
    let mut out = Vec::new();
    assert!(write_png(&mut out, full.len(), w, h, None, rgb, None, px, fast).unwrap());
    assert!(out == full);
}
