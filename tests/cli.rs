//! One-shot CLI tests: run the real binary (`oximg resize`, `oximg
//! probe`) and verify outputs, format selection precedence, exit
//! codes, and that the `serve` subcommand still boots the server.

mod common;

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oximg"))
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Per-test scratch path (pid + name) so parallel tests never collide.
fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("oximg-cli-{}-{name}", std::process::id()))
}

/// Run resize with the given trailing args, assert success, and return
/// the probed (content_type, w, h, byte_len) of the written file.
fn resize_ok(out: &std::path::Path, extra: &[&str]) -> (String, usize, usize, usize) {
    let output = bin()
        .args(["resize", &fixture("photo.jpg"), "100", "100"])
        .arg(out)
        .args(extra)
        .output()
        .expect("run oximg resize");
    assert!(
        output.status.success(),
        "resize failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = std::fs::read(out).expect("read output");
    let (format, w, h) = oximg::pipeline::probe(&bytes).expect("probe output");
    std::fs::remove_file(out).ok();
    (format.content_type().to_string(), w, h, bytes.len())
}

#[test]
fn resize_fits_and_keeps_source_format() {
    let (ct, w, h, _) = resize_ok(&tmp("keep.jpg"), &[]);
    assert_eq!(ct, "image/jpeg");
    assert_eq!((w, h), (100, 75), "fit within 100x100, never enlarged");
}

/// Output format precedence: --format > output extension > source.
#[test]
fn resize_format_precedence() {
    // extension selects webp
    let (ct, ..) = resize_ok(&tmp("ext.webp"), &[]);
    assert_eq!(ct, "image/webp");
    // explicit flag beats a contradicting extension
    let (ct, ..) = resize_ok(&tmp("flag.jpg"), &["-f", "png"]);
    assert_eq!(ct, "image/png");
    // unknown extension keeps the source format
    let (ct, ..) = resize_ok(&tmp("plain.bin"), &[]);
    assert_eq!(ct, "image/jpeg");
}

/// `.gif` is the one output extension refused rather than ignored —
/// nothing here encodes GIF, and a WebP written under a `.gif` name is the
/// mislabeled output the server rejects `@gif` for. An explicit `-f` still
/// wins, since then the extension is never consulted at all.
#[test]
fn a_gif_output_extension_is_refused_before_anything_is_written() {
    let out = tmp("refused.gif");
    std::fs::remove_file(&out).ok();
    let output = bin()
        .args(["resize", &fixture("anim.gif"), "100", "100"])
        .arg(&out)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("gif output is not supported"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !out.exists(),
        "nothing may be written under the refused name"
    );

    // The escape hatch: the caller who names both format and file gets
    // both, as the documented precedence says.
    let (ct, ..) = resize_ok(&out, &["-f", "webp"]);
    assert_eq!(ct, "image/webp");
}

/// The quality flag must actually steer the encoder.
#[test]
fn resize_quality_flag_changes_output_size() {
    let (.., low) = resize_ok(&tmp("q30.jpg"), &["-q", "30"]);
    let (.., high) = resize_ok(&tmp("q95.jpg"), &["--quality", "95"]);
    assert!(
        low < high,
        "q30 ({low} bytes) must be smaller than q95 ({high} bytes)"
    );
}

/// 0 leaves an axis unconstrained (issue #2): width-only follows the
/// aspect ratio, and `0 0` is the pure re-encode at the source's own
/// size (never-enlarge means dimensions pass through).
#[test]
fn zero_axis_resizes_width_only_and_zero_zero_transcodes() {
    let out = tmp("wonly.jpg");
    let output = bin()
        .args(["resize", &fixture("photo.jpg"), "100", "0"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (_, w, h) = oximg::pipeline::probe(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!((w, h), (100, 75), "width-only follows the aspect ratio");
    std::fs::remove_file(&out).ok();

    // photo.jpg is 200x150; 0 0 re-encodes at the stored size.
    let out = tmp("transcode.webp");
    let output = bin()
        .args(["resize", &fixture("photo.jpg"), "0", "0"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (fmt, w, h) = oximg::pipeline::probe(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(fmt, oximg::pipeline::ImageFormat::Webp);
    assert_eq!((w, h), (200, 150), "0 0 keeps the source dimensions");
    std::fs::remove_file(&out).ok();
}

#[test]
fn probe_prints_format_and_dimensions_without_decoding() {
    let output = bin()
        .args(["probe", &fixture("photo.jpg")])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("image/jpeg"), "stdout: {stdout}");
    assert!(stdout.contains("stored pixels"), "stdout: {stdout}");
}

/// The probe line is a contract with the Ruby gem, which parses it
/// (`PROBE_LINE` in rubygem/oximg/lib/oximg.rb): the fields the gem
/// reports come first, the animation summary follows them after a
/// comma, and a still source prints no summary at all. Pinned exactly,
/// so a change to the grammar fails here and not only in the gem suite.
#[test]
fn probe_prints_the_animation_summary_after_the_fields_the_gem_reads() {
    let probe = |name: &str| {
        let output = bin().args(["probe", &fixture(name)]).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let expect = |name: &str, rest: &str| format!("{}: {rest}\n", fixture(name));

    assert_eq!(
        probe("anim.gif"),
        expect(
            "anim.gif",
            "image/gif 120x90 (10800 stored pixels), 3 frames, 1500ms, looping forever"
        )
    );
    assert_eq!(
        probe("animated.webp"),
        expect(
            "animated.webp",
            "image/webp 64x48 (3072 stored pixels), 2 frames, 200ms, looping forever"
        )
    );
    assert_eq!(
        probe("still.gif"),
        expect("still.gif", "image/gif 240x180 (43200 stored pixels)")
    );
}

/// A non-upright source prints its orientation and displayed size
/// first after the comma, where the gem reads it (#163); an upright one
/// prints the line above, unchanged.
#[test]
fn probe_prints_a_non_upright_orientation() {
    let px = vec![128u8; 64 * 48 * 3];
    for o in [2u16, 6] {
        let path = tmp(&format!("o{o}.jpg"));
        std::fs::write(&path, common::jpeg_with_orientation(&px, 64, 48, Some(o))).unwrap();
        let output = bin().arg("probe").arg(&path).output().unwrap();
        std::fs::remove_file(&path).ok();
        assert!(output.status.success());
        let shown = if o == 6 { "48x64" } else { "64x48" };
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "{}: image/jpeg 64x48 (3072 stored pixels), orientation {o} (displayed {shown})\n",
                path.display()
            )
        );
    }
}

/// A wrapped option description lines up under its first line. A `\`
/// continuation in a Rust string literal drops the next line's leading
/// whitespace, which is how the wrapped `--icc` text once printed flush
/// with the option names.
#[test]
fn help_aligns_wrapped_option_descriptions() {
    let output = bin().arg("--help").output().unwrap();
    let help = String::from_utf8(output.stdout).unwrap();
    let mut in_options = false;
    let mut wrapped = 0;
    for line in help.lines() {
        let indent = line.len() - line.trim_start().len();
        if indent == 10 && line.trim_start().starts_with('-') {
            // "          -q, --quality N    JPEG quality ..." : the
            // description starts at column 29.
            assert_eq!(&line[27..29], "  ", "{line:?}");
            assert_ne!(&line[29..30], " ", "{line:?}");
            in_options = true;
        } else if in_options && indent >= 10 {
            assert_eq!(indent, 29, "wrapped option text misaligned: {line:?}");
            wrapped += 1;
        } else {
            in_options = false;
        }
    }
    assert!(wrapped >= 4, "expected wrapped option lines:\n{help}");
}

/// Usage errors are exit 2 (distinct from processing failures, exit 1),
/// with a message on stderr and nothing written.
#[test]
fn usage_errors_exit_2() {
    for args in [
        &["resize"][..],
        &["resize", "in.jpg", "-1", "100", "out.jpg"][..],
        &["resize", "in.jpg", "wide", "100", "out.jpg"][..],
        &["resize", "in.jpg", "100", "100", "out.jpg", "-f", "gif"][..],
        // Nothing encodes GIF, so a `.gif` output name is refused rather
        // than ignored — writing WebP bytes under it would mislabel the
        // file, which is what the server rejects `@gif` for.
        &["resize", "in.jpg", "100", "100", "out.gif"][..],
        &["resize", "in.jpg", "100", "100", "out.jpg", "-q", "0"][..],
        &[
            "resize", "in.jpg", "100", "100", "out.jpg", "--preset", "bogus",
        ][..],
        &["resize", "in.jpg", "100", "100", "out.jpg", "--bogus"][..],
        &[
            "resize", "in.jpg", "100", "100", "out.jpg", "--icc", "maybe",
        ][..],
        &["resize", "in.jpg", "100", "100", "out.jpg", "--icc"][..],
        &[
            "resize",
            "in.jpg",
            "100",
            "100",
            "out.jpg",
            "--no-auto-rotate=1",
        ][..],
        &["probe"][..],
        &["frobnicate"][..],
        &["serve", "extra"][..],
    ] {
        let output = bin().args(args).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stderr.is_empty(), "{args:?}: silent usage error");
    }
}

/// A missing source is a processing failure (exit 1), not a usage error.
#[test]
fn missing_source_is_a_processing_failure() {
    let out = tmp("never.jpg");
    let output = bin()
        .args(["resize", "/nonexistent/x.jpg", "100", "100"])
        .arg(&out)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!out.exists(), "no output file on failure");
}

/// `oximg serve` is the explicit spelling of the bare-invocation
/// default: it must boot the same server (and still shut down
/// gracefully).
#[cfg(unix)]
#[test]
fn serve_subcommand_boots_the_server() {
    use std::io::BufRead;
    let mut child = bin()
        .arg("serve")
        .env("PORT", "0")
        .env("IMAGES_DIR", fixture(""))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn oximg serve");
    let stderr = child.stderr.take().unwrap();
    let mut lines = std::io::BufReader::new(stderr).lines();
    let listening = lines
        .find_map(|l| {
            let l = l.ok()?;
            l.starts_with("oximg listening on :").then_some(l)
        })
        .expect("serve never printed the listening line");
    assert!(listening.contains("workers"), "{listening}");
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    // Bounded wait, then assert a clean exit.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "serve did not exit after SIGTERM"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(status.success(), "exited {status}");
}

/// Resize photo.jpg into a 400px box with `envs` set; the run must
/// succeed. Returns the output bytes.
fn resize_with_env(name: &str, envs: &[(&str, &str)]) -> Vec<u8> {
    let out = tmp(name);
    let output = bin()
        .args(["resize", &fixture("photo.jpg"), "400", "400"])
        .arg(&out)
        .envs(envs.iter().copied())
        .output()
        .expect("run oximg resize");
    assert!(
        output.status.success(),
        "{envs:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = std::fs::read(&out).expect("read output");
    std::fs::remove_file(&out).ok();
    bytes
}

/// Startup validation trims knob values, so the readers must too: a
/// value that passes the check and is then ignored over whitespace is
/// the silent fail-open the check exists to prevent (" 30" validated,
/// then served at the default quality 75).
#[test]
fn padded_knob_values_take_effect() {
    let q = |v| resize_with_env("pad-q.webp", &[("OXIMG_WEBP_QUALITY", v)]);
    assert_ne!(q("30"), q("75"), "quality must change the output");
    assert_eq!(q(" 30 "), q("30"), "padded quality ignored");

    let e = |v| resize_with_env("pad-e.png", &[("OXIMG_PNG_EFFORT", v)]);
    assert_ne!(e("high"), e("fastest"), "effort must change the output");
    assert_eq!(e("\thigh "), e("high"), "padded effort ignored");
}

/// Issue #8: `OXIMG_PNG_EFFORT=9` is what the PNG ecosystem types, and
/// it used to refuse to start. zlib-style levels now select the named
/// level with the same deflate underneath — byte-identical output.
#[test]
fn png_effort_accepts_zlib_style_levels() {
    let e = |v| resize_with_env("zlib-e.png", &[("OXIMG_PNG_EFFORT", v)]);
    assert_eq!(e("9"), e("high"));
    assert_eq!(e("6"), e("balanced"));
    assert_eq!(e("1"), e("fastest"));
}

/// Issue #46: effort is cosmetic like `OXIMG_LOG`, so a value that is
/// neither a level name nor 0-9 warns — naming what would have been
/// accepted — and encodes exactly as if the knob were unset, instead
/// of refusing to start.
#[test]
fn unknown_png_effort_warns_and_uses_the_default() {
    // The child inherits the runner's env, so the baseline must remove
    // the knob explicitly or an ambient value would stand in for unset.
    let run = |out: &std::path::Path, effort: Option<&str>| {
        let mut cmd = bin();
        cmd.args(["resize", &fixture("photo.jpg"), "400", "400"])
            .arg(out)
            .env_remove("OXIMG_PNG_EFFORT");
        if let Some(v) = effort {
            cmd.env("OXIMG_PNG_EFFORT", v);
        }
        cmd.output().expect("run oximg resize")
    };
    let base = tmp("bad-effort-unset.png");
    let output = run(&base, None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let unset = std::fs::read(&base).expect("read output");
    std::fs::remove_file(&base).ok();
    for bad in ["10", "max", "High"] {
        let out = tmp("bad-effort.png");
        let output = run(&out, Some(bad));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{bad:?}: {stderr}");
        assert!(
            stderr.contains(&format!("oximg: warning: OXIMG_PNG_EFFORT={bad:?}")),
            "{bad:?}: {stderr}"
        );
        assert!(
            stderr.contains("0-9"),
            "must name the numeric form: {stderr}"
        );
        let bytes = std::fs::read(&out).expect("read output");
        std::fs::remove_file(&out).ok();
        assert_eq!(bytes, unset, "{bad:?} must encode as if unset");
    }
}

/// A 1600x1200 photo-like JPEG for the decode-scale policy, with chroma
/// sampled at the given pixel sizes ((2, 2) is 4:2:0).
fn policy_source(name: &str, chroma: (u8, u8)) -> std::path::PathBuf {
    let (w, h) = (1600, 1200);
    let mut seed = 0x9E3779B9u32;
    let mut px = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            for c in 0..3 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                px.push((x * 170 / w + y * 50 / h + c * 5 + (seed >> 28) as usize) as u8);
            }
        }
    }
    let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
    comp.set_size(w, h);
    comp.set_quality(90.0);
    comp.set_chroma_sampling_pixel_sizes(chroma, chroma);
    let mut started = comp.start_compress(Vec::new()).unwrap();
    started.write_scanlines(&px).unwrap();
    let path = tmp(name);
    std::fs::write(&path, started.finish().unwrap()).unwrap();
    path
}

/// The same 1600x1200 scene as a single-component grayscale JPEG.
fn policy_source_gray(name: &str) -> std::path::PathBuf {
    let (w, h) = (1600, 1200);
    let mut seed = 0x9E3779B9u32;
    let mut px = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            px.push((x * 170 / w + y * 50 / h + (seed >> 28) as usize) as u8);
        }
    }
    let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_GRAYSCALE);
    comp.set_size(w, h);
    comp.set_quality(90.0);
    let mut started = comp.start_compress(Vec::new()).unwrap();
    started.write_scanlines(&px).unwrap();
    let path = tmp(name);
    std::fs::write(&path, started.finish().unwrap()).unwrap();
    path
}

/// Grayscale sources take the same policy as 4:2:0: half size at 4x
/// and at the boundary, full size past it, below it, and when off.
#[test]
fn linear_shrink_policy_covers_grayscale() {
    let src = policy_source_gray("policy-gray.jpg");
    let (full, half) = ((1600, 1200), (800, 600));
    assert_eq!(decode_size(&src, 400, &[]).0, half);
    assert_eq!(decode_size(&src, 420, &[]).0, half);
    assert_eq!(decode_size(&src, 421, &[]).0, full);
    let (dims, on) = decode_size(&src, 533, &[]);
    assert_eq!(dims, full);
    let (_, off) = decode_size(&src, 533, &[("OXIMG_LINEAR_SHRINK", "0")]);
    assert!(on == off, "below 1.9x left, the output must not change");
    assert_eq!(
        decode_size(&src, 400, &[("OXIMG_LINEAR_SHRINK", "0")]).0,
        full
    );
    std::fs::remove_file(&src).ok();
}

/// Resize `src` to `width` wide with OXIMG_TIMING on; returns the
/// decoded size the timing line reports and the output bytes.
fn decode_size(
    src: &std::path::Path,
    width: u32,
    env: &[(&str, &str)],
) -> ((usize, usize), Vec<u8>) {
    // Named after the source too: the policy tests run in parallel.
    let out = tmp(&format!(
        "out-{}-{width}-{}.jpg",
        src.file_stem().unwrap().to_string_lossy(),
        env.len()
    ));
    let mut cmd = bin();
    cmd.args(["resize"])
        .arg(src)
        .args([&width.to_string(), "0"])
        .arg(&out)
        .env("OXIMG_TIMING", "1");
    for k in [
        "OXIMG_DCT_MARGIN",
        "OXIMG_LINEAR_SHRINK",
        "OXIMG_RESIZE",
        "OXIMG_RESIZE_BACKEND",
    ] {
        cmd.env_remove(k);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr
        .lines()
        .find(|l| l.starts_with("timing "))
        .unwrap_or_else(|| panic!("no timing line: {stderr}"));
    let dims = &line[line.find('(').unwrap() + 1..];
    let (w, rest) = dims.split_once('x').unwrap();
    let h: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let bytes = std::fs::read(&out).unwrap();
    std::fs::remove_file(&out).ok();
    ((w.parse().unwrap(), h.parse().unwrap()), bytes)
}

/// The default decode takes the linear-light 1/2 shrink once the half
/// size leaves at least 1.9x for the resampler, and decodes at full
/// size otherwise, when disabled, under the sRGB resize, or when a
/// margin is asked for.
#[test]
fn linear_shrink_policy_picks_the_decode_size() {
    let src = policy_source("policy.jpg", (2, 2));
    let full = (1600, 1200);
    let half = (800, 600);
    // 4x, the last width inside the 1.9x boundary (420x315: 8000 >= 19 * 420
    // and 6000 >= 19 * 315), and the first one past it.
    assert_eq!(decode_size(&src, 400, &[]).0, half);
    assert_eq!(decode_size(&src, 420, &[]).0, half);
    assert_eq!(decode_size(&src, 421, &[]).0, full);
    // Below the threshold the default is exactly the full decode.
    let (dims, default_3x) = decode_size(&src, 533, &[]);
    assert_eq!(dims, full);
    let (_, off_3x) = decode_size(&src, 533, &[("OXIMG_LINEAR_SHRINK", "0")]);
    assert!(
        default_3x == off_3x,
        "below 1.9x left, the output must not change"
    );
    // Off switches.
    assert_eq!(
        decode_size(&src, 400, &[("OXIMG_LINEAR_SHRINK", "0")]).0,
        full
    );
    assert_eq!(decode_size(&src, 400, &[("OXIMG_RESIZE", "srgb")]).0, full);
    // A margin keeps libjpeg's own scaling: same size, different pixels.
    let (dims, stock) = decode_size(&src, 400, &[("OXIMG_DCT_MARGIN", "2.0")]);
    assert_eq!(dims, half);
    let (_, linear) = decode_size(&src, 400, &[]);
    assert!(stock != linear, "a margin must not take the linear shrink");
    std::fs::remove_file(&src).ok();
    // 4:2:2 and 4:4:4 chroma decode at full size at any reduction.
    for chroma in [(2, 1), (1, 1)] {
        let src = policy_source(&format!("policy-{}x{}.jpg", chroma.0, chroma.1), chroma);
        assert_eq!(decode_size(&src, 400, &[]).0, full, "{chroma:?}");
        std::fs::remove_file(&src).ok();
    }
}

#[test]
fn linear_shrink_knob_is_fail_closed() {
    let out = tmp("bad-shrink.jpg");
    let output = bin()
        .args(["resize", &fixture("photo.jpg"), "100", "100"])
        .arg(&out)
        .env("OXIMG_LINEAR_SHRINK", "off")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!out.exists(), "nothing may be written on a fatal config");
}

/// The fallback is scoped to effort: its fail-closed neighbours in the
/// same table still refuse to start.
#[test]
fn unknown_png_quantize_is_still_fatal() {
    let out = tmp("bad-quantize.png");
    let output = bin()
        .args(["resize", &fixture("photo.jpg"), "100", "100"])
        .arg(&out)
        .env("OXIMG_PNG_QUANTIZE", "yes")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("oximg: fatal: OXIMG_PNG_QUANTIZE=\"yes\""),
        "{stderr}"
    );
    assert!(!out.exists(), "nothing may be written on a fatal config");
}

/// Every chunk of a PNG as (type, data).
fn png_chunks(b: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    let mut v = Vec::new();
    let mut i = 8;
    while i + 12 <= b.len() {
        let len = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize;
        v.push((
            b[i + 4..i + 8].try_into().unwrap(),
            b[i + 8..i + 8 + len].to_vec(),
        ));
        i += 12 + len;
    }
    v
}

/// An APNG keeps its animation control and every frame's data, byte
/// for byte, while its text goes; a second strip changes nothing.
#[test]
fn strip_keeps_every_apng_frame() {
    let (w, h) = (16u32, 8u32);
    let mut src_bytes = Vec::new();
    {
        let mut e = png::Encoder::new(&mut src_bytes, w, h);
        e.set_color(png::ColorType::Rgb);
        e.set_depth(png::BitDepth::Eight);
        e.set_animated(3, 0).unwrap();
        e.add_text_chunk("Comment".into(), "SECRET".into()).unwrap();
        let mut wr = e.write_header().unwrap();
        for f in 0..3u8 {
            wr.set_frame_delay(f as u16 + 1, 10).unwrap();
            let px: Vec<u8> = (0..w * h * 3)
                .map(|i| (i as u8).wrapping_mul(f + 1))
                .collect();
            wr.write_image_data(&px).unwrap();
        }
    }
    let src = tmp("strip-src.png");
    let out = tmp("strip-out.png");
    let again = tmp("strip-again.png");
    std::fs::write(&src, &src_bytes).unwrap();
    for (from, to) in [(&src, &out), (&out, &again)] {
        let st = bin().arg("strip").arg(from).arg(to).status().unwrap();
        assert!(st.success());
    }
    let (out_bytes, again_bytes) = (std::fs::read(&out).unwrap(), std::fs::read(&again).unwrap());
    for p in [&src, &out, &again] {
        let _ = std::fs::remove_file(p);
    }
    assert!(!out_bytes.windows(6).any(|w| w == b"SECRET"));
    assert_eq!(again_bytes, out_bytes, "second strip changes nothing");
    let expect: Vec<_> = png_chunks(&src_bytes)
        .into_iter()
        .filter(|(t, _)| t != b"tEXt")
        .collect();
    assert_eq!(png_chunks(&out_bytes), expect);
    let types: Vec<[u8; 4]> = expect.iter().map(|c| c.0).collect();
    for t in [b"acTL", b"fcTL", b"IDAT", b"fdAT"] {
        assert!(types.contains(t), "{}", String::from_utf8_lossy(t));
    }
}

/// `oximg strip`: a JPEG carrying an XMP block and a profile comes back
/// without the XMP, the profile kept by default and dropped by
/// `--icc strip` or `OXIMG_ICC=0` (the flag wins over the knob), and
/// the summary names what went.
#[test]
fn strip_removes_metadata_and_honors_icc() {
    let px = vec![128u8; 64 * 48 * 3];
    let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
    comp.set_size(64, 48);
    let mut started = comp.start_compress(Vec::new()).unwrap();
    started.write_marker(
        mozjpeg::Marker::APP(1),
        b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta>SECRET</x:xmpmeta>",
    );
    started.write_marker(mozjpeg::Marker::APP(2), b"ICC_PROFILE\0\x01\x01PROFILE");
    started.write_scanlines(&px).unwrap();
    let src_bytes = started.finish().unwrap();
    let src = tmp("strip-src.jpg");
    std::fs::write(&src, &src_bytes).unwrap();
    let has = |b: &[u8], n: &[u8]| b.windows(n.len()).any(|w| w == n);

    for (args, env_icc, keeps_icc) in [
        (&[][..], None, true),
        (&["--icc", "strip"][..], None, false),
        (&["--icc", "keep"][..], Some("0"), true),
        (&[][..], Some("0"), false),
    ] {
        let out = tmp("strip-out.jpg");
        let mut cmd = bin();
        cmd.arg("strip").arg(&src).arg(&out).args(args);
        cmd.env_remove("OXIMG_ICC");
        if let Some(v) = env_icc {
            cmd.env("OXIMG_ICC", v);
        }
        let output = cmd.output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{args:?}: {stderr}");
        let bytes = std::fs::read(&out).unwrap();
        std::fs::remove_file(&out).ok();
        assert!(!has(&bytes, b"SECRET"), "{args:?}");
        assert_eq!(
            has(&bytes, b"ICC_PROFILE"),
            keeps_icc,
            "{args:?} {env_icc:?}"
        );
        assert!(stderr.contains("image/jpeg; removed "), "{stderr}");
        assert!(stderr.contains("XMP"), "{stderr}");
    }
    std::fs::remove_file(&src).ok();
}

#[test]
fn strip_refusals_and_usage_errors() {
    let out = tmp("strip-refused.out");
    let run = |args: &[&str]| {
        bin()
            .arg("strip")
            .args(args)
            .env_remove("OXIMG_ICC")
            .output()
            .unwrap()
    };
    let o = out.to_str().unwrap();
    let (jpg, gif, cmyk) = (
        fixture("photo.jpg"),
        fixture("still.gif"),
        fixture("cmyk_icc.jpg"),
    );
    // Usage errors exit 2.
    for args in [
        &[][..],
        &[&jpg[..]][..],
        &[&jpg, o, "--icc", "maybe"][..],
        &[&jpg, o, "--quality", "80"][..],
    ] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
    // Processing refusals exit 1 and write nothing.
    for (args, says) in [
        (&[&gif[..], o][..], "JPEG, PNG and WebP"),
        (&[&cmyk, o, "--icc", "strip"][..], "CMYK"),
    ] {
        let output = run(args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
        assert!(stderr.contains(says), "{stderr}");
        assert!(!out.exists(), "{args:?} wrote output");
    }
    // Nothing to strip is not an error, and changes nothing.
    let webp = fixture("photo.webp");
    let output = run(&[&webp, o]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("nothing to remove"));
    assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&webp).unwrap());
    std::fs::remove_file(&out).ok();
}
/// Issue #128: `--icc keep|strip` overrides `OXIMG_ICC` per call, the
/// same precedence as `strip` (flag > knob > keep), and the default
/// keeps a source profile.
#[test]
fn resize_icc_flag_overrides_the_knob() {
    let icc = common::fake_icc(900);
    let app2 = common::app2_icc_payloads(&icc, 60_000);
    let px = vec![128u8; 64 * 48 * 3];
    let src = tmp("icc-src.jpg");
    std::fs::write(
        &src,
        common::jpeg_with_markers(&px, 64, 48, &[(2, &app2[0])]),
    )
    .unwrap();
    for (args, env_icc, keeps) in [
        (&[][..], None, true),
        (&["--icc", "strip"][..], None, false),
        (&["--icc", "keep"][..], Some("0"), true),
        (&["--icc", "strip"][..], Some("1"), false),
        (&[][..], Some("0"), false),
    ] {
        let out = tmp("icc-out.jpg");
        let mut cmd = bin();
        cmd.arg("resize").arg(&src).args(["0", "0"]).arg(&out);
        cmd.args(args).env_remove("OXIMG_ICC");
        if let Some(v) = env_icc {
            cmd.env("OXIMG_ICC", v);
        }
        let output = cmd.output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let bytes = std::fs::read(&out).unwrap();
        std::fs::remove_file(&out).ok();
        let want = keeps.then_some(&icc[..]);
        assert_eq!(
            common::jpeg_icc(&bytes).as_deref(),
            want,
            "{args:?} {env_icc:?}"
        );
    }
    std::fs::remove_file(&src).ok();
}

/// Issue #128: `--no-auto-rotate` keeps an orientation-6 source's
/// stored layout (64x48 stays 64x48, corners where they were stored),
/// and `--auto-rotate` forces the upright 48x64 even under
/// `OXIMG_AUTO_ROTATE=0`.
#[test]
fn resize_auto_rotate_flags_override_the_knob() {
    let display = common::corner_base(48, 64, 16);
    let (stored, sw, sh) = common::store_for_orientation(&display, 48, 64, 6);
    assert_eq!((sw, sh), (64, 48));
    let src = tmp("rot-src.jpg");
    std::fs::write(
        &src,
        common::jpeg_with_orientation(&stored, sw, sh, Some(6)),
    )
    .unwrap();
    let (_, _, stored_corners) =
        common::corner_classes(&common::jpeg_with_orientation(&stored, sw, sh, None));
    for (args, env, upright) in [
        (&[][..], None, true),
        (&["--no-auto-rotate"][..], None, false),
        (&["--no-auto-rotate"][..], Some("1"), false),
        (&["--auto-rotate"][..], Some("0"), true),
        (&[][..], Some("0"), false),
    ] {
        let out = tmp("rot-out.jpg");
        let mut cmd = bin();
        cmd.arg("resize").arg(&src).args(["0", "0"]).arg(&out);
        cmd.args(args).env_remove("OXIMG_AUTO_ROTATE");
        if let Some(v) = env {
            cmd.env("OXIMG_AUTO_ROTATE", v);
        }
        let output = cmd.output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let bytes = std::fs::read(&out).unwrap();
        std::fs::remove_file(&out).ok();
        let got = common::corner_classes(&bytes);
        let want = if upright {
            (48, 64, ['R', 'G', 'B', 'W'])
        } else {
            (64, 48, stored_corners)
        };
        assert_eq!(got, want, "{args:?} {env:?}");
    }
    std::fs::remove_file(&src).ok();
}
