//! Fault catalog and compiled-artifact checks, shared by the tool and CI tests.
use crate::s3_harness::{Reply, S3, s3_env};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

pub fn origin() -> std::io::Result<S3> {
    let counts = Mutex::new(HashMap::<String, usize>::new());
    S3::spawn(move |path, _| {
        let mut counts = counts.lock().unwrap();
        let count = counts.entry(path.to_string()).or_default();
        *count += 1;
        let attempt = *count;
        drop(counts);
        if path.starts_with("/no-bucket/") {
            return Reply::error("404 Not Found", "NoSuchBucket");
        }
        if path.starts_with("/denied/") {
            return Reply::error("403 Forbidden", "AccessDenied");
        }
        if path.starts_with("/bad-probe-") {
            let body = "<Error><Code>AccessDenied</Code></Error>";
            let raw = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Length: 128\r\nConnection: close\r\n\r\n{body}"
            );
            if path.starts_with("/bad-probe-timeout/") {
                return Reply::raw(raw).then(Duration::from_secs(5), Vec::new());
            }
            if path.starts_with("/bad-probe-cap/") {
                let mut bytes = format!("HTTP/1.1 403 Forbidden\r\nContent-Length: 16385\r\nConnection: close\r\n\r\n{body}").into_bytes();
                bytes.extend(vec![b' '; 16385 - body.len()]);
                return Reply::raw(bytes);
            }
            return Reply::raw(raw);
        }
        // The path-style key length is decoded bytes, independent of oximg's decoder.
        let key = path.splitn(3, '/').nth(2).unwrap_or("");
        let mut bytes = key.bytes();
        let mut decoded_len = 0;
        while let Some(b) = bytes.next() {
            if b == b'%' {
                let mut next = bytes.clone();
                if next.next().is_some_and(|b| b.is_ascii_hexdigit())
                    && next.next().is_some_and(|b| b.is_ascii_hexdigit())
                {
                    bytes = next;
                }
            }
            decoded_len += 1;
        }
        if decoded_len > 1024 {
            return Reply::error("400 Bad Request", "KeyTooLongError");
        }
        if path.ends_with("/photo.jpg") || path.ends_with("/x") {
            return Reply::photo();
        }
        match path {
            "/test-bucket/space%20name.jpg" | "/test-bucket/plus%2Bsign.jpg"
            | "/test-bucket/100%25.jpg" | "/test-bucket/%E4%B8%AD%E6%96%87.jpg"
            | "/test-bucket/a%3Bb%3Dc%26d.jpg" | "/test-bucket/literal%252E.jpg" => Reply::photo(),
            "/test-bucket/region.jpg" => Reply::error("400 Bad Request", "AuthorizationHeaderMalformed"),
            "/test-bucket/denied.jpg" => Reply::error("403 Forbidden", "AccessDenied"),
            "/test-bucket/expired.jpg" => Reply::error("400 Bad Request", "ExpiredToken"),
            "/test-bucket/invalid-key.jpg" => Reply::error("400 Bad Request", "InvalidObjectName"),
            "/test-bucket/redirect.jpg" => Reply::raw("HTTP/1.1 307 Temporary Redirect\r\nLocation: /test-bucket/photo.jpg\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
            "/test-bucket/always503.jpg" => Reply::error("503 Service Unavailable", "SlowDown"),
            "/test-bucket/flaky.jpg" => {
                if attempt == 1 { Reply::error("503 Service Unavailable", "SlowDown") }
                else { Reply::photo() }
            }
            "/test-bucket/timeout400.jpg" => Reply::raw("HTTP/1.1 400 Bad Request\r\nContent-Length: 128\r\nConnection: close\r\n\r\n<Error><Code>")
                .then(Duration::from_secs(5), Vec::new()),
            "/test-bucket/truncated404.jpg" => Reply::raw("HTTP/1.1 404 Not Found\r\nContent-Length: 128\r\nConnection: close\r\n\r\n<Error><Code>NoSuch"),
            "/test-bucket/oversized404.jpg" => {
                let mut raw = b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4001\r\n".to_vec();
                raw.extend(vec![b' '; 16385]);
                raw.extend_from_slice(b"\r\n0\r\n\r\n");
                Reply::raw(raw)
            }
            "/test-bucket/retry-headers.jpg" | "/test-bucket/retry-body.jpg" => {
                let delay = Duration::from_millis(650);
                if attempt == 1 {
                    Reply::error("503 Service Unavailable", "SlowDown").delay_headers(delay)
                } else if path.ends_with("headers.jpg") {
                    Reply::photo().delay_headers(delay)
                } else {
                    let photo = include_bytes!("../../tests/fixtures/photo.jpg");
                    Reply::raw(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", photo.len()))
                        .then(delay, photo.to_vec())
                }
            }
            _ => Reply::error("404 Not Found", "NoSuchKey"),
        }
    })
}

struct Case {
    name: &'static str,
    key: String,
    wire_key: String,
    status: u16,
    attempts: usize,
    prefix: String,
    extra: Vec<(&'static str, &'static str)>,
    boot_error: Option<&'static str>,
}

impl Case {
    fn new(name: &'static str, key: &str, status: u16, attempts: usize) -> Self {
        Self {
            name,
            key: key.into(),
            wire_key: key.into(),
            status,
            attempts,
            prefix: String::new(),
            extra: Vec::new(),
            boot_error: None,
        }
    }
}

pub fn check(ctl: &Path) -> Result<Value> {
    let ctl = ctl
        .canonicalize()
        .context("build oximg and oximg-ctl, or supply --ctl PATH")?;
    let bin = ctl.with_file_name(if cfg!(windows) { "oximg.exe" } else { "oximg" });
    let server = origin()?;
    let mut cases = vec![
        Case::new("image", "photo.jpg", 200, 1),
        Case::new("missing", "missing.jpg", 404, 1),
        Case::new("region", "region.jpg", 500, 1),
        Case::new("denied", "denied.jpg", 500, 1),
        Case::new("expired", "expired.jpg", 500, 1),
        Case::new("invalid-key", "invalid-key.jpg", 400, 1),
        Case::new("redirect", "redirect.jpg", 500, 1),
        Case::new("flaky", "flaky.jpg", 200, 2),
        Case::new("retry-exhausted", "always503.jpg", 502, 2),
        Case::new("error-body-timeout", "timeout400.jpg", 504, 1),
        Case::new("error-body-truncated", "truncated404.jpg", 502, 1),
        Case::new("error-body-over-cap", "oversized404.jpg", 502, 1),
        Case::new("retry-headers-deadline", "retry-headers.jpg", 504, 2),
        Case::new("retry-body-deadline", "retry-body.jpg", 504, 2),
        Case::new("key-at-limit", &"a".repeat(1024), 404, 1),
        Case::new("key-over-limit", &"a".repeat(1025), 404, 0),
    ];
    for (name, key, wire_key) in [
        ("space", "space%20name.jpg", "space%20name.jpg"),
        ("plus", "plus+sign.jpg", "plus%2Bsign.jpg"),
        ("percent", "100%25.jpg", "100%25.jpg"),
        (
            "unicode",
            "%E4%B8%AD%E6%96%87.jpg",
            "%E4%B8%AD%E6%96%87.jpg",
        ),
        ("delimiters", "a;b=c&d.jpg", "a%3Bb%3Dc%26d.jpg"),
        ("double-encoding", "literal%252E.jpg", "literal%252E.jpg"),
    ] {
        let mut case = Case::new(name, key, 200, 1);
        case.wire_key = wire_key.into();
        cases.push(case);
    }
    let mut session = Case::new("session-token", "photo.jpg", 200, 1);
    session
        .extra
        .push(("AWS_SESSION_TOKEN", "test-session-token"));
    cases.push(session);
    let mut capped = Case::new("source-byte-cap", "photo.jpg", 413, 1);
    capped.extra.push(("OXIMG_MAX_SOURCE_BYTES", "1"));
    cases.push(capped);
    for (name, len) in [("long-prefix", 1006), ("prefix-at-limit", 1022)] {
        let mut case = Case::new(name, "x", 200, 1);
        case.prefix = "p".repeat(len);
        cases.push(case);
    }

    for (name, setting, value, reason, requests) in [
        (
            "boot-bad-secret",
            "AWS_SECRET_ACCESS_KEY",
            "wrong-secret",
            "SignatureDoesNotMatch",
            1,
        ),
        (
            "boot-bad-region",
            "AWS_REGION",
            "invalid.region",
            "not a region name",
            0,
        ),
        (
            "boot-missing-bucket",
            "OXIMG_SOURCE_BASE_URL",
            "s3://no-bucket",
            "NoSuchBucket",
            1,
        ),
        (
            "boot-truncated-body",
            "OXIMG_SOURCE_BASE_URL",
            "s3://bad-probe-eof",
            "could not read",
            1,
        ),
        (
            "boot-body-timeout",
            "OXIMG_SOURCE_BASE_URL",
            "s3://bad-probe-timeout",
            "could not read",
            1,
        ),
        (
            "boot-body-cap",
            "OXIMG_SOURCE_BASE_URL",
            "s3://bad-probe-cap",
            "16384-byte limit",
            1,
        ),
    ] {
        let mut case = Case::new(name, "photo.jpg", 500, requests);
        case.extra.push((setting, value));
        case.boot_error = Some(reason);
        cases.push(case);
    }
    let mut impossible = Case::new("boot-impossible-prefix", "x", 500, 0);
    impossible.prefix = "p".repeat(1023);
    impossible.boot_error = Some("prefix leaves no room");
    cases.push(impossible);

    let mut results = Vec::new();
    for case in cases {
        let mut command = Command::new(&ctl);
        // Keep the operator's real credentials and OXIMG_* tuning out of tests.
        for (name, _) in std::env::vars_os() {
            if name
                .to_str()
                .is_some_and(|n| n.starts_with("OXIMG_") || n.starts_with("AWS_"))
            {
                command.env_remove(name);
            }
        }
        command.arg("--bin").arg(&bin);
        for (name, value) in s3_env(server.port()) {
            command.arg("--env").arg(format!("{name}={value}"));
        }
        let prefix = if case.prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", case.prefix)
        };
        command.args([
            "--env",
            &format!("OXIMG_SOURCE_BASE_URL=s3://test-bucket/{prefix}"),
            "--env",
            "OXIMG_UPSTREAM_TIMEOUT=1",
            "--env",
            "OXIMG_FETCH_CONCURRENCY=1",
        ]);
        for (name, value) in &case.extra {
            command.arg("--env").arg(format!("{name}={value}"));
        }
        command.args([
            "get",
            &format!("/resize/100/100/{}", case.key),
            "--expect",
            &case.status.to_string(),
            "--timeout-secs",
            "5",
        ]);
        let before = server.requests().len();
        let output = command.output().context("run oximg-ctl")?;
        let observed: Value =
            serde_json::from_slice(&output.stdout).context("oximg-ctl did not return JSON")?;
        let requests = server.requests()[before..].to_vec();
        let target = format!("/test-bucket/{prefix}{}", case.wire_key);
        let shape = case.status != 200
            || (observed["probe"]["width"] == 100
                && observed["probe"]["height"] == 75
                && observed["content_type"] == "image/jpeg");
        let ok = if let Some(reason) = case.boot_error {
            !output.status.success()
                && observed["ok"] == false
                && observed["stderr"]
                    .as_str()
                    .is_some_and(|s| s.contains(reason))
                && requests.len() == case.attempts
        } else {
            output.status.success()
                && observed["ok"] == true
                && observed["status"] == case.status
                && shape
                && requests.len() == 1 + case.attempts
                && requests.iter().skip(1).all(|path| path == &target)
        };
        results.push(json!({"case": case.name, "ok": ok, "expected_status": if case.boot_error.is_some() { None } else { Some(case.status) }, "expected_boot_error": case.boot_error, "expected_attempts": case.attempts,
            "observed": observed, "requests": requests, "stderr": String::from_utf8_lossy(&output.stderr)}));
    }
    let passed = results.iter().filter(|case| case["ok"] == true).count();
    Ok(
        json!({"ok": passed == results.len(), "passed": passed, "total": results.len(), "cases": results}),
    )
}
