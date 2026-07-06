use super::*;

/// Embed → extract must be the identity, the patched container must
/// decode to the same pixels, and the growth must be exactly the
/// colr box plus one association byte.
#[test]
fn icc_embed_extract_roundtrip() {
    let rgb = pixel_samples(64 * 48 * 3, 7);
    let plain = encode_avif(&rgb, 64, 48, 3, &AvifParams::default(), None).unwrap();
    assert_eq!(extract_icc(&plain), None, "no profile without embedding");

    let icc: Vec<u8> = (0..900u32).map(|i| (i % 251) as u8).collect();
    let profiled = encode_avif(&rgb, 64, 48, 3, &AvifParams::default(), Some(&icc)).unwrap();
    assert_eq!(extract_icc(&profiled).as_deref(), Some(&icc[..]));
    assert_eq!(
        profiled.len(),
        plain.len() + 12 + icc.len() + 1,
        "growth = colr box + one association"
    );
    let (a, aw, ah, _) = decode_avif(&plain).unwrap();
    let (b, bw, bh, _) = decode_avif(&profiled).unwrap();
    assert_eq!((aw, ah), (bw, bh));
    assert_eq!(a, b, "profile splice must not disturb the image data");
    assert_eq!(probe_avif(&profiled).unwrap(), (64, 48));
}

/// Alpha adds a second item whose iloc entry must shift correctly
/// too.
#[test]
fn icc_embed_survives_alpha_items() {
    let mut rgba = pixel_samples(48 * 32 * 4, 11);
    rgba[3] = 128; // ensure a transparent pixel keeps the alpha item
    let icc: Vec<u8> = (0..300u32).map(|i| (i * 7 % 251) as u8).collect();
    let profiled = encode_avif(&rgba, 48, 32, 4, &AvifParams::default(), Some(&icc)).unwrap();
    assert_eq!(extract_icc(&profiled).as_deref(), Some(&icc[..]));
    let (_, w, h, ch) = decode_avif(&profiled).unwrap();
    assert_eq!((w, h, ch), (48, 32, 4), "alpha item survives the splice");
}

/// Simple compact box for hand-built container tests.
fn fbox(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(typ);
    v.extend_from_slice(payload);
    v
}

/// The extractor must read layouts our own serializer never
/// writes: version-1 `pitm`/`ipma` (u32 item ids), wide (15-bit)
/// associations, and essential-flagged narrow associations.
#[test]
fn icc_extract_reads_wide_and_versioned_layouts() {
    let icc = [0xAB_u8; 96];
    let mut colr = b"prof".to_vec();
    colr.extend_from_slice(&icc);
    // A filler property first, so colr is property index 2.
    let ipco = fbox(
        b"ipco",
        &[fbox(b"free", &[0u8; 4]), fbox(b"colr", &colr)].concat(),
    );

    // Wide + version 1: item id 7 as u32, association as u16.
    let mut ipma_pl = vec![1, 0, 0, 1];
    ipma_pl.extend(1u32.to_be_bytes()); // entry_count
    ipma_pl.extend(7u32.to_be_bytes()); // item_id
    ipma_pl.push(1); // association count
    ipma_pl.extend(2u16.to_be_bytes()); // wide index 2
    let iprp = fbox(b"iprp", &[ipco.clone(), fbox(b"ipma", &ipma_pl)].concat());
    let mut pitm_pl = vec![1, 0, 0, 0];
    pitm_pl.extend(7u32.to_be_bytes());
    let meta_pl = [vec![0, 0, 0, 0], fbox(b"pitm", &pitm_pl), iprp].concat();
    let avif = fbox(b"meta", &meta_pl);
    assert_eq!(extract_icc(&avif).as_deref(), Some(&icc[..]), "wide/v1");

    // Narrow + version 0 with the essential bit set on the index.
    let mut ipma_pl = vec![0, 0, 0, 0];
    ipma_pl.extend(1u32.to_be_bytes());
    ipma_pl.extend(7u16.to_be_bytes());
    ipma_pl.push(1);
    ipma_pl.push(0x80 | 2); // essential + index 2
    let iprp = fbox(b"iprp", &[ipco, fbox(b"ipma", &ipma_pl)].concat());
    let mut pitm_pl = vec![0, 0, 0, 0];
    pitm_pl.extend(7u16.to_be_bytes());
    let meta_pl = [vec![0, 0, 0, 0], fbox(b"pitm", &pitm_pl), iprp].concat();
    let avif = fbox(b"meta", &meta_pl);
    assert_eq!(
        extract_icc(&avif).as_deref(),
        Some(&icc[..]),
        "narrow/essential"
    );
}

/// HEIF applies transformative properties in association order.
/// MIAF writers put irot before imir (the rotation-first table);
/// a spec-legal mirror-first file must reduce via the dihedral
/// identity instead — pinned against libheif 1.23's rendering of
/// exactly this byte layout (association bytes swapped on the
/// irot1+imir1 fixture: libheif shows mirror-then-rotation).
#[test]
fn orientation_honors_association_order() {
    let build = |first: (&[u8; 4], u8), second: (&[u8; 4], u8)| -> Vec<u8> {
        let ipco = fbox(
            b"ipco",
            &[fbox(first.0, &[first.1]), fbox(second.0, &[second.1])].concat(),
        );
        let mut ipma_pl = vec![0, 0, 0, 0];
        ipma_pl.extend(1u32.to_be_bytes());
        ipma_pl.extend(7u16.to_be_bytes());
        ipma_pl.push(2); // two associations, in ipco order
        ipma_pl.push(0x80 | 1);
        ipma_pl.push(0x80 | 2);
        let iprp = fbox(b"iprp", &[ipco, fbox(b"ipma", &ipma_pl)].concat());
        let mut pitm_pl = vec![0, 0, 0, 0];
        pitm_pl.extend(7u16.to_be_bytes());
        let meta_pl = [vec![0, 0, 0, 0], fbox(b"pitm", &pitm_pl), iprp].concat();
        fbox(b"meta", &meta_pl)
    };
    // MIAF order: rotation then mirror → EXIF 7 (transverse).
    let rot_first = build((b"irot", 1), (b"imir", 1));
    assert_eq!(
        extract_orientation(&rot_first),
        crate::meta::Orientation::from_rot_mirror(1, Some(1))
    );
    // Mirror first: libheif renders mirror-then-rotation, which the
    // identity rot_a ∘ mirror = mirror ∘ rot_{-a} maps to the
    // rotation-first table at the negated angle → EXIF 5.
    let mirror_first = build((b"imir", 1), (b"irot", 1));
    assert_eq!(
        extract_orientation(&mirror_first),
        crate::meta::Orientation::from_rot_mirror(3, Some(1))
    );
    assert_ne!(
        extract_orientation(&rot_first),
        extract_orientation(&mirror_first)
    );
}

/// The extractor never errors on hostile input, and the embedder
/// declines rather than corrupts.
#[test]
fn icc_walkers_are_fail_safe() {
    let rgb = pixel_samples(32 * 24 * 3, 3);
    let plain = encode_avif(&rgb, 32, 24, 3, &AvifParams::default(), None).unwrap();
    for cut in [0, 8, 40, plain.len() / 2] {
        assert_eq!(extract_icc(&plain[..cut]), None);
    }
    assert_eq!(extract_icc(b"not an avif at all"), None);
    assert!(embed_icc(b"garbage", &[1, 2, 3]).is_none());
    assert!(embed_icc(&plain[..40], &[1, 2, 3]).is_none());
}

/// Deterministic pseudo-random pixels covering 0 and 255 exactly.
fn pixel_samples(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|i| match i % 17 {
            0 => 0,
            1 => 255,
            _ => {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 24) as u8
            }
        })
        .collect()
}

/// Manual micro-benchmark: kernel-vs-scalar conversion cost.
/// cargo test --release --features avif bench_yuv -- --ignored --nocapture
#[cfg(target_arch = "x86_64")]
#[test]
#[ignore = "manual micro-benchmark"]
fn bench_yuv_kernels() {
    let (w, h) = (512usize, 340usize);
    let px = pixel_samples(w * h * 3, 42);
    let mut y = vec![0u16; w * h];
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (mut cb, mut cr) = (vec![0u16; cw * ch], vec![0u16; cw * ch]);
    let iters = 3000u32;
    let ms = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1e3 / iters as f64;

    let t = std::time::Instant::now();
    for _ in 0..iters {
        luma_rows_scalar(&px, 3, &mut y);
        std::hint::black_box(&y);
    }
    eprintln!("luma scalar(auto-vec):  {:.4} ms/frame", ms(t));

    if avx2_enc::detect() {
        let t = std::time::Instant::now();
        for _ in 0..iters {
            unsafe { avx2_enc::luma_rows(&px, 3, &mut y) };
            std::hint::black_box(&y);
        }
        eprintln!("luma avx2 intrinsics:   {:.4} ms/frame", ms(t));
    }

    let rb = w * 3;
    let t = std::time::Instant::now();
    for _ in 0..iters {
        for cy in 0..ch {
            let row0 = &px[cy * 2 * rb..][..rb];
            let row1 = (cy * 2 + 1 < h).then(|| &px[(cy * 2 + 1) * rb..][..rb]);
            for cx in 0..cw {
                let (b, r) = chroma_block_rows(row0, row1, 3, cx, w);
                cb[cy * cw + cx] = b;
                cr[cy * cw + cx] = r;
            }
        }
        std::hint::black_box((&cb, &cr));
    }
    eprintln!("chroma scalar blocks:   {:.4} ms/frame", ms(t));

    if avx2_enc::detect() {
        let t = std::time::Instant::now();
        for _ in 0..iters {
            for cy in 0..ch {
                let row0 = &px[cy * 2 * rb..][..rb];
                if cy * 2 + 1 < h {
                    let row1 = &px[(cy * 2 + 1) * rb..][..rb];
                    unsafe {
                        avx2_enc::chroma_row_pair(
                            row0,
                            row1,
                            w,
                            3,
                            &mut cb[cy * cw..][..cw],
                            &mut cr[cy * cw..][..cw],
                        )
                    };
                }
            }
            std::hint::black_box((&cb, &cr));
        }
        eprintln!("chroma avx2 intrinsics: {:.4} ms/frame", ms(t));
    }
}

/// Frame-level scalar chroma reference, built from the row-pair
/// block the vector paths must match.
fn chroma_frame_scalar(px: &[u8], w: usize, h: usize, channels: usize) -> (Vec<u16>, Vec<u16>) {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (mut cb, mut cr) = (vec![0u16; cw * ch], vec![0u16; cw * ch]);
    let rb = w * channels;
    for cy in 0..ch {
        let row0 = &px[cy * 2 * rb..][..rb];
        let row1 = (cy * 2 + 1 < h).then(|| &px[(cy * 2 + 1) * rb..][..rb]);
        for cx in 0..cw {
            let (b, r) = chroma_block_rows(row0, row1, channels, cx, w);
            cb[cy * cw + cx] = b;
            cr[cy * cw + cx] = r;
        }
    }
    (cb, cr)
}

/// The fused AVIF path converts row by row instead of over the full
/// frame; the vector main loops restart (and re-tail) per row, so
/// pin that chunking never changes values.
#[test]
fn row_wise_luma_matches_full_frame() {
    let (w, h, channels) = (61usize, 9, 3);
    let px = pixel_samples(w * h * channels, 11);
    let mut full = vec![0u16; w * h];
    luma_rows(&px, channels, &mut full);
    let mut rows = vec![0u16; w * h];
    for y in 0..h {
        luma_rows(
            &px[y * w * channels..][..w * channels],
            channels,
            &mut rows[y * w..][..w],
        );
    }
    assert_eq!(full, rows);
}

/// The NEON luma path replaces `acc / 1044480` with
/// `((acc >> 12) * 8421505) >> 31`; prove both identities it stacks
/// (factor split and magic-multiply /255) over the full domain the
/// pipeline can produce.
#[test]
fn luma_divider_identity_is_exact() {
    for x in 0..=1_044_480u32 {
        let acc = x * 1023 + 522_240;
        let reference = acc / 1_044_480;
        let vectorized = (((acc >> 12) as u64 * 8_421_505) >> 31) as u32;
        assert_eq!(reference, vectorized, "x={x}");
    }
}

#[cfg(target_arch = "x86_64")]
#[test]
fn avx2_luma_rows_match_scalar_bit_exactly() {
    if !avx2_enc::detect() {
        return;
    }
    // Odd lengths exercise the scalar tail after the 16-wide loop.
    for (n, channels, seed) in [(1024, 3, 1), (1013, 3, 2), (1024, 4, 3), (777, 4, 4)] {
        let px = pixel_samples(n * channels, seed);
        let mut scalar = vec![0u16; n];
        let mut vector = vec![0u16; n];
        luma_rows_scalar(&px, channels, &mut scalar);
        unsafe { avx2_enc::luma_rows(&px, channels, &mut vector) };
        assert_eq!(scalar, vector, "n={n} channels={channels}");
    }
}

#[cfg(target_arch = "x86_64")]
#[test]
fn avx2_chroma_rows_match_scalar_bit_exactly() {
    if !avx2_enc::detect() {
        return;
    }
    for (w, h, channels, seed) in [
        (128, 64, 3, 5),
        (127, 63, 3, 6),
        (17, 5, 3, 7),
        (130, 62, 4, 8),
        (33, 7, 4, 9),
    ] {
        let px = pixel_samples(w * h * channels, seed);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (cb_s, cr_s) = chroma_frame_scalar(&px, w, h, channels);
        let (mut cb_v, mut cr_v) = (vec![0u16; cw * ch], vec![0u16; cw * ch]);
        chroma_rows(&px, w, h, channels, &mut cb_v, &mut cr_v);
        assert_eq!(cb_s, cb_v, "cb {w}x{h} channels={channels}");
        assert_eq!(cr_s, cr_v, "cr {w}x{h} channels={channels}");
    }
}

/// The AVX2 chroma path rounds with floor(x + 0.5) instead of the
/// scalar ties-away `round()` (valid for x >= 0). Sweep uniform 2x2
/// blocks across the color cube to hunt rounding-boundary
/// disagreements the randomized tests might miss.
#[cfg(target_arch = "x86_64")]
#[test]
fn avx2_chroma_rounding_matches_on_uniform_sweep() {
    if !avx2_enc::detect() {
        return;
    }
    // 16 blocks per 32x2 image; r dense, b stepped, g coarse.
    let (w, h) = (32usize, 2usize);
    for g in [0u8, 37, 128, 219, 255] {
        for b0 in (0..256usize).step_by(3) {
            let mut px = vec![0u8; w * h * 3];
            for blk in 0..16 {
                let r = ((b0 + blk * 16) % 256) as u8;
                let b = ((b0 + blk) % 256) as u8;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let p = (dy * w + blk * 2 + dx) * 3;
                        px[p] = r;
                        px[p + 1] = g;
                        px[p + 2] = b;
                    }
                }
            }
            let cw = w / 2;
            let (cb_s, cr_s) = chroma_frame_scalar(&px, w, h, 3);
            let (mut cb_v, mut cr_v) = (vec![0u16; cw], vec![0u16; cw]);
            chroma_rows(&px, w, h, 3, &mut cb_v, &mut cr_v);
            assert_eq!(cb_s, cb_v, "cb g={g} b0={b0}");
            assert_eq!(cr_s, cr_v, "cr g={g} b0={b0}");
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[test]
fn neon_luma_rows_match_scalar_bit_exactly() {
    if !crate::yuv::neon() {
        return;
    }
    // Odd lengths exercise the scalar tail after the 8-wide loop.
    for (n, channels, seed) in [(1024, 3, 1), (1021, 3, 2), (1024, 4, 3), (777, 4, 4)] {
        let px = pixel_samples(n * channels, seed);
        let mut scalar = vec![0u16; n];
        let mut neon = vec![0u16; n];
        luma_rows_scalar(&px, channels, &mut scalar);
        unsafe { neon_enc::luma_rows(&px, channels, &mut neon) };
        assert_eq!(scalar, neon, "n={n} channels={channels}");
    }
}

#[cfg(target_arch = "aarch64")]
#[test]
fn neon_chroma_rows_match_scalar_bit_exactly() {
    if !crate::yuv::neon() {
        return;
    }
    // Odd dims exercise the right-edge columns and bottom row that
    // fall back to the scalar block.
    for (w, h, channels, seed) in [
        (128, 64, 3, 5),
        (127, 63, 3, 6),
        (9, 5, 3, 7),
        (130, 62, 4, 8),
        (33, 7, 4, 9),
    ] {
        let px = pixel_samples(w * h * channels, seed);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (cb_s, cr_s) = chroma_frame_scalar(&px, w, h, channels);
        let (mut cb_n, mut cr_n) = (vec![0u16; cw * ch], vec![0u16; cw * ch]);
        chroma_rows(&px, w, h, channels, &mut cb_n, &mut cr_n);
        assert_eq!(cb_s, cb_n, "cb {w}x{h} channels={channels}");
        assert_eq!(cr_s, cr_n, "cr {w}x{h} channels={channels}");
    }
}

#[test]
fn encodes_a_decodable_avif() {
    let (w, h) = (128, 96);
    let rgb: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            let x = (i % w) as u8;
            let y = (i / w) as u8;
            [x.wrapping_mul(2), y.wrapping_mul(2), x ^ y]
        })
        .collect();
    let out = encode_avif(&rgb, w, h, 3, &AvifParams::default(), None).unwrap();
    assert!(out.len() > 100, "suspiciously small: {}", out.len());
    // container sanity: ftyp avif brand near the start
    assert_eq!(&out[4..12], b"ftypavif", "not an avif container");
}

#[test]
fn encode_decode_roundtrip_preserves_the_image() {
    let (w, h) = (160, 120);
    // Smooth gradient: compresses well, so quality loss stays small
    // and any plane/matrix/range mix-up shows up as a huge error.
    let rgb: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            let x = (i % w) as f32 / (w - 1) as f32;
            let y = (i / w) as f32 / (h - 1) as f32;
            [
                (x * 255.0) as u8,
                (y * 255.0) as u8,
                ((1.0 - x) * 200.0) as u8,
            ]
        })
        .collect();
    let params = AvifParams {
        quality: 85,
        ..AvifParams::default()
    };
    let encoded = encode_avif(&rgb, w, h, 3, &params, None).unwrap();
    let (decoded, dw, dh, channels) = decode_avif(&encoded).unwrap();
    assert_eq!((dw, dh, channels), (w, h, 3));
    assert_eq!(decoded.len(), rgb.len());
    let se: f64 = rgb
        .iter()
        .zip(&decoded)
        .map(|(&a, &b)| ((a as f64) - (b as f64)).powi(2))
        .sum();
    let rmse = (se / rgb.len() as f64).sqrt();
    assert!(rmse < 6.0, "roundtrip rmse too high: {rmse:.2}");
}

#[test]
fn probe_reports_dimensions_without_decoding() {
    let rgb = vec![128u8; 96 * 64 * 3];
    let encoded = encode_avif(&rgb, 96, 64, 3, &AvifParams::default(), None).unwrap();
    assert_eq!(probe_avif(&encoded).unwrap(), (96, 64));
}

#[test]
fn rgba_roundtrip_preserves_color_and_alpha() {
    let (w, h) = (160, 120);
    // Color gradient with an alpha ramp: left edge transparent,
    // right edge opaque.
    let rgba: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            let x = (i % w) as f32 / (w - 1) as f32;
            let y = (i / w) as f32 / (h - 1) as f32;
            [
                (x * 255.0) as u8,
                (y * 255.0) as u8,
                ((1.0 - x) * 200.0) as u8,
                (x * 255.0) as u8,
            ]
        })
        .collect();
    let params = AvifParams {
        quality: 85,
        alpha_quality: 85,
        ..AvifParams::default()
    };
    let encoded = encode_avif(&rgba, w, h, 4, &params, None).unwrap();
    let (decoded, dw, dh, channels) = decode_avif(&encoded).unwrap();
    assert_eq!((dw, dh, channels), (w, h, 4));
    let a_se: f64 = rgba
        .chunks_exact(4)
        .zip(decoded.chunks_exact(4))
        .map(|(s, d)| ((s[3] as f64) - (d[3] as f64)).powi(2))
        .sum();
    let a_rmse = (a_se / (w * h) as f64).sqrt();
    assert!(a_rmse < 3.0, "alpha rmse too high: {a_rmse:.2}");
    // Color must survive where alpha is meaningful.
    let (mut c_se, mut n) = (0f64, 0u32);
    for (s, d) in rgba.chunks_exact(4).zip(decoded.chunks_exact(4)) {
        if s[3] > 128 {
            for c in 0..3 {
                c_se += ((s[c] as f64) - (d[c] as f64)).powi(2);
            }
            n += 3;
        }
    }
    let c_rmse = (c_se / n as f64).sqrt();
    assert!(c_rmse < 8.0, "color rmse too high: {c_rmse:.2}");
}

#[test]
fn decode_rejects_garbage() {
    assert!(decode_avif(b"not an avif at all").is_err());
    assert!(decode_avif(&[]).is_err());
}

#[test]
fn yuv_conversion_hits_known_anchors() {
    // white -> Y=1023, Cb=Cr=512; black -> Y=0, Cb=Cr=512
    let (mut y, mut cb, mut cr) = (Vec::new(), Vec::new(), Vec::new());
    rgb_to_yuv420_10bit(
        &[255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        2,
        2,
        3,
        &mut y,
        &mut cb,
        &mut cr,
    );
    assert!(y.iter().all(|&v| v >= 1022), "{y:?}");
    assert_eq!((cb[0], cr[0]), (512, 512));
    let (mut y, mut cb, mut cr) = (Vec::new(), Vec::new(), Vec::new());
    rgb_to_yuv420_10bit(&[0; 12], 2, 2, 3, &mut y, &mut cb, &mut cr);
    assert!(y.iter().all(|&v| v == 0));
    assert_eq!((cb[0], cr[0]), (512, 512));
}
