//! Drive the `oximg-ctl` binary the way an agent does: JSON on stdout,
//! the real `oximg` binary underneath, committed fixtures, no network.

use serde_json::Value;
use std::process::Command;

fn ctl() -> Command {
    // Hyphenated extra bins don't always get CARGO_BIN_EXE_* in the
    // integration-test crate; they do land next to `oximg`.
    let oximg = std::path::Path::new(env!("CARGO_BIN_EXE_oximg"));
    let ctl = oximg.with_file_name(if cfg!(windows) {
        "oximg-ctl.exe"
    } else {
        "oximg-ctl"
    });
    let mut c = Command::new(&ctl);
    c.arg("--bin").arg(oximg);
    c
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn run(args: &[&str]) -> (i32, Value) {
    let output = ctl()
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run oximg-ctl {args:?}: {e}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "oximg-ctl {args:?} stdout was not JSON ({e}): {stdout:?}; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.code().unwrap_or(1), v)
}

#[test]
fn help_is_plain_text_not_json() {
    let output = ctl().arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("oximg-ctl"), "{stdout}");
    assert!(stdout.contains("matrix"), "{stdout}");
    assert!(stdout.contains("--port"), "{stdout}");
    assert!(
        serde_json::from_str::<Value>(stdout.trim()).is_err(),
        "help must stay human text so --help is greppable"
    );
}

#[test]
fn unknown_command_is_usage_json() {
    let (code, v) = run(&["nope"]);
    assert_eq!(code, 2);
    assert_eq!(v["ok"], false);
    assert!(
        v["error"].as_str().unwrap().contains("unknown command"),
        "{v}"
    );
    assert_eq!(v["hint"], "oximg-ctl --help");
}

#[test]
fn pretty_indents_usage_json() {
    let output = ctl().args(["--pretty", "nope"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains('\n') && stdout.contains("\"ok\": false"),
        "expected indented JSON, got {stdout:?}"
    );
}

#[test]
fn probe_photo_jpg() {
    let (code, v) = run(&["probe", &fixture("photo.jpg")]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["probe"]["content_type"], "image/jpeg");
    assert_eq!(v["probe"]["width"], 200);
    assert_eq!(v["probe"]["height"], 150);
    assert_eq!(v["probe"]["jpeg"]["sof"], 0);
    assert_eq!(v["probe"]["jpeg"]["progressive"], false);
    let (code, v) = run(&["probe", &fixture("cmyk_prog.jpg")]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["probe"]["jpeg"]["sof"], 2);
    assert_eq!(v["probe"]["jpeg"]["progressive"], true);
}

#[test]
fn probe_anim_gif_reports_frames() {
    let (code, v) = run(&["probe", &fixture("anim.gif")]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["probe"]["content_type"], "image/gif");
    assert_eq!(v["probe"]["animation"]["frames"], 3);
    assert_eq!(v["probe"]["animation"]["duration_ms"], 1500);
}

#[test]
fn probe_garbage_fails_closed() {
    let (code, v) = run(&["probe", &fixture("list.txt")]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["ok"], false);
}

/// Shared vector with tests/server.rs `signing_gate` and the Rails gem.
#[test]
fn sign_matches_the_server_vector() {
    let key = "deadbeef".repeat(8);
    let salt = "cafebabe".repeat(8);
    let (code, v) = run(&[
        "sign",
        "/resize/100/100/photo.jpg",
        "--key",
        &key,
        "--salt",
        &salt,
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        v["signature"],
        "t-jKRoyvzhs4dEBnGGBUS_t6Uh_HE6WysfGYvs8UaTo"
    );
    assert_eq!(
        v["url"],
        "/t-jKRoyvzhs4dEBnGGBUS_t6Uh_HE6WysfGYvs8UaTo/resize/100/100/photo.jpg"
    );

    let (code, v) = run(&[
        "sign",
        "/resize/100/100/photo.jpg@webp",
        "--key",
        &key,
        "--salt",
        &salt,
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        v["signature"],
        "XQ8C3eYRVAkFAnUczGBsuXMOu-J6vMoYi3W8_4-sT6Q"
    );

    // Percent-decoded form is what the server verifies.
    let (code, v) = run(&[
        "sign",
        "/resize/100/100/albums%2F2026%2Fphoto.jpg",
        "--key",
        &key,
        "--salt",
        &salt,
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["path"], "/resize/100/100/albums/2026/photo.jpg");
    assert_eq!(
        v["signature"],
        "i1gy8Dm1yo32_9FMzrRj8MDG_c0F0kJDV22jAgvUCow"
    );
}

#[test]
fn sign_rejects_malformed_escapes_and_bad_hex_as_usage() {
    let key = "deadbeef".repeat(8);
    let salt = "cafebabe".repeat(8);
    let (code, v) = run(&[
        "sign",
        "/resize/100/100/photo%.jpg",
        "--key",
        &key,
        "--salt",
        &salt,
    ]);
    assert_eq!(code, 2, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("percent-escape"),
        "{v}"
    );

    let (code, v) = run(&[
        "sign",
        "/resize/100/100/photo.jpg",
        "--key",
        "not-hex",
        "--salt",
        &salt,
    ]);
    assert_eq!(code, 2, "{v}");
    assert!(v["error"].as_str().unwrap().contains("hex"), "{v}");

    let (code, v) = run(&[
        "sign",
        "/resize/100/100/photo.jpg",
        "--key",
        "",
        "--salt",
        &salt,
    ]);
    assert_eq!(code, 2, "{v}");
    assert!(v["error"].as_str().unwrap().contains("empty"), "{v}");
}

#[test]
fn sign_reencodes_percent_in_the_url() {
    let key = "deadbeef".repeat(8);
    let salt = "cafebabe".repeat(8);
    let (code, v) = run(&[
        "sign",
        "/resize/100/100/a%25b.jpg",
        "--key",
        &key,
        "--salt",
        &salt,
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["path"], "/resize/100/100/a%b.jpg");
    let url = v["url"].as_str().unwrap();
    assert!(
        url.ends_with("/resize/100/100/a%25b.jpg"),
        "decoded % must be re-encoded in the URL: {url}"
    );
    assert!(
        !url.contains("/a%b.jpg"),
        "raw % in a URL is not sendable: {url}"
    );
}

#[test]
fn get_resizes_photo_and_probes_the_body() {
    let (code, v) = run(&["get", "/resize/100/100/photo.jpg", "--expect", "200"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["status"], 200);
    assert_eq!(v["content_type"], "image/jpeg");
    assert_eq!(v["probe"]["width"], 100);
    assert_eq!(v["probe"]["height"], 75);
    assert!(v["bytes"].as_u64().unwrap() > 0);
    assert_eq!(v["sha256"].as_str().unwrap().len(), 64, "sha256 hex");
}

#[test]
fn get_parallel_resize_with_fractional_band_boundaries() {
    // 150 / 117 makes the last band's bottom round to 150.00000000000003.
    let (code, v) = run(&[
        "--env",
        "OXIMG_PAR=4",
        "get",
        "/resize/156/156/photo.jpg",
        "--expect",
        "200",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["status"], 200);
    assert_eq!(v["content_type"], "image/jpeg");
    assert_eq!(v["probe"]["width"], 156);
    assert_eq!(v["probe"]["height"], 117);
}

#[test]
fn get_maps_error_classes() {
    let (code, v) = run(&["get", "/resize/0/0/photo.jpg"]);
    assert_eq!(code, 0, "a 400 is a successful observation: {v}");
    assert_eq!(v["status"], 400);
    assert!(
        v["body_text"]
            .as_str()
            .unwrap()
            .contains("invalid dimensions"),
        "{v}"
    );

    let (code, v) = run(&["get", "/resize/100/100/missing.jpg", "--expect", "404"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["status"], 404);

    let (code, v) = run(&["get", "/resize/100/100/photo.jpg@gif", "--expect", "400"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["status"], 400);

    let (code, v) = run(&["get", "/resize/100/100/photo.jpg", "--expect", "404"]);
    assert_eq!(code, 1, "expect mismatch must fail the process: {v}");
    assert_eq!(v["ok"], false);
    assert_eq!(v["status"], 200);
}

#[test]
fn get_cross_format_token() {
    let (code, v) = run(&["get", "/resize/100/100/photo.jpg@webp", "--expect", "200"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["content_type"], "image/webp");
    assert_eq!(v["probe"]["content_type"], "image/webp");
}

/// RGBA8 samples and dims of a PNG file.
fn png_rgba(path: &str) -> (u32, u32, Vec<u8>) {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let mut r = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .unwrap();
    let mut buf = vec![0u8; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut buf).unwrap();
    assert_eq!(info.color_type, png::ColorType::Rgba, "{path}");
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

/// A box larger than the source resizes nothing: the decoded frame is
/// handed straight to the encoder (issue #129, no second full-frame
/// copy). Through the server that must still be lossless into PNG,
/// alpha included, keep every animation frame, and serve the other
/// full-frame formats at their stored size.
#[test]
fn get_exact_size_hands_the_frame_through_unchanged() {
    let out = std::env::temp_dir().join(format!("oximg-ctl-exact-{}.png", std::process::id()));
    let out_s = out.to_str().unwrap();
    let (code, v) = run(&[
        "get",
        "/resize/400/400/rgba.png",
        "--expect",
        "200",
        "--write",
        out_s,
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["content_type"], "image/png");
    assert_eq!(
        (&v["probe"]["width"], &v["probe"]["height"]),
        (&200.into(), &150.into())
    );
    assert!(
        png_rgba(out_s) == png_rgba(&fixture("rgba.png")),
        "exact-size PNG must reproduce the source RGBA bytes"
    );
    let _ = std::fs::remove_file(&out);

    let (code, v) = run(&["get", "/resize/400/400/alpha.webp@png", "--expect", "200"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        (&v["probe"]["width"], &v["probe"]["height"]),
        (&200.into(), &150.into())
    );

    // JPEG cannot carry alpha: the handed-over RGBA frame flattens.
    let (code, v) = run(&["get", "/resize/400/400/rgba.png@jpg", "--expect", "200"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["content_type"], "image/jpeg");
    assert_eq!(
        (&v["probe"]["width"], &v["probe"]["height"]),
        (&200.into(), &150.into())
    );

    // The animated-GIF loop keeps its canvas across frames instead.
    let (code, v) = run(&["get", "/resize/400/400/anim.gif", "--expect", "200"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["probe"]["animation"]["frames"], 3, "{v}");
    assert_eq!(
        (&v["probe"]["width"], &v["probe"]["height"]),
        (&120.into(), &90.into())
    );
}

/// AVIF `irot` at exact size: the handed-over frame still rotates
/// (stored 240x180 displays 180x240).
#[cfg(feature = "avif")]
#[test]
fn get_exact_size_avif_still_rotates() {
    let (code, v) = run(&[
        "get",
        "/resize/400/400/orient_irot1.avif@png",
        "--expect",
        "200",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        (&v["probe"]["width"], &v["probe"]["height"]),
        (&180.into(), &240.into())
    );
}

#[test]
fn resize_shells_out_to_the_cli() {
    let out = std::env::temp_dir().join(format!("oximg-ctl-test-{}.jpg", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let (code, v) = run(&[
        "resize",
        &fixture("photo.jpg"),
        "80",
        "80",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["probe"]["content_type"], "image/jpeg");
    assert_eq!(v["probe"]["width"], 80);
    assert_eq!(v["probe"]["height"], 60);
    assert!(out.is_file(), "resize must write the file it reports");
    let _ = std::fs::remove_file(&out);
}

/// Issue #130: `resize` reports the child's peak RSS, and parses the
/// decoded-bytes report into JSON when oximg prints one, so a memory
/// claim is one ctl call: the estimate beside what the process held.
#[test]
fn resize_reports_peak_rss_and_the_decoded_bytes_estimate() {
    let (code, v) = run(&[
        "--env",
        "OXIMG_LOG_DECODED_BYTES_ABOVE=1",
        "resize",
        &fixture("photo.jpg"),
        "100",
        "100",
    ]);
    assert_eq!(code, 0, "{v}");
    if cfg!(unix) {
        assert!(v["peak_rss_bytes"].as_u64().unwrap() > 1 << 20, "{v}");
    }
    let d = &v["decoded_bytes"];
    assert_eq!(d["format"], "JPEG", "{v}");
    let terms = d["terms"].as_object().unwrap();
    let names: Vec<&str> = terms.keys().map(String::as_str).collect();
    for name in [
        "staged",
        "resize_input",
        "output",
        "whole_source",
        "encode",
        "compressed",
    ] {
        assert!(names.contains(&name), "missing term {name}: {v}");
    }
    let sum: u64 = terms.values().map(|t| t.as_u64().unwrap()).sum();
    assert_eq!(d["total"].as_u64(), Some(sum), "terms sum to the total");
    // A 100x75 JPEG from the row kernel: `out8` and the encoder only.
    assert_eq!(terms["output"].as_u64(), Some(100 * 75 * 3), "{v}");

    // No report asked for, no field.
    let (_, v) = run(&["resize", &fixture("photo.jpg"), "100", "100"]);
    assert!(v.get("decoded_bytes").is_none(), "{v}");

    // A file name containing the report's own words still parses.
    let dir = std::env::temp_dir().join(format!("oximg-ctl-delim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let odd = dir.join("a decode needs about b bytes (c.jpg");
    std::fs::copy(fixture("photo.jpg"), &odd).unwrap();
    let (code, v) = run(&[
        "--env",
        "OXIMG_LOG_DECODED_BYTES_ABOVE=1",
        "resize",
        odd.to_str().unwrap(),
        "100",
        "100",
    ]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["decoded_bytes"]["format"], "JPEG", "{v}");
    assert_eq!(v["decoded_bytes"]["total"], d["total"], "{v}");
}

#[test]
fn resize_usage_errors_exit_2() {
    let out = std::env::temp_dir().join(format!("oximg-ctl-usage-{}.jpg", std::process::id()));
    let (code, v) = run(&[
        "resize",
        &fixture("photo.jpg"),
        "80",
        "80",
        "--out",
        out.to_str().unwrap(),
        "-q",
        "0",
    ]);
    assert_eq!(code, 2, "{v}");
    assert_eq!(v["ok"], false);
}

#[test]
fn matrix_dry_run_is_the_plan() {
    let (code, v) = run(&[
        "--dry-run",
        "matrix",
        "--source",
        "photo.jpg",
        "--box",
        "100x100",
        "--format",
        "source",
        "--format",
        "webp",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["dry_run"], true);
    let cells = v["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 5, "2 positives + 3 negatives: {cells:?}");
    assert_eq!(cells[0]["path"], "/resize/100/100/photo.jpg");
    assert_eq!(cells[1]["path"], "/resize/100/100/photo.jpg@webp");
    assert_eq!(cells[2]["expect"], 400);
    assert_eq!(cells[3]["expect"], 404);
    assert_eq!(cells[4]["path"], "/resize/100/100/photo.jpg@gif");
}

#[test]
fn matrix_negatives_omit_present_missing_jpg() {
    let dir = std::env::temp_dir().join(format!("oximg-ctl-has-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(fixture("photo.jpg"), dir.join("photo.jpg")).unwrap();
    std::fs::copy(fixture("photo.jpg"), dir.join("missing.jpg")).unwrap();
    let (code, v) = run(&[
        "--dry-run",
        "--images-dir",
        dir.to_str().unwrap(),
        "matrix",
        "--source",
        "photo.jpg",
        "--box",
        "100x100",
        "--format",
        "source",
    ]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{v}");
    let cells = v["cells"].as_array().unwrap();
    assert!(
        cells
            .iter()
            .all(|c| c["path"] != "/resize/100/100/missing.jpg"),
        "must not 404 a file that exists: {cells:?}"
    );
}

#[test]
fn matrix_negatives_omit_missing_jpg_directory() {
    let dir = std::env::temp_dir().join(format!("oximg-ctl-missdir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(fixture("photo.jpg"), dir.join("photo.jpg")).unwrap();
    std::fs::create_dir_all(dir.join("missing.jpg")).unwrap();
    let (code, v) = run(&[
        "--dry-run",
        "--images-dir",
        dir.to_str().unwrap(),
        "matrix",
        "--source",
        "photo.jpg",
        "--box",
        "100x100",
        "--format",
        "source",
    ]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{v}");
    let cells = v["cells"].as_array().unwrap();
    assert!(
        cells
            .iter()
            .all(|c| c["path"] != "/resize/100/100/missing.jpg"),
        "a directory named missing.jpg is not a 404: {cells:?}"
    );
}

#[test]
fn matrix_env_source_base_skips_local_oracle_negatives() {
    let (code, v) = run(&[
        "--dry-run",
        "--env",
        "OXIMG_SOURCE_BASE_URL=https://example.invalid/",
        "matrix",
        "--source",
        "photo.jpg",
        "--box",
        "100x100",
        "--format",
        "source",
        "--format",
        "webp",
    ]);
    assert_eq!(code, 0, "{v}");
    let cells = v["cells"].as_array().unwrap();
    assert!(
        cells
            .iter()
            .all(|c| c["path"] != "/resize/100/100/missing.jpg"),
        "remote origin is not the fixture tree: {cells:?}"
    );
}

#[test]
fn matrix_base_skips_the_local_missing_file_negative() {
    let (code, v) = run(&[
        "--dry-run",
        "matrix",
        "--base",
        "http://127.0.0.1:9",
        "--source",
        "photo.jpg",
        "--box",
        "100x100",
        "--format",
        "source",
        "--format",
        "webp",
    ]);
    assert_eq!(code, 0, "{v}");
    let cells = v["cells"].as_array().unwrap();
    assert_eq!(
        cells.len(),
        4,
        "no missing.jpg against a foreign tree: {cells:?}"
    );
    assert!(
        cells
            .iter()
            .all(|c| c["path"] != "/resize/100/100/missing.jpg"),
        "{cells:?}"
    );
}

#[test]
fn matrix_runs_against_a_spawned_server() {
    let (code, v) = run(&[
        "matrix",
        "--source",
        "photo.jpg",
        "--box",
        "100x100",
        "--format",
        "source",
        "--no-negatives",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["failed"], 0);
    assert_eq!(v["total"], 1);
    assert_eq!(v["cells"][0]["pass"], true);
    assert_eq!(v["cells"][0]["status"], 200);
    assert_eq!(v["cells"][0]["probe"]["width"], 100);
    assert_eq!(v["cells"][0]["probe"]["height"], 75);
}

#[test]
fn serve_dry_run_names_the_binary() {
    let (code, v) = run(&["--dry-run", "serve"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["dry_run"], true);
    assert!(
        v["bin"].as_str().unwrap().contains("oximg"),
        "bin={}",
        v["bin"]
    );
}

#[test]
fn serve_dry_run_reports_env_under_the_real_key() {
    let (code, v) = run(&["--env", "OXIMG_LOG=request", "--dry-run", "serve"]);
    assert_eq!(code, 0, "{v}");
    let env = v["env"].as_array().expect("env array");
    assert_eq!(env.len(), 1, "{v}");
    assert_eq!(env[0]["OXIMG_LOG"], "request", "{v}");
    assert!(env[0].get("k").is_none(), "json! ident trap: {v}");
}

#[test]
fn serve_dry_run_redacts_signing_secrets() {
    let (code, v) = run(&[
        "--env",
        "OXIMG_KEY=deadbeef",
        "--env",
        "OXIMG_SALT=cafebabe",
        "--env",
        "OXIMG_LOG=request",
        "--dry-run",
        "serve",
    ]);
    assert_eq!(code, 0, "{v}");
    let env = v["env"].as_array().expect("env array");
    let find = |k: &str| {
        env.iter()
            .find_map(|o| o.get(k).and_then(|x| x.as_str()))
            .unwrap_or_else(|| panic!("missing {k} in {v}"))
    };
    assert_eq!(find("OXIMG_KEY"), "<redacted>");
    assert_eq!(find("OXIMG_SALT"), "<redacted>");
    assert_eq!(find("OXIMG_LOG"), "request");
}

#[test]
fn empty_bind_env_does_not_inherit_parent() {
    let (code, v) = run(&[
        "--env",
        "OXIMG_BIND=",
        "get",
        "/resize/100/100/photo.jpg",
        "--expect",
        "200",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["status"], 200, "{v}");
}

#[test]
fn env_last_bind_wins() {
    let (code, v) = run(&[
        "--env",
        "OXIMG_BIND=0.0.0.0",
        "--env",
        "OXIMG_BIND=127.0.0.1",
        "--dry-run",
        "serve",
    ]);
    assert_eq!(code, 0, "{v}");
    let env = v["env"].as_array().expect("env array");
    let binds: Vec<_> = env
        .iter()
        .filter_map(|o| o.get("OXIMG_BIND").and_then(|x| x.as_str()))
        .collect();
    assert_eq!(binds.last().copied(), Some("127.0.0.1"), "{v}");
}

#[test]
fn auto_spawn_ignores_inherited_source_base_url() {
    let mut c = ctl();
    c.env("OXIMG_SOURCE_BASE_URL", "https://example.invalid/");
    let output = c
        .args(["get", "/resize/100/100/photo.jpg", "--expect", "200"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout not JSON ({e}): {stdout:?}"));
    assert_eq!(output.status.code(), Some(0), "{v}");
    assert_eq!(v["status"], 200, "{v}");
}

#[test]
fn auto_spawn_ignores_inherited_signing_keys() {
    let mut c = ctl();
    c.env("OXIMG_KEY", "deadbeef".repeat(8));
    c.env("OXIMG_SALT", "cafebabe".repeat(8));
    let output = c
        .args(["get", "/resize/100/100/photo.jpg", "--expect", "200"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout not JSON ({e}): {stdout:?}"));
    assert_eq!(output.status.code(), Some(0), "{v}");
    assert_eq!(v["status"], 200, "{v}");
}

#[test]
fn auto_spawn_does_not_complete_signing_from_the_parent() {
    let mut c = ctl();
    c.env("OXIMG_SALT", "cafebabe".repeat(8));
    let output = c
        .args([
            "--env",
            &format!("OXIMG_KEY={}", "deadbeef".repeat(8)),
            "get",
            "/resize/100/100/photo.jpg",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout not JSON ({e}): {stdout:?}"));
    assert_ne!(
        output.status.code(),
        Some(0),
        "half-signing must not silently 403 unsigned URLs: {v}"
    );
    assert_eq!(v["ok"], false, "{v}");
}

#[test]
fn env_bind_and_workers_are_canonicalized() {
    let (code, v) = run(&[
        "--env",
        "oximg_workers=2",
        "--env",
        "oximg_bind=127.0.0.1",
        "--dry-run",
        "serve",
    ]);
    assert_eq!(code, 0, "{v}");
    let env = v["env"].as_array().expect("env array");
    let find = |k: &str| {
        env.iter()
            .find_map(|o| o.get(k).and_then(|x| x.as_str()))
            .unwrap_or_else(|| panic!("missing {k} in {v}"))
    };
    assert_eq!(find("OXIMG_WORKERS"), "2");
    assert_eq!(find("OXIMG_BIND"), "127.0.0.1");
    assert!(env.iter().all(|o| o.get("oximg_workers").is_none()));
}

#[test]
fn env_cannot_override_managed_spawn_keys() {
    let (code, v) = run(&["--env", "PORT=8081", "--dry-run", "serve"]);
    assert_eq!(code, 2, "{v}");
    assert!(v["error"].as_str().unwrap().contains("PORT"), "{v}");

    let (code, v) = run(&["--env", "port=8081", "--dry-run", "serve"]);
    assert_eq!(code, 2, "Windows-style case fold: {v}");
    assert!(v["error"].as_str().unwrap().contains("PORT"), "{v}");

    let (code, v) = run(&["--env", "IMAGES_DIR=/tmp", "--dry-run", "serve"]);
    assert_eq!(code, 2, "{v}");
    assert!(v["error"].as_str().unwrap().contains("IMAGES_DIR"), "{v}");
}

#[test]
fn matrix_rejects_unknown_format_tokens() {
    let (code, v) = run(&["--dry-run", "matrix", "--format", "bogus"]);
    assert_eq!(code, 2, "{v}");
    assert!(v["error"].as_str().unwrap().contains("--format"), "{v}");
}

#[test]
fn matrix_rejects_invalid_boxes_as_usage() {
    let (code, v) = run(&["--dry-run", "matrix", "--box", "0x0"]);
    assert_eq!(code, 2, "{v}");
    assert!(v["error"].as_str().unwrap().contains("0x0"), "{v}");

    let (code, v) = run(&["--dry-run", "matrix", "--box", "8193x100"]);
    assert_eq!(code, 2, "{v}");
    assert!(v["error"].as_str().unwrap().contains("8192"), "{v}");
}

#[test]
fn matrix_sniffs_source_bytes_not_the_extension() {
    let dir = std::env::temp_dir().join(format!("oximg-ctl-sniff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(fixture("photo.jpg"), dir.join("mismatch.png")).unwrap();
    let output = ctl()
        .args([
            "--images-dir",
            dir.to_str().unwrap(),
            "matrix",
            "--source",
            "mismatch.png",
            "--format",
            "source",
            "--no-negatives",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout not JSON ({e}): {stdout:?}"));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(output.status.code(), Some(0), "{v}");
    assert_eq!(v["cells"][0]["pass"], true, "{v}");
    assert_eq!(v["cells"][0]["content_type"], "image/jpeg", "{v}");
}

#[test]
fn matrix_encodes_percent_in_source_names() {
    let (code, v) = run(&[
        "--dry-run",
        "matrix",
        "--source",
        "a%b.jpg",
        "--format",
        "source",
        "--no-negatives",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["cells"][0]["path"], "/resize/100/100/a%25b.jpg");
}

#[test]
fn sign_rejects_non_ascii_hex_without_panicking() {
    let (code, v) = run(&[
        "sign",
        "/resize/100/100/photo.jpg",
        "--key",
        "€a",
        "--salt",
        "cafebabe",
    ]);
    assert_eq!(code, 2, "{v}");
    assert!(v["error"].as_str().unwrap().contains("hex"), "{v}");
}

#[test]
fn resize_dry_run_does_not_delete_an_existing_out() {
    let out =
        std::env::temp_dir().join(format!("oximg-ctl-dryrun-keep-{}.out", std::process::id()));
    std::fs::write(&out, b"keep").unwrap();
    let (code, v) = run(&[
        "--dry-run",
        "resize",
        &fixture("photo.jpg"),
        "80",
        "80",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(std::fs::read(&out).unwrap(), b"keep");
    let _ = std::fs::remove_file(&out);
}

#[test]
fn resize_without_out_does_not_leave_a_temp_file() {
    let (code, v) = run(&["resize", &fixture("photo.jpg"), "80", "80"]);
    assert_eq!(code, 0, "{v}");
    assert!(
        v.get("out").is_none(),
        "ephemeral output must not linger: {v}"
    );
    assert_eq!(v["probe"]["width"], 80);
}

/// Live `serve`: one ready JSON object, `/health` answers, SIGTERM on
/// the wrapper reaps the oximg child. SIGKILL would skip handlers.
#[test]
#[cfg(unix)]
fn serve_prints_ready_json_and_reaps_the_child() {
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};

    let mut child = ctl()
        .arg("serve")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn oximg-ctl serve");
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    std::thread::spawn(move || {
        let mut sink = std::io::sink();
        let _ = std::io::copy(&mut { stderr }, &mut sink);
    });
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("serve ready JSON");
    let v: Value = serde_json::from_str(line.trim())
        .unwrap_or_else(|e| panic!("serve stdout was not JSON ({e}): {line:?}"));
    assert_eq!(v["ok"], true, "{v}");
    let port = v["port"].as_u64().expect("port") as u16;
    let oximg_pid = v["pid"].as_u64().expect("pid");
    let resp = ureq::get(format!("http://127.0.0.1:{port}/health"))
        .call()
        .expect("GET /health");
    assert_eq!(resp.status().as_u16(), 200);
    drop(resp);

    assert!(
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("oximg-ctl serve did not exit after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let still = Command::new("kill")
        .args(["-0", &oximg_pid.to_string()])
        .status()
        .unwrap();
    assert!(
        !still.success(),
        "oximg child {oximg_pid} still running after wrapper exit"
    );
}

/// Ctrl-C during parity reaches the spawned server, not the wrapper. The
/// run must stop and say so, not count the dead server's refusals as
/// differences between the builds, wherever the signal lands.
#[test]
#[cfg(unix)]
fn parity_interrupted_during_side_a_reports_it_instead_of_a_diff() {
    assert_parity_interrupted_after_listening(1);
}

#[test]
#[cfg(unix)]
fn parity_interrupted_during_side_b_reports_it_instead_of_a_diff() {
    assert_parity_interrupted_after_listening(2);
}

/// SIGINT a parity run once its `nth` server has printed its listening
/// line: that server is starting up or serving, and both must end in
/// "interrupted". Ten boxes per source keep each side busy well past any
/// scheduling delay before the signal, so a side cannot finish first.
#[cfg(unix)]
fn assert_parity_interrupted_after_listening(nth: usize) {
    use std::io::{BufRead, BufReader};
    use std::time::Duration;

    let oximg = env!("CARGO_BIN_EXE_oximg");
    let mut args = vec!["parity", "--against", oximg];
    for b in [
        "100x100", "90x90", "80x80", "70x70", "60x60", "50x50", "40x40", "333x0", "0x111", "64x64",
    ] {
        args.extend(["--box", b]);
    }
    let mut child = ctl()
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn oximg-ctl parity");
    let mut stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = BufReader::new(stderr).lines();
        let mut seen = 0;
        for line in lines.by_ref().map_while(Result::ok) {
            if line.starts_with("oximg listening on :") {
                seen += 1;
                if seen == nth {
                    let _ = tx.send(());
                    break;
                }
            }
        }
        for _ in lines {}
    });
    rx.recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("server {nth} never listened"));
    assert!(
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert_parity_reported_interrupted(child, &mut stdout);
}

/// Ctrl-C before the server prints its listening line kills it outright
/// (oximg installs its own handlers just before that line), so the
/// wrapper sees stderr close with no port. That is the caller's stop,
/// not a broken --bin, and must read "interrupted" too. The --bin here
/// sleeps before exec'ing oximg; the trap makes it exit at once on the
/// signal under both bash and dash, before it ever listens.
#[test]
#[cfg(unix)]
fn parity_interrupted_before_the_server_listens_reports_it() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    let dir = std::env::temp_dir().join(format!(
        "oximg-ctl-parity-slow-start-{}",
        std::process::id()
    ));
    // A run killed from outside (a CI timeout) can leave a dir with a
    // stale marker under a PID this run may reuse; start empty.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let slow = dir.join("oximg");
    let started = dir.join("started");
    // The marker proves ctl already spawned side A, so its SIGINT
    // handler is installed: the signal cannot hit ctl's default action.
    std::fs::write(
        &slow,
        format!(
            "#!/bin/sh\ntrap 'exit 130' INT TERM\n: > '{}'\nsleep 5 </dev/null >/dev/null 2>&1 &\nwait $!\nexec '{}' \"$@\"\n",
            started.display(),
            env!("CARGO_BIN_EXE_oximg")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    let oximg = env!("CARGO_BIN_EXE_oximg");
    let mut child = ctl()
        .args([
            "--bin",
            slow.to_str().unwrap(),
            "parity",
            "--against",
            oximg,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn oximg-ctl parity");
    let mut stdout = child.stdout.take().expect("stdout piped");
    // Side A is running and will not listen for 5 s.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !started.exists() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&dir);
            panic!("side A's wrapper never started");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let sent = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap()
        .success();
    // Clean up before asserting, so a failure leaves nothing behind.
    let _ = std::fs::remove_dir_all(&dir);
    assert!(sent, "kill -INT failed");
    assert_parity_reported_interrupted(child, &mut stdout);
}

/// Wait for a signalled parity run and require "interrupted": exit 1,
/// ok false, and no verdict.
#[cfg(unix)]
fn assert_parity_reported_interrupted(
    mut child: std::process::Child,
    stdout: &mut std::process::ChildStdout,
) {
    use std::io::Read;
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Ok(Some(st)) = child.try_wait() {
            break st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("oximg-ctl parity did not exit after SIGINT");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut out = String::new();
    stdout.read_to_string(&mut out).unwrap();
    let v: Value = serde_json::from_str(out.trim())
        .unwrap_or_else(|e| panic!("parity stdout was not JSON ({e}): {out:?}"));
    assert_eq!(status.code(), Some(1), "{v}");
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().contains("interrupted"), "{v}");
    assert!(
        v.get("differ").is_none(),
        "no verdict on a cut-short run: {v}"
    );
}

/// Subsampled chroma is replicated (libjpeg's merged upsampler) when the
/// resize reduces and triangle-filtered at 1:1. A 4:2:0 source whose
/// chroma alternates at its own Nyquist rate (constant luma, Cb +-40 per
/// chroma column) separates the two: the triangle filter maps it to
/// exactly half the amplitude, replication keeps all of it, and a mild
/// Lanczos reduction passes that frequency almost untouched.
#[test]
fn jpeg_chroma_is_replicated_when_reducing_and_filtered_at_one_to_one() {
    let (w, h) = (256usize, 32usize);
    // YCbCr (128, 128 +- 40, 128) in RGB; 2-pixel runs so the encoder's
    // 2x2 box downsample lands exactly on +-40.
    let px: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            if (i % w) / 2 % 2 == 0 {
                [128, 114, 199]
            } else {
                [128, 142, 57]
            }
        })
        .collect();
    let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
    comp.set_size(w, h);
    comp.set_quality(100.0);
    let mut started = comp.start_compress(Vec::new()).unwrap();
    started.write_scanlines(&px).unwrap();
    let jpg = started.finish().unwrap();

    let dir = std::env::temp_dir().join(format!("oximg-ctl-chroma-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("chroma.jpg"), &jpg).unwrap();
    let get = |path: &str, tag: &str| -> (usize, Vec<u8>) {
        let out = dir.join(format!("{tag}.png"));
        let (code, v) = run(&[
            "--images-dir",
            dir.to_str().unwrap(),
            "get",
            path,
            "--expect",
            "200",
            "--write",
            out.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "{v}");
        let mut r = png::Decoder::new(std::io::Cursor::new(std::fs::read(&out).unwrap()))
            .read_info()
            .unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        let info = r.next_frame(&mut buf).unwrap();
        assert_eq!(info.color_type, png::ColorType::Rgb, "{path}");
        buf.truncate(info.buffer_size());
        (info.width as usize, buf)
    };
    // Mean |B - mean B| along the middle row, away from the edges.
    let amplitude = |ow: usize, rgb: &[u8]| -> f64 {
        let row = &rgb[rgb.len() / 2 / (ow * 3) * ow * 3..][..ow * 3];
        let b: Vec<f64> = row[8 * 3..(ow - 8) * 3]
            .iter()
            .skip(2)
            .step_by(3)
            .map(|&v| v as f64)
            .collect();
        let mean = b.iter().sum::<f64>() / b.len() as f64;
        b.iter().map(|v| (v - mean).abs()).sum::<f64>() / b.len() as f64
    };

    let (w11, same) = get("/resize/256/32/chroma.jpg@png", "same");
    let (wr, reduced) = get("/resize/224/28/chroma.jpg@png", "reduced");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!((w11, wr), (256, 224));

    // 1:1 is the plain libjpeg decode, triangle filter included.
    let mut dec = mozjpeg::Decompress::new_mem(&jpg).unwrap().rgb().unwrap();
    let fancy: Vec<u8> = dec.read_scanlines().unwrap();
    assert!(
        same == fancy,
        "1:1 must equal libjpeg's default (fancy) decode"
    );

    let (a11, ar) = (amplitude(w11, &same), amplitude(wr, &reduced));
    assert!(
        ar > 1.5 * a11,
        "reduced chroma amplitude {ar:.1} vs 1:1 {a11:.1}: expected replication (~2x)"
    );
}

/// The linear-light RGB8 PNG path streams decoded rows into the resize
/// on single-band requests and stages the frame for band-parallel ones
/// (issue #129). Through the binary, both must answer the fitted size —
/// including an output height that does not divide the source and a
/// 2000-row source reduced to one row — and a stream that ends early
/// must still be a 422, never a partially resized image.
#[test]
fn png_rgb8_resize_streams_and_truncation_is_422() {
    let dir = std::env::temp_dir().join(format!("oximg-ctl-pngstream-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let rgb = std::fs::read(fixture("rgb.png")).unwrap();
    std::fs::write(dir.join("rgb.png"), &rgb).unwrap();
    std::fs::write(dir.join("cut.png"), &rgb[..rgb.len() / 3]).unwrap();
    let (tw, th) = (4u32, 2000u32);
    let mut tall = Vec::new();
    let mut enc = png::Encoder::new(&mut tall, tw, th);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let px: Vec<u8> = (0..tw * th * 3).map(|i| (i * 7 % 251) as u8).collect();
    enc.write_header().unwrap().write_image_data(&px).unwrap();
    std::fs::write(dir.join("tall.png"), &tall).unwrap();

    let images = dir.to_str().unwrap();
    for par in ["1", "4"] {
        let env = format!("OXIMG_PAR={par}");
        let get = |path: &str, expect: &str| {
            let (code, v) = run(&[
                "--images-dir",
                images,
                "--env",
                &env,
                "get",
                path,
                "--expect",
                expect,
            ]);
            assert_eq!(code, 0, "PAR={par} {path}: {v}");
            v
        };
        // 200x150 into 37x37: 37x28, and 150 / 28 is not whole.
        let v = get("/resize/37/37/rgb.png", "200");
        assert_eq!(v["content_type"], "image/png", "{v}");
        assert_eq!(
            (v["probe"]["width"].as_u64(), v["probe"]["height"].as_u64()),
            (Some(37), Some(28)),
            "{v}"
        );
        let v = get("/resize/1/1/tall.png", "200");
        assert_eq!(
            (v["probe"]["width"].as_u64(), v["probe"]["height"].as_u64()),
            (Some(1), Some(1)),
            "{v}"
        );
        let v = get("/resize/37/37/cut.png", "422");
        assert_eq!(v["body_text"], "decode PNG", "{v}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// --dry-run prints the plan without spawning, reading, or signing.
#[test]
fn dry_runs_print_the_plan_without_side_effects() {
    let (code, v) = run(&[
        "--dry-run",
        "get",
        "resize/1/1/photo.jpg",
        "--base",
        "http://127.0.0.1:1",
        "--expect",
        "200",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["dry_run"], true);
    assert_eq!(
        v["path"], "/resize/1/1/photo.jpg",
        "a leading slash is added"
    );
    assert_eq!(v["expect"], 200);

    let (code, v) = run(&["--dry-run", "probe", "/no/such/file"]);
    assert_eq!(code, 0, "a dry run does not read the file: {v}");
    assert_eq!(v["file"], "/no/such/file");

    let key = "deadbeef".repeat(8);
    let (code, v) = run(&[
        "--dry-run",
        "sign",
        "/a%20b.jpg",
        "--key",
        &key,
        "--salt",
        &key,
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        v["path"], "/a b.jpg",
        "the plan names the decoded path it would sign"
    );
    assert!(v.get("signature").is_none(), "{v}");
}

#[test]
fn get_reports_an_unreachable_base_as_a_failed_proof() {
    // Port 1 on loopback refuses at once; nothing is spawned for --base.
    let (code, v) = run(&["get", "/health", "--base", "http://127.0.0.1:1"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["ok"], false);
    assert!(
        v["hint"].as_str().unwrap().contains("is the server up"),
        "{v}"
    );
}

#[test]
fn probe_of_a_missing_file_names_the_working_directory() {
    let (code, v) = run(&["probe", "no/such/file.jpg"]);
    assert_eq!(code, 1, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("no/such/file.jpg"),
        "{v}"
    );
    assert!(
        v["hint"]
            .as_str()
            .unwrap()
            .contains("current working directory"),
        "{v}"
    );
}

#[test]
fn resize_with_a_missing_binary_says_which() {
    // ctl() passes the real --bin first; a later --bin wins.
    let (code, v) = run(&[
        "--bin",
        "/no/such/oximg",
        "resize",
        &fixture("photo.jpg"),
        "10",
        "10",
    ]);
    assert_eq!(code, 1, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("/no/such/oximg"),
        "{v}"
    );
    assert!(v["hint"].as_str().unwrap().contains("--bin"), "{v}");
}

/// A build compared with itself agrees on every cell it compares.
#[test]
fn parity_of_a_build_with_itself_passes() {
    let oximg = env!("CARGO_BIN_EXE_oximg");
    let (code, v) = run(&[
        "parity",
        "--against",
        oximg,
        "--source",
        "photo.jpg",
        "--source",
        "anim.gif",
        "--box",
        "100x100",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["cells"], 4, "2 sources × (bare, @webp)");
    assert_eq!(v["compared"], 4);
    assert_eq!(v["differ"], 0);
}

/// The instrument must be able to fail: a build whose server encodes at
/// another quality differs, and parity says where.
#[cfg(unix)]
#[test]
fn parity_catches_a_build_that_encodes_differently() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("oximg-ctl-parity-skew-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let skewed = dir.join("oximg");
    std::fs::write(
        &skewed,
        format!(
            "#!/bin/sh\nQUALITY=50 exec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_oximg")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&skewed, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (code, v) = run(&[
        "parity",
        "--against",
        skewed.to_str().unwrap(),
        "--source",
        "photo.jpg",
        "--box",
        "100x100",
        "--format",
        "source",
    ]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["ok"], false);
    assert_eq!(v["differ"], 1);
    assert_eq!(v["compared"], 0);
    let cell = &v["differing"][0];
    assert_eq!(cell["path"], "/resize/100/100/photo.jpg");
    // Both builds served the image; only the encode differs. A broken
    // --against (a 404, a refused connection) must not pass this test.
    assert_eq!(
        (cell["a"]["status"].as_u64(), cell["b"]["status"].as_u64()),
        (Some(200), Some(200))
    );
    assert!(cell["b"]["sha256"].is_string(), "{v}");
    assert_ne!(cell["a"]["sha256"], cell["b"]["sha256"]);
}

#[test]
fn parity_dry_run_is_the_plan_and_needs_against() {
    let (code, v) = run(&["parity"]);
    assert_eq!(code, 2, "{v}");
    assert!(v["error"].as_str().unwrap().contains("--against"), "{v}");

    let (code, v) = run(&[
        "--dry-run",
        "parity",
        "--against",
        "/other/oximg",
        "--source",
        "photo.jpg",
        "--box",
        "64x0",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["b"], "/other/oximg");
    assert_eq!(
        v["cells"],
        serde_json::json!(["/resize/64/0/photo.jpg", "/resize/64/0/photo.jpg@webp"])
    );
}

/// The segments of a JPEG up to SOS, minus every APP2, then the rest.
fn without_app2(jpeg: &[u8]) -> Vec<u8> {
    let mut out = jpeg[..2].to_vec();
    let mut i = 2;
    while jpeg[i] == 0xFF && jpeg[i + 1] != 0xDA {
        let end = i + 2 + u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        if jpeg[i + 1] != 0xE2 {
            out.extend_from_slice(&jpeg[i..end]);
        }
        i = end;
    }
    out.extend_from_slice(&jpeg[i..]);
    out
}

/// A box at least the source's size re-encodes the JPEG at its own
/// size, which streams decoder rows into the encoder instead of
/// staging the frame (issue #129). Over HTTP the body is the bytes the
/// library's staged decode + encode makes, and a source profile still
/// lands ahead of the scan: the profiled body minus its APP2 chain is
/// the unprofiled one.
#[test]
fn get_exact_size_jpeg_matches_the_staged_encode() {
    let dir = std::env::temp_dir().join(format!("oximg-ctl-exact-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let photo = std::fs::read(fixture("photo.jpg")).unwrap();
    let icc: Vec<u8> = (0..900usize).map(|i| ((i * 131 + 7) % 251) as u8).collect();
    let mut app2 = vec![0xFF, 0xE2];
    app2.extend_from_slice(&((2 + 14 + icc.len()) as u16).to_be_bytes());
    app2.extend_from_slice(b"ICC_PROFILE\0\x01\x01");
    app2.extend_from_slice(&icc);
    let mut profiled = photo[..2].to_vec();
    profiled.extend_from_slice(&app2);
    profiled.extend_from_slice(&photo[2..]);
    std::fs::write(dir.join("photo.jpg"), &photo).unwrap();
    std::fs::write(dir.join("icc.jpg"), &profiled).unwrap();

    let body = |name: &str| {
        let out = dir.join(format!("{name}.out"));
        let (code, v) = run(&[
            "--images-dir",
            dir.to_str().unwrap(),
            "get",
            &format!("/resize/1000/1000/{name}"),
            "--expect",
            "200",
            "--write",
            out.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "{v}");
        assert_eq!(v["content_type"], "image/jpeg", "{v}");
        assert_eq!(
            (&v["probe"]["width"], &v["probe"]["height"]),
            (&200.into(), &150.into())
        );
        std::fs::read(out).unwrap()
    };
    let plain = body("photo.jpg");
    let (rgb, w, h) = oximg::pipeline::decode_and_resize(&photo, 1000, 1000, 1).unwrap();
    assert_eq!((w, h), (200, 150));
    let staged = oximg::pipeline::encode(&rgb, w, h, &oximg::pipeline::Params::default()).unwrap();
    assert!(
        plain == staged,
        "streamed body differs from the staged encode"
    );

    let with_icc = body("icc.jpg");
    assert!(
        with_icc.windows(icc.len()).any(|w| w == icc),
        "profile dropped"
    );
    assert!(
        without_app2(&with_icc) == plain,
        "profile changed more than APP2"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
