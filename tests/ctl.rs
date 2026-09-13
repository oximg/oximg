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
fn resize_without_out_does_not_leave_a_temp_file() {
    let (code, v) = run(&["resize", &fixture("photo.jpg"), "80", "80"]);
    assert_eq!(code, 0, "{v}");
    assert!(
        v.get("out").is_none(),
        "ephemeral output must not linger: {v}"
    );
    assert_eq!(v["probe"]["width"], 80);
}
