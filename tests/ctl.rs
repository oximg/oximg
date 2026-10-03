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

/// #70: an RGBA PNG whose alpha is 255 everywhere is served exactly as
/// the same pixels stored as RGB, so OXIMG_PNG_QUANTIZE applies to it.
/// Through the real binary, with a resize, for PNG and WebP output.
#[test]
fn opaque_rgba_png_is_served_like_rgb() {
    let (w, h) = (160usize, 120usize);
    let rgb: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            let n = (x * 37 + y * 101) ^ (x * y);
            [(x * 3) as u8, (y * 5 + n % 7) as u8, (n % 251) as u8]
        })
        .collect();
    let rgba: Vec<u8> = rgb
        .chunks(3)
        .flat_map(|p| [p[0], p[1], p[2], 255])
        .collect();
    let encode = |color: png::ColorType, data: &[u8]| {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        writer.finish().unwrap();
        out
    };
    let dir = std::env::temp_dir().join(format!("oximg-ctl-opaque-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("rgb.png"), encode(png::ColorType::Rgb, &rgb)).unwrap();
    std::fs::write(dir.join("rgba.png"), encode(png::ColorType::Rgba, &rgba)).unwrap();
    let get = |path: &str, tag: &str| -> Vec<u8> {
        let out = dir.join(tag);
        let (code, v) = run(&[
            "--images-dir",
            dir.to_str().unwrap(),
            "--env",
            "OXIMG_PNG_QUANTIZE=1",
            "get",
            path,
            "--expect",
            "200",
            "--write",
            out.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "{v}");
        std::fs::read(&out).unwrap()
    };
    let (png_rgba, png_rgb) = (
        get("/resize/97/97/rgba.png", "a.png"),
        get("/resize/97/97/rgb.png", "b.png"),
    );
    let (webp_rgba, webp_rgb) = (
        get("/resize/97/97/rgba.png@webp", "a.webp"),
        get("/resize/97/97/rgb.png@webp", "b.webp"),
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert!(png_rgba == png_rgb, "PNG: opaque RGBA must serve like RGB");
    assert!(
        webp_rgba == webp_rgb,
        "WebP: opaque RGBA must serve like RGB"
    );
    let color = png::Decoder::new(std::io::Cursor::new(&png_rgba))
        .read_info()
        .unwrap()
        .info()
        .color_type;
    assert_eq!(color, png::ColorType::Indexed, "quantization must apply");
}
