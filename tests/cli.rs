//! One-shot CLI tests: run the real binary (`oximg resize`, `oximg
//! probe`) and verify outputs, format selection precedence, exit
//! codes, and that the `serve` subcommand still boots the server.

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

/// Usage errors are exit 2 (distinct from processing failures, exit 1),
/// with a message on stderr and nothing written.
#[test]
fn usage_errors_exit_2() {
    for args in [
        &["resize"][..],
        &["resize", "in.jpg", "-1", "100", "out.jpg"][..],
        &["resize", "in.jpg", "wide", "100", "out.jpg"][..],
        &["resize", "in.jpg", "100", "100", "out.jpg", "-f", "gif"][..],
        &["resize", "in.jpg", "100", "100", "out.jpg", "-q", "0"][..],
        &[
            "resize", "in.jpg", "100", "100", "out.jpg", "--preset", "bogus",
        ][..],
        &["resize", "in.jpg", "100", "100", "out.jpg", "--bogus"][..],
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
