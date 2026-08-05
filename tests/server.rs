//! End-to-end HTTP tests: spawn the real server binary and exercise the
//! full request path, including content types, error mapping, request
//! coalescing, URL signing, and the remote-source mode.

mod common;

use std::io::Read;
use std::process::{Child, Command};

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    /// Spawns the binary on an OS-assigned port (PORT=0) and discovers
    /// it from the "listening on" stderr line — hardcoded ports sat in
    /// the ephemeral range, where a parallel test's outbound client
    /// connection could occupy them as a source port at exactly the
    /// wrong moment (observed as CI-only bind failures).
    fn start(envs: &[(&str, String)]) -> Server {
        let fixtures = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximg"));
        cmd.env("PORT", "0")
            .env("IMAGES_DIR", fixtures)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn oximg");
        let stderr = child.stderr.take().expect("stderr piped");
        let mut reader = std::io::BufReader::new(stderr);
        let mut port = None;
        let mut line = String::new();
        use std::io::BufRead;
        // The listening line is the first thing a healthy server prints.
        for _ in 0..100 {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // stderr closed: the process is exiting
                Ok(_) => {
                    if let Some(rest) = line.strip_prefix("oximg listening on :") {
                        port = rest.split_whitespace().next().and_then(|p| p.parse().ok());
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // Keep draining stderr (request logs) so the pipe never fills
        // and blocks the server.
        std::thread::spawn(move || {
            let mut sink = std::io::sink();
            let _ = std::io::copy(&mut reader.into_inner(), &mut sink);
        });
        let Some(port) = port else {
            let status = child.wait().ok();
            panic!("server exited before becoming healthy: {status:?}");
        };
        let mut server = Server { child, port };
        // Generous deadline: loaded CI runners can take seconds to page in
        // a release binary alongside the parallel test processes.
        for _ in 0..400 {
            if server.get("/health").is_ok() {
                return server;
            }
            if let Ok(Some(status)) = server.child.try_wait() {
                panic!("server exited before becoming healthy: {status}");
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        panic!("server did not become healthy");
    }

    fn get(&self, path: &str) -> Result<(u16, String, Vec<u8>), ureq::Error> {
        let mut resp = ureq::get(format!("http://127.0.0.1:{}{}", self.port, path)).call()?;
        let status = resp.status().as_u16();
        let ct = resp
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap_or("").to_string())
            .unwrap_or_default();
        let mut body = Vec::new();
        resp.body_mut()
            .as_reader()
            .read_to_end(&mut body)
            .unwrap_or(0);
        Ok((status, ct, body))
    }

    /// Send an arbitrary method, returning (status, Allow header).
    /// ureq errors on non-2xx, so both arms are unwrapped by hand.
    fn method_of(&self, method: &str, path: &str) -> (u16, Option<String>) {
        let url = format!("http://127.0.0.1:{}{}", self.port, path);
        let req = match method {
            "OPTIONS" => ureq::options(&url),
            "POST" => return (self.post_status(&url), None),
            other => panic!("unsupported test method {other}"),
        };
        match req
            .header("Origin", "https://example.com")
            .header("Access-Control-Request-Method", "GET")
            .call()
        {
            Ok(resp) => {
                let allow = resp
                    .headers()
                    .get("allow")
                    .map(|v| v.to_str().unwrap_or("").to_string());
                (resp.status().as_u16(), allow)
            }
            Err(ureq::Error::StatusCode(s)) => (s, None),
            Err(e) => panic!("transport error: {e}"),
        }
    }

    fn post_status(&self, url: &str) -> u16 {
        match ureq::post(url).send_empty() {
            Ok(r) => r.status().as_u16(),
            Err(ureq::Error::StatusCode(s)) => s,
            Err(e) => panic!("transport error: {e}"),
        }
    }

    /// Status even for error responses (ureq errors on non-2xx by default).
    fn status_of(&self, path: &str) -> u16 {
        match self.get(path) {
            Ok((s, _, _)) => s,
            Err(ureq::Error::StatusCode(s)) => s,
            Err(e) => panic!("transport error: {e}"),
        }
    }

    /// Like `get`, with an optional Accept request header; also returns
    /// the response's Vary header.
    fn get_accept(
        &self,
        path: &str,
        accept: Option<&str>,
    ) -> Result<(u16, String, Option<String>, Vec<u8>), ureq::Error> {
        let mut req = ureq::get(format!("http://127.0.0.1:{}{}", self.port, path));
        if let Some(a) = accept {
            req = req.header("Accept", a);
        }
        let mut resp = req.call()?;
        let status = resp.status().as_u16();
        let hdr = |name: &str| {
            resp.headers()
                .get(name)
                .map(|v| v.to_str().unwrap_or("").to_string())
        };
        let ct = hdr("content-type").unwrap_or_default();
        let vary = hdr("vary");
        let mut body = Vec::new();
        resp.body_mut()
            .as_reader()
            .read_to_end(&mut body)
            .unwrap_or(0);
        Ok((status, ct, vary, body))
    }
}

impl Server {
    /// Deliver a signal by name ("TERM", "INT") via /bin/kill — takes
    /// &self so a test can signal while other threads hold requests
    /// open against the server.
    #[cfg(unix)]
    fn signal(&self, sig: &str) {
        let status = Command::new("kill")
            .arg(format!("-{sig}"))
            .arg(self.child.id().to_string())
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -{sig} failed");
    }

    /// Wait for the process to exit on its own (no kill), panicking
    /// past the deadline — a graceful shutdown that hangs must fail
    /// the test, not stall the suite.
    fn wait_exit(&mut self, timeout: std::time::Duration) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            if std::time::Instant::now() > deadline {
                panic!("server did not exit within {timeout:?}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn serves_each_format_with_matching_content_type() {
    let s = Server::start(&[]);
    for (file, ct) in [
        ("photo.jpg", "image/jpeg"),
        ("rgb.png", "image/png"),
        ("photo.webp", "image/webp"),
    ] {
        let (status, got_ct, body) = s.get(&format!("/resize/100/100/{file}")).unwrap();
        assert_eq!(status, 200, "{file}");
        assert_eq!(got_ct, ct, "{file}");
        assert!(!body.is_empty());
        let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
        assert_eq!((w, h), (100, 75), "{file}");
    }
}

#[cfg(feature = "avif")]
#[test]
fn serves_avif_with_matching_content_type() {
    let s = Server::start(&[]);
    let (status, ct, body) = s.get("/resize/100/100/photo.avif").unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/avif");
    let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!((w, h), (100, 75));
}

#[test]
fn error_mapping() {
    let s = Server::start(&[]);
    assert_eq!(s.status_of("/resize/0/0/photo.jpg"), 400);
    assert_eq!(s.status_of("/resize/9000/9000/photo.jpg"), 400);
    assert_eq!(s.status_of("/resize/100/100/missing.jpg"), 404);
    assert_eq!(s.status_of("/resize/100/100/..%2Fsecret"), 400);
    assert_eq!(s.status_of("/resize/100/100/photo.jpg%3Fx=1"), 400);
    assert_eq!(s.status_of("/resize/100/100/photo.jpg%23frag"), 400);
}

#[test]
fn concurrent_identical_requests_coalesce_to_identical_bytes() {
    let s = Server::start(&[]);
    let results: Vec<Vec<u8>> = std::thread::scope(|sc| {
        (0..12)
            .map(|_| sc.spawn(|| s.get("/resize/120/120/photo.jpg").unwrap().2))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect()
    });
    for r in &results[1..] {
        assert_eq!(r, &results[0], "coalesced responses must be identical");
    }
}

/// --version prints the crate version and exits 0 without binding a
/// port (the flag a bug report is asked to cite).
#[test]
fn version_flag_prints_and_exits() {
    let out = Command::new(env!("CARGO_BIN_EXE_oximg"))
        .arg("--version")
        .output()
        .expect("run oximg --version");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim(),
        format!("oximg {}", env!("CARGO_PKG_VERSION"))
    );
    // An unknown flag is a usage error, not a boot.
    let bad = Command::new(env!("CARGO_BIN_EXE_oximg"))
        .arg("--nonsense")
        .output()
        .unwrap();
    assert!(!bad.status.success());
}

/// CMYK/YCCK JPEG sources are served end to end (200 with real
/// pixels), where the mozjpeg unwinding panic used to produce a 500.
#[test]
fn cmyk_source_is_served_not_a_500() {
    let s = Server::start(&[]);
    let (status, ct, body) = s.get("/resize/32/32/cmyk_ycck.jpg").unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/jpeg");
    let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!((w, h), (32, 24));
}

/// OXIMG_ICC=0 skips profile extraction entirely, which for a CMYK
/// source means the naive conversion instead of the color-managed
/// one — visibly different pixels, not just missing metadata.
#[test]
fn oximg_icc_zero_downgrades_cmyk_to_naive() {
    let managed = Server::start(&[]);
    let naive = Server::start(&[("OXIMG_ICC", "0".into())]);
    let a = managed.get("/resize/64/64/cmyk_icc.jpg").unwrap().2;
    let b = naive.get("/resize/64/64/cmyk_icc.jpg").unwrap().2;
    let (pa, w, h) = oximg::pipeline::decode_and_resize(&a, 64, 64, 1).unwrap();
    let (pb, ..) = oximg::pipeline::decode_and_resize(&b, 64, 64, 1).unwrap();
    assert_eq!((w, h), (64, 48));
    let worst = pa
        .iter()
        .zip(&pb)
        .map(|(x, y)| (*x as i32 - *y as i32).abs())
        .max()
        .unwrap();
    assert!(worst >= 30, "renderings barely differ (max delta {worst})");
    // Exact pin, not just divergence: cmyk_icc.jpg is an APP2 splice
    // of cmyk_ycck.jpg (identical entropy data, identical decoded
    // pixels), so with the profile ignored the two URLs must produce
    // byte-identical responses.
    let twin = naive.get("/resize/64/64/cmyk_ycck.jpg").unwrap().2;
    assert_eq!(
        b, twin,
        "ICC=0 must render exactly like the profile-less twin"
    );
}

/// A local source that exists but cannot be read (here: a directory
/// where a file was expected) is a 500, not a 422 blaming the client.
#[test]
fn unreadable_local_source_is_server_error() {
    let dir = std::env::temp_dir().join(format!("oximg-eisdir-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("isdir.jpg")).unwrap();
    let s = Server::start(&[("IMAGES_DIR", dir.to_str().unwrap().to_string())]);
    assert_eq!(s.status_of("/resize/100/100/isdir.jpg"), 500);
}

/// The decoded-pixel cap rejects decompression bombs before any
/// pixel-sized allocation, across formats — as 413 (the source is too
/// large), the same class as the byte cap, not 422 blaming the bytes.
#[test]
fn src_pixel_cap_rejects_before_allocation() {
    // photo.jpg is 200x150 = 30000 px; tiny.jpg is 40x30 = 1200.
    let s = Server::start(&[("OXIMG_MAX_SRC_PIXELS", "10000".into())]);
    assert_eq!(s.status_of("/resize/100/100/tiny.jpg"), 200);
    for name in ["photo.jpg", "rgb.png", "photo.webp"] {
        assert_eq!(
            s.status_of(&format!("/resize/100/100/{name}")),
            413,
            "{name} must be rejected by the pixel cap"
        );
    }
    // AVIF only reaches its header parse (and thus the cap) with the
    // feature compiled in; without it the source is rejected earlier
    // as unsupported input.
    let want = if cfg!(feature = "avif") { 413 } else { 422 };
    assert_eq!(s.status_of("/resize/100/100/photo.avif"), want);
}

/// Set-but-invalid runtime knobs refuse to boot: a typo in a limit
/// must not silently fail open to the default.
#[test]
fn invalid_knobs_refuse_to_boot() {
    for (k, v) in [
        ("OXIMG_MAX_SOURCE_BYTES", "512k"),
        ("OXIMG_AUTO_ROTATE", "false"),
        ("OXIMG_WEBP_QUALITY", "150"),
        ("OXIMG_OVERLAP", "yes"),
        ("OXIMG_PNG_QUANTIZE", "yes"),
        ("OXIMG_UPSTREAM_TIMEOUT", "0"),
        ("OXIMG_METRICS", "yes"),
        ("OXIMG_WORKERS", "0"),
        ("OXIMG_WORKERS", "600"),
        ("OXIMG_WORKERS", "two"),
        ("OXIMG_UPSTREAM_CONNECT_TIMEOUT", "fast"),
        ("OXIMG_FETCH_CONCURRENCY", "0"),
        ("OXIMG_FETCH_CONCURRENCY", "2000"),
        ("OXIMG_FETCH_CONCURRENCY", "many"),
        ("OXIMG_PNG_QUANTIZE_COLORS", "300"),
        ("OXIMG_PNG_QUANTIZE_COLORS", "1"),
        ("QUALITY", "eighty"),
    ] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximg"));
        cmd.env("PORT", "0")
            .env(k, v)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().expect("spawn oximg");
        let mut status = None;
        for _ in 0..200 {
            if let Ok(Some(s)) = child.try_wait() {
                status = Some(s);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let Some(status) = status else {
            let _ = child.kill();
            panic!("server booted despite {k}={v}");
        };
        assert!(!status.success(), "{k}={v} must exit non-zero");
    }
}

/// A set-but-undecodable signing key must refuse to boot — never
/// serve unsigned.
#[test]
fn invalid_signing_config_refuses_to_boot() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximg"));
    cmd.env("PORT", "0")
        .env("OXIMG_KEY", "not-hex-at-all")
        .env("OXIMG_SALT", "cafebabe")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().expect("spawn oximg");
    let mut status = None;
    for _ in 0..200 {
        if let Ok(Some(s)) = child.try_wait() {
            status = Some(s);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let Some(status) = status else {
        let _ = child.kill();
        panic!("server kept running with an undecodable OXIMG_KEY");
    };
    assert!(!status.success(), "exit must be non-zero, got {status}");
}

#[test]
fn signing_gate() {
    let key = "deadbeef".repeat(8);
    let salt = "cafebabe".repeat(8);
    let s = Server::start(&[("OXIMG_KEY", key), ("OXIMG_SALT", salt)]);
    // unsigned path is rejected while signing is enabled
    assert_eq!(s.status_of("/resize/100/100/photo.jpg"), 403);
    assert_eq!(s.status_of("/AAAA/resize/100/100/photo.jpg"), 403);
    // valid signature (precomputed with python for this key/salt/path)
    let sig = "t-jKRoyvzhs4dEBnGGBUS_t6Uh_HE6WysfGYvs8UaTo";
    let (status, ct, _) = s.get(&format!("/{sig}/resize/100/100/photo.jpg")).unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/jpeg");
    // same signature must not authorize a different path
    assert_eq!(
        s.status_of(&format!("/{sig}/resize/101/100/photo.jpg")),
        403
    );
}

#[test]
fn explicit_format_token_transcodes() {
    let s = Server::start(&[]);
    let (status, ct, body) = s.get("/resize/100/100/photo.jpg@webp").unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/webp");
    let (fmt, w, h) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!(fmt, oximg::pipeline::ImageFormat::Webp);
    assert_eq!((w, h), (100, 75));
    // An explicit token naming the source format is the same-format path.
    let plain = s.get("/resize/100/100/photo.jpg").unwrap().2;
    let explicit = s.get("/resize/100/100/photo.jpg@jpeg").unwrap().2;
    assert_eq!(plain, explicit, "@jpeg must match the bare URL's bytes");
}

#[test]
fn format_token_error_mapping() {
    let s = Server::start(&[]);
    // Unknown suffix falls through as a filename -> 404, not 400.
    assert_eq!(s.status_of("/resize/100/100/photo.jpg@bogus"), 404);
    // Reserved for a future encoder: clear 400 instead of a silent 404.
    assert_eq!(s.status_of("/resize/100/100/photo.jpg@jxl"), 400);
    #[cfg(not(feature = "avif"))]
    assert_eq!(s.status_of("/resize/100/100/photo.jpg@avif"), 400);
}

#[test]
fn signed_urls_cover_the_format_token() {
    let key = "deadbeef".repeat(8);
    let salt = "cafebabe".repeat(8);
    let s = Server::start(&[("OXIMG_KEY", key), ("OXIMG_SALT", salt)]);
    // Precomputed with python hmac for this key/salt over
    // "/resize/100/100/photo.jpg@webp" (same method as signing_gate's
    // vector, which pins the scheme).
    let sig = "XQ8C3eYRVAkFAnUczGBsuXMOu-J6vMoYi3W8_4-sT6Q";
    let (status, ct, _) = s
        .get(&format!("/{sig}/resize/100/100/photo.jpg@webp"))
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/webp");
    // The bare-path signature must not authorize a different target
    // format (that would let one signature buy heavier encodes).
    let plain_sig = "t-jKRoyvzhs4dEBnGGBUS_t6Uh_HE6WysfGYvs8UaTo";
    assert_eq!(
        s.status_of(&format!("/{plain_sig}/resize/100/100/photo.jpg@webp")),
        403
    );
}

#[test]
fn accept_negotiation_and_vary() {
    // Negotiation off (default): no Vary header, format follows source.
    let s = Server::start(&[]);
    let (_, ct, vary, _) = s
        .get_accept("/resize/100/100/photo.jpg", Some("image/webp,*/*"))
        .unwrap();
    assert_eq!(ct, "image/jpeg", "negotiation must be opt-in");
    assert_eq!(vary, None, "no Vary when negotiation is off");

    // Negotiation on: Accept steers the format; Vary is emitted on
    // every response (config-static), including non-negotiated ones.
    let s = Server::start(&[("OXIMG_AUTO_FORMAT", "webp".into())]);
    let (_, ct, vary, body) = s
        .get_accept("/resize/100/100/photo.jpg", Some("image/webp,*/*"))
        .unwrap();
    assert_eq!(ct, "image/webp");
    assert_eq!(vary.as_deref(), Some("Accept"));
    let (fmt, _, _) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!(fmt, oximg::pipeline::ImageFormat::Webp);
    let (_, ct, vary, _) = s.get_accept("/resize/100/100/photo.jpg", None).unwrap();
    assert_eq!(ct, "image/jpeg", "no Accept -> source format");
    assert_eq!(
        vary.as_deref(),
        Some("Accept"),
        "Vary must be config-static"
    );
    // Explicit token beats negotiation.
    let (_, ct, _, _) = s
        .get_accept("/resize/100/100/photo.jpg@png", Some("image/webp,*/*"))
        .unwrap();
    assert_eq!(ct, "image/png");
}

#[test]
fn mixed_format_requests_do_not_cross_coalesce() {
    let s = Server::start(&[]);
    let (jpegs, webps): (Vec<_>, Vec<_>) = std::thread::scope(|sc| {
        let j: Vec<_> = (0..6)
            .map(|_| sc.spawn(|| s.get("/resize/120/120/photo.jpg").unwrap()))
            .collect();
        let w: Vec<_> = (0..6)
            .map(|_| sc.spawn(|| s.get("/resize/120/120/photo.jpg@webp").unwrap()))
            .collect();
        (
            j.into_iter().map(|h| h.join().unwrap()).collect(),
            w.into_iter().map(|h| h.join().unwrap()).collect(),
        )
    });
    for (_, ct, body) in &jpegs {
        assert_eq!(ct, "image/jpeg");
        assert_eq!(body, &jpegs[0].2);
    }
    for (_, ct, body) in &webps {
        assert_eq!(ct, "image/webp");
        assert_eq!(body, &webps[0].2);
        assert!(body.starts_with(b"RIFF"), "must be WebP bytes");
    }
}

/// Forcing the fused JPEG path on must not let it capture cross-format
/// requests (the jpegli fused worker is same-format only), and the
/// cross-format fused-pixels worker it takes instead must produce the
/// same bytes as the serial path — one URL, one output, regardless of
/// the overlap gate.
#[test]
fn forced_overlap_cross_format_matches_serial() {
    let fused = Server::start(&[("OXIMG_OVERLAP", "1".into())]);
    let serial = Server::start(&[("OXIMG_OVERLAP", "0".into())]);
    let mut urls = vec![
        "/resize/100/100/photo.jpg@webp",
        "/resize/100/100/photo.jpg@png",
    ];
    if cfg!(feature = "avif") {
        // The fused AVIF path converts YUV during the decode overlap;
        // bytes must still match the serial full-frame conversion.
        urls.push("/resize/100/100/photo.jpg@avif");
    }
    for url in urls {
        let (status, ct, body) = fused.get(url).unwrap();
        assert_eq!(status, 200, "{url}");
        let (s2, ct2, body2) = serial.get(url).unwrap();
        assert_eq!(s2, 200, "{url}");
        assert_eq!(ct, ct2, "{url}");
        assert_eq!(body, body2, "{url}: fused and serial bytes must match");
    }
    let (_, ct, body) = fused.get("/resize/100/100/photo.jpg@webp").unwrap();
    assert_eq!(ct, "image/webp");
    assert!(body.starts_with(b"RIFF"), "fused gate leaked jpegli bytes");
    assert_eq!(&body[8..12], b"WEBP");
}

/// Write orientation-6 (90°-rotated) sources of every rotatable
/// format into a fresh directory usable as IMAGES_DIR.
fn oriented_images_dir(tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!("oximg-orient-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let display = common::corner_base(240, 180, 60);
    let (stored, sw, sh) = common::store_for_orientation(&display, 240, 180, 6);
    let jpeg = common::jpeg_with_orientation(&stored, sw, sh, Some(6));
    std::fs::write(dir.join("rotated.jpg"), jpeg).unwrap();
    std::fs::write(
        dir.join("rotated.png"),
        common::png_with_orientation(&stored, sw, sh, 6),
    )
    .unwrap();
    dir.to_str().unwrap().to_string()
}

/// Auto-rotation is on by default (dimensions come out display-fit)
/// and OXIMG_AUTO_ROTATE=0 restores the stored-orientation behavior.
#[test]
fn auto_rotate_default_and_kill_switch() {
    let dir = oriented_images_dir("kill");
    let on = Server::start(&[("IMAGES_DIR", dir.clone())]);
    let (status, _, body) = on.get("/resize/120/120/rotated.jpg").unwrap();
    assert_eq!(status, 200);
    let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
    // Stored portrait 180x240 displays as landscape 240x180.
    assert_eq!((w, h), (120, 90), "default: display-oriented fit");

    let (_, _, body) = on.get("/resize/120/120/rotated.png").unwrap();
    let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!((w, h), (120, 90), "png default: display-oriented fit");
    #[cfg(feature = "avif")]
    {
        // orient_irot1.avif (fixtures dir is also served) stores
        // 240x180 landscape displaying portrait.
        let fx = Server::start(&[]);
        let (_, _, body) = fx.get("/resize/120/120/orient_irot1.avif@jpg").unwrap();
        let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
        assert_eq!((w, h), (90, 120), "avif default: irot applied");
    }
    drop(on);

    let off = Server::start(&[
        ("IMAGES_DIR", dir),
        ("OXIMG_AUTO_ROTATE", "0".into()),
        ("OXIMG_ICC", "0".into()),
    ]);
    for name in ["rotated.jpg", "rotated.png"] {
        let (_, _, body) = off.get(&format!("/resize/120/120/{name}")).unwrap();
        let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
        assert_eq!((w, h), (90, 120), "{name} kill switch: stored orientation");
    }
    drop(off);
    #[cfg(feature = "avif")]
    {
        let off = Server::start(&[("OXIMG_AUTO_ROTATE", "0".into())]);
        let (_, _, body) = off.get("/resize/120/120/orient_irot1.avif@jpg").unwrap();
        let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
        assert_eq!((w, h), (120, 90), "avif kill switch: stored orientation");
    }
}

/// Oriented sources force the pixel fuse; their bytes must still be
/// independent of the overlap gate.
#[test]
fn oriented_bytes_do_not_depend_on_overlap_gate() {
    let dir = oriented_images_dir("gate");
    let fused = Server::start(&[("IMAGES_DIR", dir.clone()), ("OXIMG_OVERLAP", "1".into())]);
    let serial = Server::start(&[("IMAGES_DIR", dir), ("OXIMG_OVERLAP", "0".into())]);
    let a = fused.get("/resize/120/120/rotated.jpg").unwrap().2;
    let b = serial.get("/resize/120/120/rotated.jpg").unwrap().2;
    assert_eq!(a, b, "oriented fused and serial bytes must match");
    #[cfg(feature = "avif")]
    {
        let a = fused.get("/resize/120/120/rotated.jpg@avif").unwrap().2;
        let b = serial.get("/resize/120/120/rotated.jpg@avif").unwrap().2;
        assert_eq!(a, b, "preheated-session and serial AVIF bytes must match");
    }
}

/// ICC pass-through is on by default and OXIMG_ICC=0 strips it; the
/// profiled source serves fine either way, and profiled bytes stay
/// independent of the overlap gate (non-AVIF targets take the pixel
/// fuse; AVIF targets splice the profile after the encode).
#[test]
fn icc_default_kill_switch_and_gate_independence() {
    let dir = std::env::temp_dir().join(format!("oximg-icc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let icc = common::fake_icc(700);
    let px = common::corner_base(240, 180, 60);
    let app2 = common::app2_icc_payloads(&icc, 60_000).remove(0);
    let jpeg = common::jpeg_with_markers(&px, 240, 180, &[(2, &app2)]);
    std::fs::write(dir.join("profiled.jpg"), jpeg).unwrap();
    let dir = dir.to_str().unwrap().to_string();

    let on = Server::start(&[("IMAGES_DIR", dir.clone()), ("OXIMG_OVERLAP", "1".into())]);
    let (status, _, body) = on.get("/resize/120/120/profiled.jpg").unwrap();
    assert_eq!(status, 200);
    assert_eq!(
        common::jpeg_icc(&body).as_deref(),
        Some(&icc[..]),
        "default: profile passes through"
    );
    let fused_bytes = body;
    drop(on);

    let serial = Server::start(&[("IMAGES_DIR", dir.clone()), ("OXIMG_OVERLAP", "0".into())]);
    let (_, _, body) = serial.get("/resize/120/120/profiled.jpg").unwrap();
    assert_eq!(body, fused_bytes, "profiled bytes are gate-independent");
    drop(serial);

    let off = Server::start(&[("IMAGES_DIR", dir.clone()), ("OXIMG_ICC", "0".into())]);
    let (status, _, body) = off.get("/resize/120/120/profiled.jpg").unwrap();
    assert_eq!(status, 200);
    assert_eq!(common::jpeg_icc(&body), None, "kill switch: no profile");
    drop(off);

    // AVIF sources honor the same gate (their extraction runs through
    // a separate code path in process_avif).
    #[cfg(feature = "avif")]
    {
        let fx = common::fake_icc(900); // the icc.avif fixture's blob
        let on = Server::start(&[]);
        let (_, _, body) = on.get("/resize/100/100/icc.avif@jpg").unwrap();
        assert_eq!(
            common::jpeg_icc(&body).as_deref(),
            Some(&fx[..]),
            "avif source: profile passes through by default"
        );
        drop(on);
        let off = Server::start(&[("OXIMG_ICC", "0".into())]);
        let (_, _, body) = off.get("/resize/100/100/icc.avif@jpg").unwrap();
        assert_eq!(
            common::jpeg_icc(&body),
            None,
            "avif source: kill switch strips it"
        );
        drop(off);
    }

    // The knobs are independent: rotation off, profile still carried.
    let display = common::corner_base(240, 180, 60);
    let (stored, sw, sh) = common::store_for_orientation(&display, 240, 180, 6);
    let app1 = common::app1_orientation(6);
    let app2 = common::app2_icc_payloads(&icc, 60_000).remove(0);
    let both = common::jpeg_with_markers(&stored, sw, sh, &[(1, &app1), (2, &app2)]);
    std::fs::write(std::path::Path::new(&dir).join("both.jpg"), both).unwrap();
    let no_rot = Server::start(&[("IMAGES_DIR", dir), ("OXIMG_AUTO_ROTATE", "0".into())]);
    let (_, _, body) = no_rot.get("/resize/120/120/both.jpg").unwrap();
    let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!((w, h), (90, 120), "rotation off: stored orientation");
    assert_eq!(
        common::jpeg_icc(&body).as_deref(),
        Some(&icc[..]),
        "rotation off: profile still passes through"
    );
}

/// mozjpeg presets fuse the decode with the resize on a second thread;
/// like every fused path, their bytes must not depend on the overlap
/// gate.
#[test]
fn preset_bytes_do_not_depend_on_overlap_gate() {
    for preset in ["fast", "small"] {
        let fused = Server::start(&[("OXIMG_OVERLAP", "1".into()), ("PRESET", preset.into())]);
        let serial = Server::start(&[("OXIMG_OVERLAP", "0".into()), ("PRESET", preset.into())]);
        let a = fused.get("/resize/100/100/photo.jpg").unwrap().2;
        let b = serial.get("/resize/100/100/photo.jpg").unwrap().2;
        assert_eq!(a, b, "PRESET={preset}: fused and serial bytes must match");
        assert!(a.starts_with(&[0xFF, 0xD8]), "PRESET={preset}: not a JPEG");
    }
}

/// The fir escape hatch swaps in a byte-different resize backend, so it
/// must also switch fusing off — otherwise the same URL's bytes would
/// depend on the instantaneous overlap gate. PNG output keeps the
/// comparison deterministic.
#[test]
fn fir_backend_disables_fusing_for_stable_bytes() {
    let fir = ("OXIMG_RESIZE_BACKEND", "fir".to_string());
    let fused = Server::start(&[("OXIMG_OVERLAP", "1".into()), fir.clone()]);
    let serial = Server::start(&[("OXIMG_OVERLAP", "0".into()), fir]);
    for url in ["/resize/100/100/photo.jpg@png", "/resize/100/100/photo.jpg"] {
        let a = fused.get(url).unwrap().2;
        let b = serial.get(url).unwrap().2;
        assert_eq!(a, b, "{url}: bytes must not depend on the overlap gate");
    }
}

/// Failure statuses are honest: an origin failure is 502 (the client's
/// request was fine), an origin 404 passes through as 404, and
/// undecodable input stays 422.
#[test]
fn error_statuses_are_honest() {
    // Origin that 500s on "boom*", 404s on missing, serves otherwise.
    let fixtures = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let fixtures = fixtures.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .trim_start_matches('/');
                use std::io::Write;
                if path.starts_with("boom") {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    return;
                }
                if path.starts_with("moved") {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 301 Moved Permanently\r\nLocation: http://127.0.0.1:1/pwned\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    return;
                }
                if path.starts_with("truncated") {
                    // Promise a large PNG, deliver a valid header, then
                    // drop the connection mid-body: the buffered reader
                    // hits UnexpectedEof.
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR");
                    return;
                }
                match std::fs::read(format!("{fixtures}/{path}")) {
                    Ok(data) => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            data.len()
                        );
                        let _ = stream.write_all(&data);
                    }
                    Err(_) => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                    }
                }
            });
        }
    });

    let s = Server::start(&[(
        "OXIMG_SOURCE_BASE_URL",
        format!("http://127.0.0.1:{origin_port}"),
    )]);
    assert_eq!(
        s.status_of("/resize/100/100/boom.jpg"),
        502,
        "origin 5xx is the upstream's fault"
    );
    assert_eq!(
        s.status_of("/resize/100/100/moved.jpg"),
        502,
        "origin redirects are refused, not followed"
    );
    assert_eq!(
        s.status_of("/resize/100/100/truncated.png"),
        502,
        "an origin body dying mid-stream is the upstream's fault"
    );
    assert_eq!(
        s.status_of("/resize/100/100/missing.jpg"),
        404,
        "origin 404 passes through"
    );
    drop(s);

    // An over-cap source is 413, not a misleading decode error.
    let s = Server::start(&[
        (
            "OXIMG_SOURCE_BASE_URL",
            format!("http://127.0.0.1:{origin_port}"),
        ),
        ("OXIMG_MAX_SOURCE_BYTES", "1000".into()),
    ]);
    assert_eq!(
        s.status_of("/resize/100/100/photo.jpg"),
        413,
        "over-cap remote source"
    );
    // Text served as an image is undecodable client input: 422 with a
    // message (LICENSE is a fixture-relative text file? use README).
    assert_eq!(s.status_of("/resize/100/100/list.txt"), 422);
}

/// The existing coalescing test only proves identical bytes — it
/// passes even if singleflight is completely broken. This one proves
/// the flight actually coalesces: a slow origin counts its fetches,
/// and N concurrent identical requests must produce exactly one.
/// Error results are shared the same way (one upstream failure, N
/// identical 502s, still one fetch).
#[test]
fn singleflight_coalesces_to_one_origin_fetch() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let fixtures = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = listener.local_addr().unwrap().port();
    let fetches = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&fetches);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let fixtures = fixtures.clone();
            let counter = Arc::clone(&counter);
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .trim_start_matches('/');
                use std::io::Write;
                // Slow responses widen the window in which followers
                // must coalesce behind the in-flight leader.
                std::thread::sleep(std::time::Duration::from_millis(800));
                if path.starts_with("boom") {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    return;
                }
                counter.fetch_add(1, Ordering::SeqCst);
                match std::fs::read(format!("{fixtures}/{path}")) {
                    Ok(data) => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            data.len()
                        );
                        let _ = stream.write_all(&data);
                    }
                    Err(_) => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                    }
                }
            });
        }
    });

    let s = Server::start(&[(
        "OXIMG_SOURCE_BASE_URL",
        format!("http://127.0.0.1:{origin_port}"),
    )]);

    // 8 identical requests fired together: one origin fetch, identical
    // bytes for everyone.
    let results: Vec<Vec<u8>> = std::thread::scope(|sc| {
        (0..8)
            .map(|_| sc.spawn(|| s.get("/resize/120/120/photo.jpg").unwrap().2))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect()
    });
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        1,
        "concurrent identical requests must coalesce to one origin fetch"
    );
    for r in &results[1..] {
        assert_eq!(r, &results[0]);
    }

    // Same for a failing flight: one fetch, shared 502s.
    let statuses: Vec<u16> = std::thread::scope(|sc| {
        (0..8)
            .map(|_| sc.spawn(|| s.status_of("/resize/120/120/boom.jpg")))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect()
    });
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        2,
        "the failing flight must also fetch exactly once"
    );
    assert!(
        statuses.iter().all(|&st| st == 502),
        "shared 502s: {statuses:?}"
    );
}

#[test]
fn remote_source_mode_serves_from_http_origin() {
    // origin: a second oximg? No — a minimal static file server thread.
    let fixtures = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let fixtures = fixtures.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .trim_start_matches('/');
                use std::io::Write;
                match std::fs::read(format!("{fixtures}/{path}")) {
                    Ok(data) => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            data.len()
                        );
                        let _ = stream.write_all(&data);
                    }
                    Err(_) => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                    }
                }
            });
        }
    });

    let s = Server::start(&[(
        "OXIMG_SOURCE_BASE_URL",
        format!("http://127.0.0.1:{origin_port}"),
    )]);
    let (status, ct, body) = s.get("/resize/100/100/photo.webp").unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/webp");
    let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!((w, h), (100, 75));
    // The format token must be stripped before the origin fetch: the
    // origin only has photo.webp, so an unstripped URL would 404.
    let (status, ct, body) = s.get("/resize/100/100/photo.webp@jpeg").unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/jpeg");
    assert!(body.starts_with(&[0xFF, 0xD8]));
    // origin 404 passes through
    assert_eq!(s.status_of("/resize/100/100/nope.jpg"), 404);
}

/// An idle server must exit 0 promptly on SIGTERM (what docker stop,
/// Kubernetes, and Cloud Run send) and on SIGINT (terminal ctrl-C) —
/// not ride out the orchestrator's SIGKILL as exit 137.
#[cfg(unix)]
#[test]
fn idle_server_exits_cleanly_on_sigterm_and_sigint() {
    for sig in ["TERM", "INT"] {
        let mut s = Server::start(&[]);
        s.signal(sig);
        let status = s.wait_exit(std::time::Duration::from_secs(10));
        assert!(status.success(), "SIG{sig}: exited {status}");
    }
}

/// The full graceful-shutdown contract, made deterministic by an
/// origin that stalls mid-body until the test releases it: after
/// SIGTERM lands with a request in flight, (1) the listener closes —
/// new connections are refused, (2) the in-flight response still
/// completes as a valid 200, (3) the process then exits 0.
#[cfg(unix)]
#[test]
fn graceful_shutdown_drains_inflight_and_refuses_new_connections() {
    use std::io::Write;

    let jpeg = std::fs::read(format!(
        "{}/tests/fixtures/photo.jpg",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = listener.local_addr().unwrap().port();
    let (inflight_tx, inflight_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    // One-shot origin: send headers plus the first KB, then hold the
    // body open until released — the request is pinned in flight for
    // exactly as long as the test needs, with no sleeps to race.
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 2048];
        let _ = std::io::Read::read(&mut stream, &mut buf);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            jpeg.len()
        )
        .unwrap();
        stream.write_all(&jpeg[..1024]).unwrap();
        stream.flush().unwrap();
        inflight_tx.send(()).unwrap();
        let _ = release_rx.recv();
        let _ = stream.write_all(&jpeg[1024..]);
    });

    let mut s = Server::start(&[(
        "OXIMG_SOURCE_BASE_URL",
        format!("http://127.0.0.1:{origin_port}"),
    )]);
    let port = s.port;

    std::thread::scope(|sc| {
        let request = sc.spawn(|| s.get("/resize/100/100/photo.jpg"));
        inflight_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("request never reached the origin");

        s.signal("TERM");

        // (1) The accept loop must stop while the request drains. Poll:
        // signal delivery is asynchronous, but the listener has to be
        // closed well before the deadline.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let refused = std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                std::time::Duration::from_millis(250),
            )
            .is_err();
            if refused {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "listener still accepting after SIGTERM"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        // (2) The pinned request survives the shutdown and completes.
        release_tx.send(()).unwrap();
        let (status, ct, body) = request.join().unwrap().expect("in-flight request failed");
        assert_eq!(status, 200);
        assert_eq!(ct, "image/jpeg");
        let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
        assert_eq!((w, h), (100, 75));
    });

    // (3) With the last response delivered, the process exits cleanly.
    let status = s.wait_exit(std::time::Duration::from_secs(10));
    assert!(status.success(), "exited {status}");
}

/// A temp IMAGES_DIR with a nested tree (and symlinks, for the
/// containment tests): albums/2026/photo.jpg is a copy of the fixture.
fn nested_images_dir(tag: &str) -> std::path::PathBuf {
    let fixtures = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let dir = std::env::temp_dir().join(format!("oximg-nested-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("albums/2026")).unwrap();
    std::fs::copy(
        format!("{fixtures}/photo.jpg"),
        dir.join("albums/2026/photo.jpg"),
    )
    .unwrap();
    dir
}

/// Nested source paths resolve under IMAGES_DIR — the issue #1 happy
/// path — and the @fmt token still rides on the last segment.
#[test]
fn nested_paths_resolve_under_images_dir() {
    let dir = nested_images_dir("happy");
    let s = Server::start(&[("IMAGES_DIR", dir.to_str().unwrap().to_string())]);
    let (status, ct, body) = s.get("/resize/100/100/albums/2026/photo.jpg").unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/jpeg");
    let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!((w, h), (100, 75));

    let (status, ct, body) = s.get("/resize/100/100/albums/2026/photo.jpg@webp").unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/webp");
    let (fmt, _, _) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!(fmt, oximg::pipeline::ImageFormat::Webp);

    // An encoded slash decodes to the same nested path (the extractor
    // decodes before matching — %2F is addressing, not smuggling).
    let (status, _, enc_body) = s.get("/resize/100/100/albums%2F2026%2Fphoto.jpg").unwrap();
    assert_eq!(status, 200);
    assert!(!enc_body.is_empty());
}

/// Every escape the multi-segment capture must refuse, end to end.
/// Suspicious characters are percent-encoded so they survive the HTTP
/// client untouched and exercise the server's own decode+validate.
#[test]
fn nested_path_escapes_are_refused() {
    let dir = nested_images_dir("escapes");
    let s = Server::start(&[("IMAGES_DIR", dir.to_str().unwrap().to_string())]);
    for url in [
        // traversal components, encoded and mixed
        "/resize/100/100/albums/%2e%2e/%2e%2e/secret.jpg",
        "/resize/100/100/%2e%2e%2Fsecret.jpg",
        "/resize/100/100/albums/2026/%2e%2E/photo.jpg",
        // '.' component
        "/resize/100/100/%2e/photo.jpg",
        // absolute path and empty components
        "/resize/100/100/%2Fetc%2Fpasswd",
        "/resize/100/100/albums%2F%2F2026%2Fphoto.jpg",
        "/resize/100/100/albums%2F2026%2F",
        // rejected bytes
        "/resize/100/100/albums%2F2026%5Cphoto.jpg",
        "/resize/100/100/albums%2Fphoto.jpg%3Fx=1",
        "/resize/100/100/albums%2Fphoto.jpg%23frag",
        "/resize/100/100/albums%2Fphoto%00.jpg",
    ] {
        assert_eq!(s.status_of(url), 400, "{url}");
    }
}

/// Symlink containment: a link that stays inside IMAGES_DIR works; a
/// link that resolves outside it is refused as 404 — indistinguishable
/// from an absent file, even though the target exists and is readable.
#[cfg(unix)]
#[test]
fn symlinks_are_contained_to_images_dir() {
    let fixtures = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let dir = nested_images_dir("symlink");
    std::os::unix::fs::symlink("albums/2026/photo.jpg", dir.join("alias.jpg")).unwrap();
    std::os::unix::fs::symlink(format!("{fixtures}/photo.jpg"), dir.join("escape.jpg")).unwrap();
    let s = Server::start(&[("IMAGES_DIR", dir.to_str().unwrap().to_string())]);
    assert_eq!(
        s.status_of("/resize/100/100/alias.jpg"),
        200,
        "inside-root symlink must serve"
    );
    assert_eq!(
        s.status_of("/resize/100/100/escape.jpg"),
        404,
        "outside-root symlink must read as absent"
    );
}

/// Signing over nested paths: the canonical signed form is the decoded
/// multi-segment path, so one signature covers the encoded and decoded
/// spellings of the same source, @fmt is still covered, and a
/// signature never authorizes a different nested path.
#[test]
fn signed_urls_cover_nested_paths() {
    let dir = nested_images_dir("signed");
    let key = "deadbeef".repeat(8);
    let salt = "cafebabe".repeat(8);
    let s = Server::start(&[
        ("IMAGES_DIR", dir.to_str().unwrap().to_string()),
        ("OXIMG_KEY", key),
        ("OXIMG_SALT", salt),
    ]);
    // Precomputed with python hmac over the decoded path
    // "/resize/100/100/albums/2026/photo.jpg" (same key/salt/scheme as
    // signing_gate's vector).
    let sig = "i1gy8Dm1yo32_9FMzrRj8MDG_c0F0kJDV22jAgvUCow";
    let (status, ct, _) = s
        .get(&format!("/{sig}/resize/100/100/albums/2026/photo.jpg"))
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/jpeg");
    // The same signature verifies the %2F spelling: clients sign the
    // decoded form, whatever encoding the URL uses.
    assert_eq!(
        s.status_of(&format!("/{sig}/resize/100/100/albums%2F2026%2Fphoto.jpg")),
        200
    );
    // Not a different path...
    assert_eq!(
        s.status_of(&format!("/{sig}/resize/100/100/albums/2027/photo.jpg")),
        403
    );
    // ...and not a different format either.
    assert_eq!(
        s.status_of(&format!("/{sig}/resize/100/100/albums/2026/photo.jpg@webp")),
        403
    );
    // The @webp target has its own precomputed signature.
    let sig_webp = "1TIbduLRsnsAJc4TDVwcHSeKCe6IpdVwdzs_elu9fG8";
    let (status, ct, _) = s
        .get(&format!(
            "/{sig_webp}/resize/100/100/albums/2026/photo.jpg@webp"
        ))
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/webp");
}

/// The remote-source mode with nested paths: the origin must receive
/// exactly the segments the client addressed — no climbing above the
/// base prefix, no authority injection, and no double-decode (a
/// percent-encoded byte in the decoded name reaches the origin
/// re-encoded, never re-interpreted).
#[test]
fn base_url_mode_forwards_nested_paths_verbatim() {
    use std::sync::{Arc, Mutex};

    let fixtures = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorder = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let fixtures = fixtures.clone();
            let recorder = Arc::clone(&recorder);
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                // Raw request-target, exactly as sent on the wire.
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                recorder.lock().unwrap().push(path.clone());
                use std::io::Write;
                if path.ends_with(".jpg") && !path.contains("missing") {
                    let data = std::fs::read(format!("{fixtures}/photo.jpg")).unwrap();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        data.len()
                    );
                    let _ = stream.write_all(&data);
                } else {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                }
            });
        }
    });

    let s = Server::start(&[(
        "OXIMG_SOURCE_BASE_URL",
        format!("http://127.0.0.1:{origin_port}/prefix"),
    )]);

    // Nested path: forwarded verbatim under the base's own prefix.
    assert_eq!(
        s.get("/resize/100/100/albums/2026/photo.jpg").unwrap().0,
        200
    );
    // Double-encoded traversal: the extractor decodes %252e to %2e; the
    // origin must receive that byte sequence re-encoded (%252e again),
    // NOT a second decode's ".." — the wire-path assertion below is the
    // real check (this origin happily serves any literal .jpg name).
    assert_eq!(
        s.status_of("/resize/100/100/albums%2F%252e%252e%2Fx.jpg"),
        200
    );
    // Climbing and authority injection never reach the origin at all.
    assert_eq!(
        s.status_of("/resize/100/100/%2e%2e/%2e%2e/other-bucket/x.jpg"),
        400
    );
    assert_eq!(
        s.status_of("/resize/100/100/%2F%2Fevil.example%2Fx.jpg"),
        400
    );

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.as_slice(),
        [
            "/prefix/albums/2026/photo.jpg",
            "/prefix/albums/%252e%252e/x.jpg",
        ],
        "origin saw exactly the addressed segments, nothing else"
    );
}

/// Issue #2: 0 on one axis means unconstrained. Width-only and
/// height-only requests produce the aspect-following dimension, an
/// output taller than the old 8192 sentinel is possible (the silent
/// narrowing the sentinel caused is gone), and both-zero stays 400.
#[test]
fn zero_axis_is_unconstrained() {
    // photo.jpg is 200x150.
    let s = Server::start(&[]);
    let (status, _, body) = s.get("/resize/100/0/photo.jpg").unwrap();
    assert_eq!(status, 200);
    let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!((w, h), (100, 75), "width-only follows the aspect ratio");

    let (status, _, body) = s.get("/resize/0/75/photo.jpg").unwrap();
    assert_eq!(status, 200);
    let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!((w, h), (100, 75), "height-only follows the aspect ratio");

    assert_eq!(s.status_of("/resize/0/0/photo.jpg"), 400, "no box at all");
    assert_eq!(
        s.status_of("/resize/9000/0/photo.jpg"),
        400,
        "cap still applies"
    );
}

/// The regression that motivated #2: a tall source under a width-only
/// request must come out at the requested width, even when the height
/// lands beyond the old 8192 sentinel — the case where the workaround
/// silently produced a narrower image and corrupted srcset descriptors.
#[test]
fn width_only_serves_taller_than_the_old_sentinel() {
    let dir = std::env::temp_dir().join(format!("oximg-tall-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, 20, 18000);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    writer
        .write_image_data(&vec![64u8; 20 * 18000 * 3])
        .unwrap();
    writer.finish().unwrap();
    std::fs::write(dir.join("tall.png"), &out).unwrap();

    let s = Server::start(&[("IMAGES_DIR", dir.to_str().unwrap().to_string())]);
    let (status, _, body) = s.get("/resize/10/0/tall.png").unwrap();
    assert_eq!(status, 200);
    let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!(
        (w, h),
        (10, 9000),
        "requested width delivered; height exceeds the old sentinel"
    );
    // The best pre-#2 emulation of the same request, for contrast: the
    // sentinel constrains this source and the width comes out wrong.
    let (_, _, body) = s.get("/resize/10/8192/tall.png").unwrap();
    let (_, w, _) = oximg::pipeline::probe(&body).unwrap();
    assert!(w < 10, "the sentinel workaround narrows tall sources ({w})");
}

/// OXIMG_PNG_QUANTIZE steers the server end to end: PNG-in/PNG-out
/// shrinks, an explicit @png token from another source format is
/// governed by the same knob (encode settings are keyed by the output
/// format), and the colors knob steers further.
#[test]
fn png_quantize_knob_shrinks_png_responses() {
    let plain = Server::start(&[]);
    let quant = Server::start(&[("OXIMG_PNG_QUANTIZE", "1".into())]);
    let quant16 = Server::start(&[
        ("OXIMG_PNG_QUANTIZE", "1".into()),
        ("OXIMG_PNG_QUANTIZE_COLORS", "16".into()),
    ]);
    for url in ["/resize/100/100/rgb.png", "/resize/100/100/photo.jpg@png"] {
        let lossless = plain.get(url).unwrap().2;
        let quantized = quant.get(url).unwrap().2;
        let q16 = quant16.get(url).unwrap().2;
        assert!(
            quantized.len() < lossless.len(),
            "{url}: quantized ({}) must undercut lossless ({})",
            quantized.len(),
            lossless.len()
        );
        assert!(
            q16.len() < quantized.len(),
            "{url}: 16 colors ({}) must undercut 256 ({})",
            q16.len(),
            quantized.len()
        );
        // Every variant still probes as a PNG at the same dimensions.
        let (fmt, w, h) = oximg::pipeline::probe(&q16).unwrap();
        assert_eq!(fmt, oximg::pipeline::ImageFormat::Png, "{url}");
        assert_eq!((w, h), (100, 75), "{url}");
    }
    // Alpha sources are untouched by the knob: identical lossless bytes.
    let a = plain.get("/resize/100/100/rgba.png").unwrap().2;
    let b = quant.get("/resize/100/100/rgba.png").unwrap().2;
    assert_eq!(a, b, "alpha PNG must stay lossless under the knob");
}

/// Issue #4: a stalled origin is bounded by OXIMG_UPSTREAM_TIMEOUT and
/// answers 504 (distinct from other upstream failures' 502) — and,
/// the part that matters for capacity, the CPU permit comes back: a
/// normal request right after the timeout is served promptly instead
/// of queueing behind a zombie fetch.
#[test]
fn stalled_origin_times_out_as_504_and_releases_the_permit() {
    use std::io::Write;

    let fixtures = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let fixtures = fixtures.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .trim_start_matches('/');
                if path.starts_with("stall") {
                    // Accept, read the request, answer nothing: the
                    // classic hung origin. Hold the socket open well
                    // past the server's deadline.
                    std::thread::sleep(std::time::Duration::from_secs(30));
                    return;
                }
                if path.starts_with("drip") {
                    // Headers promptly, then a body that stalls
                    // mid-stream: the timeout must cover body reads,
                    // not just the response head.
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(b"\x89PNG\r\n\x1a\n");
                    std::thread::sleep(std::time::Duration::from_secs(30));
                    return;
                }
                match std::fs::read(format!("{fixtures}/{path}")) {
                    Ok(data) => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            data.len()
                        );
                        let _ = stream.write_all(&data);
                    }
                    Err(_) => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                    }
                }
            });
        }
    });

    let s = Server::start(&[
        (
            "OXIMG_SOURCE_BASE_URL",
            format!("http://127.0.0.1:{origin_port}"),
        ),
        ("OXIMG_UPSTREAM_TIMEOUT", "1".into()),
    ]);

    for path in ["stall.jpg", "drip.jpg"] {
        let t0 = std::time::Instant::now();
        assert_eq!(
            s.status_of(&format!("/resize/100/100/{path}")),
            504,
            "{path}: a deadline-exceeding origin is a gateway timeout"
        );
        let elapsed = t0.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(8),
            "{path}: answered in {elapsed:?}, the deadline did not bound the fetch"
        );
        // The permit is back: a healthy request completes promptly.
        let t0 = std::time::Instant::now();
        let (status, _, _) = s.get("/resize/100/100/photo.jpg").unwrap();
        assert_eq!(status, 200);
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "{path}: follow-up request stalled — permit not released?"
        );
    }
}

/// Extract a metric value by its exact exposition-line prefix.
fn metric(body: &str, prefix: &str) -> f64 {
    body.lines()
        .find(|l| l.starts_with(prefix) && l.as_bytes().get(prefix.len()) == Some(&b' '))
        .unwrap_or_else(|| panic!("no metric line starts with {prefix:?}"))
        .rsplit(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap_or_else(|_| panic!("unparseable value for {prefix:?}"))
}

/// Issue #4: the metrics surface. Off by default (the route does not
/// exist); with OXIMG_METRICS=1 the counters move with traffic —
/// status class and resolved format, both duration phases, permits,
/// and singleflight roles.
#[test]
fn metrics_endpoint_counts_what_happens() {
    let off = Server::start(&[]);
    assert_eq!(off.status_of("/metrics"), 404, "off by default");
    drop(off);

    let s = Server::start(&[("OXIMG_METRICS", "1".into())]);
    assert_eq!(s.get("/resize/100/100/photo.jpg").unwrap().0, 200);
    assert_eq!(s.get("/resize/100/100/photo.jpg@webp").unwrap().0, 200);
    assert_eq!(s.status_of("/resize/100/100/missing.jpg"), 404);
    assert_eq!(s.status_of("/resize/0/0/photo.jpg"), 400);

    let (status, ct, body) = s.get("/metrics").unwrap();
    assert_eq!(status, 200);
    assert!(ct.starts_with("text/plain"), "{ct}");
    let body = String::from_utf8(body).unwrap();

    // Status class x resolved format. The bare-URL 200 and the 404 both
    // resolved "no explicit/negotiated target" -> format="source"; the
    // 400 failed before resolution -> format="none".
    let m = |p: &str| metric(&body, p);
    assert_eq!(
        m("oximg_requests_total{class=\"2xx\",format=\"source\"}"),
        1.0
    );
    assert_eq!(
        m("oximg_requests_total{class=\"2xx\",format=\"webp\"}"),
        1.0
    );
    assert_eq!(
        m("oximg_requests_total{class=\"4xx\",format=\"source\"}"),
        1.0
    );
    assert_eq!(
        m("oximg_requests_total{class=\"4xx\",format=\"none\"}"),
        1.0
    );

    // Both duration phases observed once per request that reached the
    // pipeline (two 200s and the 404; the 400 never got there).
    assert_eq!(
        m("oximg_request_duration_seconds_count{phase=\"queue\"}"),
        3.0
    );
    assert_eq!(
        m("oximg_request_duration_seconds_count{phase=\"process\"}"),
        3.0
    );
    // Histogram buckets are cumulative: the +Inf bucket equals count.
    assert_eq!(
        m("oximg_request_duration_seconds_bucket{phase=\"queue\",le=\"+Inf\"}"),
        3.0
    );

    // Three distinct URLs, no concurrency: three leaders, no followers.
    assert_eq!(m("oximg_coalesced_requests_total{role=\"leader\"}"), 3.0);
    assert_eq!(m("oximg_coalesced_requests_total{role=\"follower\"}"), 0.0);

    // Local mode: the upstream family stays untouched.
    assert_eq!(m("oximg_upstream_fetch_total{outcome=\"ok\"}"), 0.0);

    // Nothing is processing at scrape time.
    assert_eq!(m("oximg_cpu_permits_in_use"), 0.0);
    assert!(m("oximg_cpu_workers") >= 1.0);
    assert_eq!(m("oximg_inflight_keys"), 0.0);
}

/// OXIMG_WORKERS pins the CPU permit count — the knob for platforms
/// that present more vCPUs than they allocate (issue #10: Cloud Run
/// cpu=1 shows 2 CPUs) — and the gauge confirms what took effect.
#[test]
fn workers_override_sizes_the_semaphore() {
    let s = Server::start(&[("OXIMG_WORKERS", "1".into()), ("OXIMG_METRICS", "1".into())]);
    // Still serves normally at one permit.
    assert_eq!(s.get("/resize/100/100/photo.jpg").unwrap().0, 200);
    let body = String::from_utf8(s.get("/metrics").unwrap().2).unwrap();
    assert_eq!(metric(&body, "oximg_cpu_workers"), 1.0);
}

/// The upstream outcome family, exercised against a real origin: ok,
/// not_found, and timeout each count exactly once — the split that
/// lets an operator tell "origin is slow" from "origin is broken".
#[test]
fn metrics_split_upstream_outcomes() {
    use std::io::Write;

    let fixtures = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let fixtures = fixtures.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .trim_start_matches('/');
                if path.starts_with("stall") {
                    std::thread::sleep(std::time::Duration::from_secs(30));
                    return;
                }
                match std::fs::read(format!("{fixtures}/{path}")) {
                    Ok(data) => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            data.len()
                        );
                        let _ = stream.write_all(&data);
                    }
                    Err(_) => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                    }
                }
            });
        }
    });

    let s = Server::start(&[
        (
            "OXIMG_SOURCE_BASE_URL",
            format!("http://127.0.0.1:{origin_port}"),
        ),
        ("OXIMG_UPSTREAM_TIMEOUT", "1".into()),
        ("OXIMG_METRICS", "1".into()),
    ]);
    assert_eq!(s.get("/resize/100/100/photo.jpg").unwrap().0, 200);
    assert_eq!(s.status_of("/resize/100/100/missing.jpg"), 404);
    assert_eq!(s.status_of("/resize/100/100/stall.jpg"), 504);

    let body = String::from_utf8(s.get("/metrics").unwrap().2).unwrap();
    let m = |p: &str| metric(&body, p);
    assert_eq!(m("oximg_upstream_fetch_total{outcome=\"ok\"}"), 1.0);
    assert_eq!(m("oximg_upstream_fetch_total{outcome=\"not_found\"}"), 1.0);
    assert_eq!(m("oximg_upstream_fetch_total{outcome=\"timeout\"}"), 1.0);
    assert_eq!(m("oximg_upstream_fetch_total{outcome=\"error\"}"), 0.0);
    // The 504 shows up in the status classes too.
    assert_eq!(
        m("oximg_requests_total{class=\"5xx\",format=\"source\"}"),
        1.0
    );
}

/// Issue #9: the Cloudflare Images option grammar. Not mounted unless
/// OXIMG_OPTIONS_PREFIX is set; mounted, the option list drives
/// dimensions, quality, and format, the filename is literal (no @fmt
/// token on this route), and rejections name the offending key.
#[test]
fn options_route_speaks_the_cloudflare_grammar() {
    let off = Server::start(&[]);
    assert_eq!(
        off.status_of("/image/width=100/photo.jpg"),
        404,
        "not mounted by default"
    );
    drop(off);

    let s = Server::start(&[("OXIMG_OPTIONS_PREFIX", "/image".into())]);

    // width-only follows the aspect ratio (photo.jpg is 200x150).
    let (status, ct, body) = s.get("/image/width=100/photo.jpg").unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/jpeg");
    let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!((w, h), (100, 75));

    // format= transcodes; nested paths work as on the positional route.
    let dir = nested_images_dir("options");
    drop(s);
    let s = Server::start(&[
        ("OXIMG_OPTIONS_PREFIX", "/image".into()),
        ("IMAGES_DIR", dir.to_str().unwrap().to_string()),
    ]);
    let (status, ct, body) = s
        .get("/image/width=100,format=webp/albums/2026/photo.jpg")
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/webp");
    let (fmt, w, _) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!(fmt, oximg::pipeline::ImageFormat::Webp);
    assert_eq!(w, 100);

    // quality= steers the encoder per request...
    let q20 = s
        .get("/image/width=100,quality=20/albums/2026/photo.jpg")
        .unwrap()
        .2;
    let q95 = s
        .get("/image/width=100,quality=95/albums/2026/photo.jpg")
        .unwrap()
        .2;
    assert!(
        q20.len() < q95.len(),
        "q20 ({}) must be smaller than q95 ({})",
        q20.len(),
        q95.len()
    );
    // ...for webp output too (the option applies to whatever format
    // the output resolves to).
    let wq20 = s
        .get("/image/width=100,quality=20,format=webp/albums/2026/photo.jpg")
        .unwrap()
        .2;
    let wq95 = s
        .get("/image/width=100,quality=95,format=webp/albums/2026/photo.jpg")
        .unwrap()
        .2;
    assert!(wq20.len() < wq95.len());

    // Option order does not change the bytes (parsed values key the
    // coalescing, not the raw string).
    let a = s
        .get("/image/width=100,quality=80/albums/2026/photo.jpg")
        .unwrap()
        .2;
    let b = s
        .get("/image/quality=80,width=100/albums/2026/photo.jpg")
        .unwrap()
        .2;
    assert_eq!(a, b);

    // The grammar rejections, end to end.
    for url in [
        "/image/width=100,fit=cover/albums/2026/photo.jpg", // unknown key
        "/image/width=100,width=50/albums/2026/photo.jpg",  // duplicate
        "/image/quality=80/albums/2026/photo.jpg",          // no dimension
        "/image/width=9000/albums/2026/photo.jpg",          // over cap
        "/image/width=100,quality=0/albums/2026/photo.jpg", // range
        "/image/width=100,format=gif/albums/2026/photo.jpg",
    ] {
        assert_eq!(s.status_of(url), 400, "{url}");
    }
    // Traversal guards apply on this route too.
    assert_eq!(
        s.status_of("/image/width=100/%2e%2e/secret.jpg"),
        400,
        "traversal refused"
    );

    // No @fmt token grammar here: the filename is literal, so the
    // suffixed name simply does not exist.
    assert_eq!(
        s.status_of("/image/width=100/albums/2026/photo.jpg@webp"),
        404,
        "options route takes the filename literally"
    );
}

/// format=auto (and an absent format) run the same Accept negotiation
/// as a bare positional URL, Vary rules included.
#[test]
fn options_route_negotiates_like_the_positional_route() {
    let s = Server::start(&[
        ("OXIMG_OPTIONS_PREFIX", "/image".into()),
        ("OXIMG_AUTO_FORMAT", "webp".into()),
    ]);
    for url in [
        "/image/width=100,format=auto/photo.jpg",
        "/image/width=100/photo.jpg",
    ] {
        let (status, ct, vary, _) = s.get_accept(url, Some("image/webp,*/*")).unwrap();
        assert_eq!(status, 200, "{url}");
        assert_eq!(ct, "image/webp", "{url}");
        assert_eq!(vary.as_deref(), Some("Accept"), "{url}");
    }
    // An explicit format= wins over negotiation.
    let (_, ct, _, _) = s
        .get_accept(
            "/image/width=100,format=jpeg/photo.jpg",
            Some("image/webp,*/*"),
        )
        .unwrap();
    assert_eq!(ct, "image/jpeg");
}

/// Signing covers the options route with the same scheme: the decoded
/// path (prefix, raw option order, nested file) is the signed
/// material, an unsigned request is refused while signing is on, and
/// a signature never authorizes different options.
#[test]
fn signed_urls_cover_the_options_route() {
    let key = "deadbeef".repeat(8);
    let salt = "cafebabe".repeat(8);
    let s = Server::start(&[
        ("OXIMG_OPTIONS_PREFIX", "/image".into()),
        ("OXIMG_KEY", key),
        ("OXIMG_SALT", salt),
    ]);
    assert_eq!(
        s.status_of("/image/width=100,quality=80/photo.jpg"),
        403,
        "unsigned options URL refused while signing is enabled"
    );
    // Precomputed with python hmac over
    // "/image/width=100,quality=80/photo.jpg" (same key/salt/scheme as
    // signing_gate).
    let sig = "3L75Z6c-9s0175zccq1KSndX9lTfEkuk0VciL8PXwPA";
    let (status, ct, _) = s
        .get(&format!("/{sig}/image/width=100,quality=80/photo.jpg"))
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/jpeg");
    // The signature covers the raw option string: different options
    // (here: a different quality) do not verify...
    assert_eq!(
        s.status_of(&format!("/{sig}/image/width=100,quality=20/photo.jpg")),
        403
    );
    // ...and neither does the same option set spelled in another order.
    assert_eq!(
        s.status_of(&format!("/{sig}/image/quality=80,width=100/photo.jpg")),
        403,
        "signed material is the raw path, order included"
    );
    let sig_plain = "w-W-i0ZiV_9i2RiBO4La4E0I_2dR4m7nmSg3Crqgfjk";
    assert_eq!(
        s.status_of(&format!("/{sig_plain}/image/width=100/photo.jpg")),
        200
    );
}

/// A misconfigured options prefix refuses to boot, fail-closed like
/// every other startup setting.
#[test]
fn invalid_options_prefix_refuses_to_boot() {
    for bad in [
        "image",
        "/resize",
        "/resize/x",
        "/health",
        "/a//b",
        "/a/../b",
    ] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximg"));
        cmd.env("PORT", "0")
            .env("OXIMG_OPTIONS_PREFIX", bad)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().expect("spawn oximg");
        let mut status = None;
        for _ in 0..200 {
            if let Ok(Some(s)) = child.try_wait() {
                status = Some(s);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let Some(status) = status else {
            let _ = child.kill();
            panic!("server booted despite OXIMG_OPTIONS_PREFIX={bad}");
        };
        assert!(!status.success(), "{bad} must exit non-zero");
    }
    // A multi-segment prefix is legitimate (the Cloudflare default).
    let s = Server::start(&[("OXIMG_OPTIONS_PREFIX", "/cdn-cgi/image".into())]);
    assert_eq!(s.status_of("/cdn-cgi/image/width=100/photo.jpg"), 200);
}

/// Issue #11 (phase 1): one retry on connection-level transients. An
/// origin that drops the first connection cold — the "single network
/// blip" that caused a production rollback — now yields a 200 instead
/// of a 502, the retry is visible in metrics, and an origin that is
/// actually down still fails as 502 after the one retry.
#[test]
fn transient_connection_failure_is_retried_once() {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let fixtures = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = listener.local_addr().unwrap().port();
    static CONNS: AtomicUsize = AtomicUsize::new(0);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Drop the very first connection without reading or
            // writing a byte: the client sees a reset/EOF at the
            // response head — a connection-level transient.
            if CONNS.fetch_add(1, Ordering::SeqCst) == 0 {
                drop(stream);
                continue;
            }
            let fixtures = fixtures.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let data = std::fs::read(format!("{fixtures}/photo.jpg")).unwrap();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    data.len()
                );
                let _ = stream.write_all(&data);
            });
        }
    });

    let s = Server::start(&[
        (
            "OXIMG_SOURCE_BASE_URL",
            format!("http://127.0.0.1:{origin_port}"),
        ),
        ("OXIMG_METRICS", "1".into()),
    ]);
    let (status, ct, _) = s.get("/resize/100/100/photo.jpg").unwrap();
    assert_eq!(status, 200, "the blip must be invisible to the client");
    assert_eq!(ct, "image/jpeg");
    let body = String::from_utf8(s.get("/metrics").unwrap().2).unwrap();
    assert_eq!(metric(&body, "oximg_upstream_retries_total"), 1.0);
    assert_eq!(
        metric(&body, "oximg_upstream_fetch_total{outcome=\"ok\"}"),
        1.0
    );
    drop(s);

    // A dead origin (nothing listening) is still a 502 — one retry,
    // not an infinite hope.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = dead.local_addr().unwrap().port();
    drop(dead);
    let s = Server::start(&[(
        "OXIMG_SOURCE_BASE_URL",
        format!("http://127.0.0.1:{dead_port}"),
    )]);
    assert_eq!(s.status_of("/resize/100/100/photo.jpg"), 502);
}

/// A fake GCP metadata server: answers the service-account token
/// route (asserting the Metadata-Flavor handshake) with a counted
/// token, so tests can pin both the auth header downstream and how
/// many times credentials were fetched.
fn fake_metadata_server(expires_in: u64) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let count = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&count);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let counter = Arc::clone(&counter);
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                // ureq (the http crate) emits lowercase header names.
                if !req.contains("metadata-flavor: google")
                    || !req.contains("/computemetadata/v1/instance/service-accounts/default/token")
                {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    return;
                }
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let body = format!(
                    "{{\"access_token\":\"test-token-{n}\",\"expires_in\":{expires_in},\"token_type\":\"Bearer\"}}"
                );
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
            });
        }
    });
    (port, count)
}

/// Issue #11 (phase 2a): the gs:// source mode reads a private bucket
/// with metadata-server credentials. The fake origin asserts the
/// Bearer header and the exact object path (bucket + configured
/// prefix + nested request path); 404/403 map to honest statuses; a
/// transient 503 is retried like the SDKs would.
#[test]
fn gcs_source_mode_authenticates_and_maps_statuses() {
    use std::io::Write;

    let (md_port, md_count) = fake_metadata_server(3600);

    let fixtures = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let gcs_port = listener.local_addr().unwrap().port();
    static FLAKY_HITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let fixtures = fixtures.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let respond = |stream: &mut std::net::TcpStream, code: &str| {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {code}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                };
                // Every request must carry the metadata-server token
                // (lowercase: the http crate's wire format).
                if !req
                    .to_lowercase()
                    .contains("authorization: bearer test-token-")
                {
                    return respond(&mut stream, "401 Unauthorized");
                }
                match path.as_str() {
                    "/test-bucket/originals/albums/2026/photo.jpg" => {
                        let data = std::fs::read(format!("{fixtures}/photo.jpg")).unwrap();
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            data.len()
                        );
                        let _ = stream.write_all(&data);
                    }
                    "/test-bucket/originals/forbidden.jpg" => respond(&mut stream, "403 Forbidden"),
                    "/test-bucket/originals/flaky.jpg" => {
                        if FLAKY_HITS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                            respond(&mut stream, "503 Service Unavailable");
                        } else {
                            let data = std::fs::read(format!("{fixtures}/photo.jpg")).unwrap();
                            let _ = write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                data.len()
                            );
                            let _ = stream.write_all(&data);
                        }
                    }
                    _ => respond(&mut stream, "404 Not Found"),
                }
            });
        }
    });

    let s = Server::start(&[
        ("OXIMG_SOURCE_BASE_URL", "gs://test-bucket/originals".into()),
        ("GCE_METADATA_HOST", format!("127.0.0.1:{md_port}")),
        ("OXIMG_GCS_ENDPOINT", format!("http://127.0.0.1:{gcs_port}")),
        ("OXIMG_METRICS", "1".into()),
    ]);

    // Happy path: nested key under the configured prefix, authenticated.
    let (status, ct, body) = s.get("/resize/100/100/albums/2026/photo.jpg").unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/jpeg");
    let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!((w, h), (100, 75));

    // Status mapping: absent object 404; permission problem is a
    // deployment fault (500), never blamed on the requester.
    assert_eq!(s.status_of("/resize/100/100/missing.jpg"), 404);
    assert_eq!(s.status_of("/resize/100/100/forbidden.jpg"), 500);

    // A transient 503 is retried once, SDK-style: the client sees 200.
    assert_eq!(s.get("/resize/100/100/flaky.jpg").unwrap().0, 200);
    let body = String::from_utf8(s.get("/metrics").unwrap().2).unwrap();
    assert!(metric(&body, "oximg_upstream_retries_total") >= 1.0);
    assert_eq!(
        metric(&body, "oximg_upstream_fetch_total{outcome=\"not_found\"}"),
        1.0
    );

    // The boot probe plus all requests shared one cached token: the
    // metadata server was hit exactly once.
    assert_eq!(
        md_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "token must be fetched once and cached"
    );
}

/// gs:// without reachable credentials refuses to boot (fail closed,
/// with the actionable message), and malformed source URLs are fatal
/// rather than falling back to a mode with different security
/// assumptions.
#[test]
fn gcs_boot_is_fail_closed() {
    // A dead metadata host: allocate a port and close it.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = dead.local_addr().unwrap().port();
    drop(dead);

    for envs in [
        vec![
            ("OXIMG_SOURCE_BASE_URL", "gs://bucket".to_string()),
            ("GCE_METADATA_HOST", format!("127.0.0.1:{dead_port}")),
        ],
        vec![("OXIMG_SOURCE_BASE_URL", "gs://".to_string())],
        vec![("OXIMG_SOURCE_BASE_URL", "s3://bucket".to_string())],
        vec![("OXIMG_SOURCE_BASE_URL", "ftp://host".to_string())],
        vec![("OXIMG_SOURCE_BASE_URL", "bucket-host/path".to_string())],
    ] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximg"));
        cmd.env("PORT", "0")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        for (k, v) in &envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn oximg");
        let mut status = None;
        for _ in 0..400 {
            if let Ok(Some(s)) = child.try_wait() {
                status = Some(s);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let Some(status) = status else {
            let _ = child.kill();
            panic!("server booted despite {envs:?}");
        };
        assert!(!status.success(), "{envs:?} must exit non-zero");
        // The claim is "fail closed with an actionable message", so
        // pin the message, not just the exit code.
        let mut stderr = String::new();
        std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
        assert!(
            stderr.contains("oximg: fatal:"),
            "{envs:?}: no fatal diagnostic on stderr: {stderr:?}"
        );
    }
}

/// Issue #13: a source key no store can serve is the requester's
/// fault, not the upstream's. Over-length keys answer 404 *without a
/// round trip*, an origin's 400/414 answers 400, and neither counts
/// in the upstream `error` series that an operator watches for
/// origin health.
#[test]
fn impossible_source_keys_are_client_errors_not_502() {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (md_port, _md_count) = fake_metadata_server(3600);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let gcs_port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let counter = Arc::clone(&counter);
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
                counter.fetch_add(1, Ordering::SeqCst);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/");
                // The store's own verdict on a malformed request.
                let code = if path.contains("bad-request") {
                    "400 Bad Request"
                } else if path.contains("too-long") {
                    "414 URI Too Long"
                } else {
                    "500 Internal Server Error"
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {code}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            });
        }
    });

    let s = Server::start(&[
        ("OXIMG_SOURCE_BASE_URL", "gs://test-bucket".into()),
        ("GCE_METADATA_HOST", format!("127.0.0.1:{md_port}")),
        ("OXIMG_GCS_ENDPOINT", format!("http://127.0.0.1:{gcs_port}")),
        ("OXIMG_METRICS", "1".into()),
    ]);
    hits.store(0, Ordering::SeqCst);

    // GCS caps object names at 1024 bytes: 1024 is legal (the origin
    // answers, here with a 500 -> 502), 1025 cannot exist.
    let legal = "x".repeat(1020);
    assert_eq!(
        s.status_of(&format!("/resize/100/100/{legal}.png")),
        502,
        "a legal-length key still reaches the origin"
    );
    let before = hits.load(Ordering::SeqCst);
    assert!(before > 0, "the legal key must have been fetched");

    for len in [1025usize, 1400] {
        let over = "y".repeat(len);
        assert_eq!(
            s.status_of(&format!("/resize/100/100/{over}.png")),
            404,
            "{len}-byte key is impossible, not an upstream failure"
        );
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        before,
        "over-length keys must never leave the process"
    );

    // The store's own client errors are 400s, not 502s.
    assert_eq!(s.status_of("/resize/100/100/bad-request.png"), 400);
    assert_eq!(s.status_of("/resize/100/100/too-long.png"), 400);

    let body = String::from_utf8(s.get("/metrics").unwrap().2).unwrap();
    let m = |p: &str| metric(&body, p);
    // Impossible keys are counted apart from upstream ill-health: the
    // two 400s are "rejected", the over-length pair "not_found", and
    // only the genuine 500 lands in "error".
    assert_eq!(m("oximg_upstream_fetch_total{outcome=\"rejected\"}"), 2.0);
    assert_eq!(m("oximg_upstream_fetch_total{outcome=\"not_found\"}"), 2.0);
    assert_eq!(m("oximg_upstream_fetch_total{outcome=\"error\"}"), 1.0);
}

/// The HTTP source mode shares the verdict: an origin answering
/// 400/414 (the shape an over-long URL produces there) is a client
/// error, not an upstream failure.
#[test]
fn http_origin_client_errors_are_not_upstream_failures() {
    use std::io::Write;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/");
                let code = if path.contains("too-long") {
                    "414 URI Too Long"
                } else if path.contains("bad-request") {
                    "400 Bad Request"
                } else {
                    "503 Service Unavailable"
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {code}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            });
        }
    });

    let s = Server::start(&[
        (
            "OXIMG_SOURCE_BASE_URL",
            format!("http://127.0.0.1:{origin_port}"),
        ),
        ("OXIMG_METRICS", "1".into()),
    ]);
    assert_eq!(s.status_of("/resize/100/100/too-long.png"), 400);
    assert_eq!(s.status_of("/resize/100/100/bad-request.png"), 400);
    assert_eq!(
        s.status_of("/resize/100/100/other.png"),
        502,
        "a genuine origin fault stays 502"
    );
    let body = String::from_utf8(s.get("/metrics").unwrap().2).unwrap();
    assert_eq!(
        metric(&body, "oximg_upstream_fetch_total{outcome=\"rejected\"}"),
        2.0
    );
    assert_eq!(
        metric(&body, "oximg_upstream_fetch_total{outcome=\"error\"}"),
        1.0
    );
}

/// Issue #14 end to end: a source taller than WebP can express comes
/// back as the largest WebP that keeps its shape, instead of a 500
/// from the encoder. The same source in a format without that ceiling
/// keeps its full height, so the clamp is the format's constraint and
/// not a new global cap.
#[test]
fn tall_sources_encode_to_webp_by_fitting_the_format_ceiling() {
    let dir = std::env::temp_dir().join(format!("oximg-tallwebp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // 20x16500: past WebP's 16383 ceiling by 117 px, but only 330k
    // pixels, so generating and encoding it stays cheap.
    let (w, h) = (20u32, 16500u32);
    let mut png = Vec::new();
    let mut enc = png::Encoder::new(&mut png, w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    let mut rows = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            rows.extend([(x * 12) as u8, (y % 251) as u8, 128]);
        }
    }
    writer.write_image_data(&rows).unwrap();
    writer.finish().unwrap();
    std::fs::write(dir.join("tall.png"), &png).unwrap();

    let s = Server::start(&[("IMAGES_DIR", dir.to_str().unwrap().to_string())]);

    // Width-only at the source's own width (the reporter's shape: the
    // height is unconstrained and overflows the ceiling on its own).
    let (status, ct, body) = s.get("/resize/20/0/tall.png@webp").unwrap();
    assert_eq!(status, 200, "the format ceiling must not fail the request");
    assert_eq!(ct, "image/webp");
    let (fmt, ow, oh) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!(fmt, oximg::pipeline::ImageFormat::Webp);
    assert_eq!(oh, 16383, "the long side sits exactly on the limit");
    // 20 * 16383/16500 = 19.86, which rounds back to 20: the scale is
    // proportional, and the narrow axis keeps its nearest pixel.
    assert_eq!(ow, 20);

    // The options route reaches the same code path.
    let opts = Server::start(&[
        ("IMAGES_DIR", dir.to_str().unwrap().to_string()),
        ("OXIMG_OPTIONS_PREFIX", "/image".into()),
    ]);
    let (status, ct, body) = opts.get("/image/width=20,format=webp/tall.png").unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/webp");
    let (_, _, oh) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!(oh, 16383);

    // PNG has no such ceiling: the full height survives.
    let (status, ct, body) = s.get("/resize/20/0/tall.png").unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/png");
    let (_, ow, oh) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!((ow, oh), (20, 16500), "PNG keeps the source dimensions");
}

/// Issue #15: a CORS preflight needs a 2xx, and a 405 cannot be
/// rescued by CORS headers attached at the edge — the status itself
/// fails it. OPTIONS on every image route answers 204 with `Allow`,
/// including on the signed routes (a preflight performs no work and
/// answers identically for every path, so requiring a signature would
/// only stop the browser from ever sending the signed GET). Other
/// methods keep their 405.
#[test]
fn options_answers_204_so_preflight_can_succeed() {
    let s = Server::start(&[("OXIMG_OPTIONS_PREFIX", "/image".into())]);
    for path in [
        "/resize/100/100/photo.jpg",
        "/image/width=100/photo.jpg",
        // A nested path and an @fmt token take the same route shapes.
        "/resize/100/100/photo.jpg@webp",
    ] {
        let (status, allow) = s.method_of("OPTIONS", path);
        assert_eq!(status, 204, "{path}");
        assert_eq!(allow.as_deref(), Some("GET, HEAD, OPTIONS"), "{path}");
    }
    // Genuinely unsupported methods still say so.
    assert_eq!(s.method_of("POST", "/resize/100/100/photo.jpg").0, 405);
    // GET is untouched.
    assert_eq!(s.get("/resize/100/100/photo.jpg").unwrap().0, 200);
    drop(s);

    // With signing on, the preflight still succeeds while the
    // unsigned GET it precedes is still refused.
    let signed = Server::start(&[
        ("OXIMG_OPTIONS_PREFIX", "/image".into()),
        ("OXIMG_KEY", "deadbeef".repeat(8)),
        ("OXIMG_SALT", "cafebabe".repeat(8)),
    ]);
    for path in [
        "/resize/100/100/photo.jpg",
        "/AAAA/resize/100/100/photo.jpg",
        "/AAAA/image/width=100/photo.jpg",
    ] {
        assert_eq!(signed.method_of("OPTIONS", path).0, 204, "{path}");
    }
    assert_eq!(signed.status_of("/resize/100/100/photo.jpg"), 403);
}

/// The 413 body is generic across the three source caps, so the log
/// line is the only place an operator learns *which* limit they are
/// against and by how much. Previously no 413 logged anything.
#[test]
fn oversize_413_names_the_limit_on_stderr() {
    let dir = std::env::temp_dir().join(format!("oximg-413log-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // A 1000x1000 RGB PNG: 3 MB staged plus 6 MB as the linear-light
    // resize input, comfortably over the 1 MiB floor cap. (A JPEG would
    // not do — it streams, which is exactly why it is cheap.)
    let mut png_bytes = Vec::new();
    let mut enc = png::Encoder::new(&mut png_bytes, 1000, 1000);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    writer
        .write_image_data(&vec![77u8; 1000 * 1000 * 3])
        .unwrap();
    writer.finish().unwrap();
    std::fs::write(dir.join("big.png"), &png_bytes).unwrap();

    // Spawn by hand: the shared helper drains stderr into a sink.
    let mut child = Command::new(env!("CARGO_BIN_EXE_oximg"))
        .env("PORT", "0")
        .env("IMAGES_DIR", dir.to_str().unwrap())
        .env("OXIMG_MAX_DECODED_BYTES", (1024 * 1024).to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn oximg");
    use std::io::BufRead;
    let stderr = child.stderr.take().unwrap();
    let mut lines = std::io::BufReader::new(stderr).lines();
    let port: u16 = lines
        .find_map(|l| {
            let l = l.ok()?;
            l.strip_prefix("oximg listening on :")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .expect("listening line");

    let url = format!("http://127.0.0.1:{port}/resize/500/500/big.png");
    let status = match ureq::get(&url).call() {
        Ok(r) => r.status().as_u16(),
        Err(ureq::Error::StatusCode(s)) => s,
        Err(e) => panic!("transport error: {e}"),
    };
    assert_eq!(status, 413);

    let logged = lines
        .find_map(|l| {
            let l = l.ok()?;
            l.contains("status=413").then_some(l)
        })
        .expect("a 413 must log its cause");
    assert!(
        logged.contains("OXIMG_MAX_DECODED_BYTES"),
        "the log must name which limit: {logged}"
    );
    assert!(
        logged.contains("bytes"),
        "and the figure behind it: {logged}"
    );
    let _ = child.kill();
    let _ = child.wait();
}

/// Issue #17: the decoded-bytes cap is expressed in the unit an
/// operator has a limit in, and — the part that made pixel caps
/// unusable — it separates sources that pixel counts cannot. The
/// estimate is always computed and exposed, so a cap can be read off a
/// corpus before being enforced.
#[test]
fn decoded_bytes_cap_bounds_what_pixels_cannot() {
    let dir = std::env::temp_dir().join(format!("oximg-decbytes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // A wide-but-shallow PNG: 4000x800 = 3.2 MP, ~9.6 MB decoded at
    // RGB8. Cheap to generate, and its estimate is far above the
    // 1 MiB floor so a cap can straddle it.
    let (w, h) = (4000u32, 800u32);
    let mut png_bytes = Vec::new();
    let mut enc = png::Encoder::new(&mut png_bytes, w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    writer
        .write_image_data(&vec![96u8; (w * h * 3) as usize])
        .unwrap();
    writer.finish().unwrap();
    std::fs::write(dir.join("wide.png"), &png_bytes).unwrap();
    // The cheap baseline-JPEG comparison lives in the same directory.
    std::fs::copy(
        format!("{}/tests/fixtures/photo.jpg", env!("CARGO_MANIFEST_DIR")),
        dir.join("photo.jpg"),
    )
    .unwrap();
    let images = dir.to_str().unwrap().to_string();

    // Unset: the estimate is computed and exposed, nothing is refused.
    let s = Server::start(&[
        ("IMAGES_DIR", images.clone()),
        ("OXIMG_METRICS", "1".into()),
    ]);
    assert_eq!(s.get("/resize/100/100/wide.png").unwrap().0, 200);
    let body = String::from_utf8(s.get("/metrics").unwrap().2).unwrap();
    assert_eq!(
        metric(&body, "oximg_decoded_bytes_estimate_count"),
        1.0,
        "the estimate is recorded even with no cap set"
    );
    // 3.2 MP RGB: 9.6 MB staged plus 19.2 MB as the linear-light u16
    // resize input, so ~28 MB — the 32 MiB bucket holds it and the
    // 16 MiB one does not. This histogram is what an operator reads a
    // cap off, so its placement is the contract.
    assert_eq!(
        metric(
            &body,
            "oximg_decoded_bytes_estimate_bucket{le=\"33554432\"}"
        ),
        1.0
    );
    assert_eq!(
        metric(
            &body,
            "oximg_decoded_bytes_estimate_bucket{le=\"16777216\"}"
        ),
        0.0
    );
    assert!(metric(&body, "oximg_decoded_bytes_estimate_sum") > 25_000_000.0);
    drop(s);

    // A cap above the estimate serves; below it answers 413 (the same
    // class as the other source caps).
    let generous = Server::start(&[
        ("IMAGES_DIR", images.clone()),
        ("OXIMG_MAX_DECODED_BYTES", (64 * 1024 * 1024).to_string()),
    ]);
    assert_eq!(generous.get("/resize/100/100/wide.png").unwrap().0, 200);
    drop(generous);

    let tight = Server::start(&[
        ("IMAGES_DIR", images.clone()),
        ("OXIMG_MAX_DECODED_BYTES", (4 * 1024 * 1024).to_string()),
    ]);
    assert_eq!(tight.status_of("/resize/100/100/wide.png"), 413);
    // The cheap baseline-JPEG path is *not* caught by the same cap:
    // shrink-on-load decodes near the output, which is the whole point
    // — a pixel cap could not express this distinction.
    assert_eq!(tight.get("/resize/100/100/photo.jpg").unwrap().0, 200);

    // A set-but-absurd cap refuses to boot rather than 413ing
    // everything.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximg"));
    cmd.env("PORT", "0")
        .env("OXIMG_MAX_DECODED_BYTES", "1024")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let mut status = None;
    for _ in 0..200 {
        if let Ok(Some(st)) = child.try_wait() {
            status = Some(st);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let Some(status) = status else {
        let _ = child.kill();
        panic!("server booted with a 1 KiB decoded-bytes cap");
    };
    assert!(!status.success());
}

/// Regression for the wiring, not the model (issue #17 field
/// validation): a streaming JPEG's estimate must come from the
/// post-shrink-on-load dimensions. The first attempt read
/// `Decompress::width()`, which is libjpeg's `image_width` — the
/// source — so an 8 MP source asked for a 100 px output estimated the
/// whole frame and the cheapest path produced the largest figure.
///
/// The cap here sits far below the source-side cost and far above the
/// output-side one, so only correct wiring passes.
#[test]
fn jpeg_estimate_follows_the_shrink_on_load_scale() {
    let dir = std::env::temp_dir().join(format!("oximg-jpegest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // Build an 8 MP JPEG through the pipeline itself.
    let (w, h) = (4000usize, 2000usize);
    let mut png = Vec::new();
    let mut enc = png::Encoder::new(&mut png, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    let mut rows = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            rows.extend([(x % 251) as u8, (y % 241) as u8, 140]);
        }
    }
    writer.write_image_data(&rows).unwrap();
    writer.finish().unwrap();
    let encode = |encoder| {
        oximg::pipeline::process(
            &png,
            &oximg::pipeline::Params {
                output: Some(oximg::pipeline::ImageFormat::Jpeg),
                encoder,
                ..Default::default()
            },
        )
        .expect("encode fixture")
        .0
    };
    // PRESET=fast is mozjpeg's baseline profile; the default (jpegli)
    // writes progressive, which is the other half of this test.
    std::fs::write(
        dir.join("baseline.jpg"),
        encode(oximg::pipeline::Encoder::MozFast),
    )
    .unwrap();
    std::fs::write(
        dir.join("progressive.jpg"),
        encode(oximg::pipeline::Encoder::Jpegli),
    )
    .unwrap();

    // 8 MP staged as RGB is 24 MB, and with the linear-light copy 72 MB;
    // the 100x50 output side is well under a mebibyte. A 8 MiB cap
    // therefore fails loudly if the estimate ever regresses to source
    // dimensions.
    let s = Server::start(&[
        ("IMAGES_DIR", dir.to_str().unwrap().to_string()),
        ("OXIMG_MAX_DECODED_BYTES", (8 * 1024 * 1024).to_string()),
        ("OXIMG_METRICS", "1".into()),
    ]);
    let (status, _, body) = s
        .get("/resize/100/0/baseline.jpg")
        .expect("a shrink-on-load JPEG must fit a small cap");
    assert_eq!(status, 200);
    let (_, ow, _) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!(ow, 100);

    // Same pixels, same request, progressive encoding: libjpeg will
    // buffer whole-image coefficients (8 MP x 3 comps x 2 B = 48 MB),
    // which no output size reduces — so it exceeds the same cap. This
    // is the distinction a pixel cap cannot express, in one pair.
    assert_eq!(
        s.status_of("/resize/100/0/progressive.jpg"),
        413,
        "progressive coefficients must be counted"
    );

    // The recorded estimate must be small — the smallest bucket holds
    // it — rather than source-sized.
    let metrics = String::from_utf8(s.get("/metrics").unwrap().2).unwrap();
    assert_eq!(
        metric(
            &metrics,
            "oximg_decoded_bytes_estimate_bucket{le=\"1048576\"}"
        ),
        1.0,
        "a streaming JPEG's estimate is output-sized, not source-sized"
    );
}

/// Every source path that estimates decoded bytes must be reachable by
/// the cap, not just the JPEG and PNG ones the other tests cover. The
/// buffered formats (WebP, AVIF, PNG) all materialize a whole frame, so
/// a cap below that frame must refuse them.
#[test]
fn decoded_bytes_cap_reaches_every_source_path() {
    let dir = std::env::temp_dir().join(format!("oximg-allpaths-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // One 900x900 RGB source, re-encoded into each buffered format
    // through the pipeline itself. Staged it is 2.4 MB, plus 4.9 MB as
    // the linear-light u16 input — so a 4 MiB cap is below the frame
    // and a 64 MiB one is well above it.
    let (w, h) = (900usize, 900usize);
    let mut png = Vec::new();
    let mut enc = png::Encoder::new(&mut png, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    let mut rows = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            rows.extend([(x % 233) as u8, (y % 197) as u8, 90]);
        }
    }
    writer.write_image_data(&rows).unwrap();
    writer.finish().unwrap();
    std::fs::write(dir.join("big.png"), &png).unwrap();

    let transcode = |target| {
        oximg::pipeline::process(
            &png,
            &oximg::pipeline::Params {
                output: Some(target),
                ..Default::default()
            },
        )
        .expect("transcode fixture")
        .0
    };
    std::fs::write(
        dir.join("big.webp"),
        transcode(oximg::pipeline::ImageFormat::Webp),
    )
    .unwrap();
    #[cfg(feature = "avif")]
    std::fs::write(
        dir.join("big.avif"),
        transcode(oximg::pipeline::ImageFormat::Avif),
    )
    .unwrap();

    let images = dir.to_str().unwrap().to_string();
    let tight = Server::start(&[
        ("IMAGES_DIR", images.clone()),
        ("OXIMG_MAX_DECODED_BYTES", (4 * 1024 * 1024).to_string()),
    ]);
    let generous = Server::start(&[
        ("IMAGES_DIR", images),
        ("OXIMG_MAX_DECODED_BYTES", (64 * 1024 * 1024).to_string()),
    ]);

    // Only mutated under the avif feature.
    #[cfg_attr(not(feature = "avif"), allow(unused_mut))]
    let mut sources = vec!["big.png", "big.webp"];
    #[cfg(feature = "avif")]
    sources.push("big.avif");
    for file in sources {
        assert_eq!(
            generous.get(&format!("/resize/100/100/{file}")).unwrap().0,
            200,
            "{file} fits a generous cap"
        );
        assert_eq!(
            tight.status_of(&format!("/resize/100/100/{file}")),
            413,
            "{file}: a buffered format must be bounded by the frame it materializes"
        );
    }

    // The streaming JPEG path is the contrast: the same pixels cost
    // only the output side, so the tight cap serves it. This is the
    // distinction a source-pixel cap cannot express, across formats.
    // PRESET=fast is mozjpeg's baseline profile — the default (jpegli)
    // writes progressive, whose coefficient arrays are themselves over
    // this cap, as jpeg_estimate_follows_the_shrink_on_load_scale pins.
    let baseline = oximg::pipeline::process(
        &png,
        &oximg::pipeline::Params {
            output: Some(oximg::pipeline::ImageFormat::Jpeg),
            encoder: oximg::pipeline::Encoder::MozFast,
            ..Default::default()
        },
    )
    .expect("transcode fixture")
    .0;
    std::fs::write(dir.join("big.jpg"), &baseline).unwrap();
    assert_eq!(
        tight.get("/resize/100/100/big.jpg").unwrap().0,
        200,
        "a streaming source of identical pixels fits the same cap"
    );
}

/// The conservative both-orientations fit, end to end. The check runs
/// before a PNG's `eXIf` chunk is parsed, so it cannot know which way
/// the fit will go — and therefore assumes the larger candidate for
/// *every* source. That is deliberate: an axis-swapping source under an
/// asymmetric box really does produce the larger frame, and
/// under-counting it is the failure mode this cap exists to prevent.
#[test]
fn oriented_sources_are_capped_on_the_larger_fit() {
    let dir = std::env::temp_dir().join(format!("oximg-orientcap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // 1200x300 stored; orientation 6 presents it as 300x1200, so a
    // width-only box of 1000 fits the displayed frame to 300x1200 —
    // 360k pixels of output against the unoriented fit's 250k.
    let (w, h) = (1200usize, 300usize);
    let px = vec![120u8; w * h * 3];
    std::fs::write(
        dir.join("upright.png"),
        common::png_with_orientation(&px, w, h, 1),
    )
    .unwrap();
    std::fs::write(
        dir.join("rotated.png"),
        common::png_with_orientation(&px, w, h, 6),
    )
    .unwrap();
    let images = dir.to_str().unwrap().to_string();

    // The rotated source really does produce the swapped, larger frame.
    let loose = Server::start(&[("IMAGES_DIR", images.clone())]);
    let (_, _, body) = loose.get("/resize/1000/0/rotated.png").unwrap();
    let (_, ow, oh) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!((ow, oh), (300, 1200), "orientation 6 swaps the axes");
    let (_, _, body) = loose.get("/resize/1000/0/upright.png").unwrap();
    let (_, ow, oh) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!((ow, oh), (1000, 250));

    // A cap between the unoriented estimate (3.24 MB source-side plus
    // 2.25 MB of 1000x250 output) and the conservative one (the same
    // source side plus 3.24 MB of 1200x300) refuses *both*: the
    // estimate cannot yet know which way the fit goes, so it assumes
    // the larger for either. Refusing the upright source is the price
    // of never under-counting the rotated one.
    let capped = Server::start(&[
        ("IMAGES_DIR", images.clone()),
        ("OXIMG_MAX_DECODED_BYTES", (6 * 1024 * 1024).to_string()),
    ]);
    for file in ["rotated.png", "upright.png"] {
        assert_eq!(
            capped.status_of(&format!("/resize/1000/0/{file}")),
            413,
            "{file} is estimated on the larger candidate fit"
        );
    }
    // Above the conservative figure, both serve.
    let generous = Server::start(&[
        ("IMAGES_DIR", images),
        ("OXIMG_MAX_DECODED_BYTES", (16 * 1024 * 1024).to_string()),
    ]);
    for file in ["rotated.png", "upright.png"] {
        assert_eq!(
            generous.get(&format!("/resize/1000/0/{file}")).unwrap().0,
            200,
            "{file} serves above the conservative estimate"
        );
    }
}

/// Issue #19: the cap can only ever name what it *rejects*, so a
/// deployment learning its corpus needs expensive requests named while
/// they are still served. The threshold is orthogonal to the cap, both
/// unset by default, and the report carries the same per-term
/// breakdown a rejection does.
#[test]
fn expensive_requests_are_reported_without_being_refused() {
    let dir = std::env::temp_dir().join(format!("oximg-report-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // 800x800 RGB: 1.9 MB staged plus 3.8 MB as the linear-light input.
    let (w, h) = (800u32, 800u32);
    let mut png = Vec::new();
    let mut enc = png::Encoder::new(&mut png, w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    writer
        .write_image_data(&vec![55u8; (w * h * 3) as usize])
        .unwrap();
    writer.finish().unwrap();
    std::fs::write(dir.join("costly.png"), &png).unwrap();
    let images = dir.to_str().unwrap().to_string();

    /// Run one server with `envs`, issue `path`, then stop it and drain
    /// stderr to EOF. Killing first is what makes the read terminate —
    /// reading a live process's stderr for a line that may never come
    /// is how this test first hung.
    fn run(images: &str, envs: &[(&str, String)], path: &str) -> (u16, Vec<String>) {
        use std::io::BufRead;
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximg"));
        cmd.env("PORT", "0")
            .env("IMAGES_DIR", images)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn oximg");
        let stderr = child.stderr.take().unwrap();
        let mut reader = std::io::BufReader::new(stderr);
        let mut port = None;
        let mut line = String::new();
        for _ in 0..100 {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if let Some(rest) = line.strip_prefix("oximg listening on :") {
                        port = rest.split_whitespace().next().and_then(|p| p.parse().ok());
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let port: u16 = port.expect("listening line");
        let status = match ureq::get(format!("http://127.0.0.1:{port}{path}")).call() {
            Ok(r) => r.status().as_u16(),
            Err(ureq::Error::StatusCode(s)) => s,
            Err(e) => panic!("transport error: {e}"),
        };
        let _ = child.kill();
        let _ = child.wait();
        let rest = reader.lines().map_while(Result::ok).collect();
        (status, rest)
    }

    // Threshold below the estimate: served *and* named, with the
    // filename and the terms.
    let (status, logs) = run(
        &images,
        &[("OXIMG_LOG_DECODED_BYTES_ABOVE", (1024 * 1024).to_string())],
        "/resize/200/200/costly.png",
    );
    assert_eq!(status, 200);
    let line = logs
        .iter()
        .find(|l| l.contains("decoded-bytes"))
        .unwrap_or_else(|| panic!("an above-threshold request must be reported: {logs:?}"));
    assert!(line.contains("costly.png"), "must name the source: {line}");
    assert!(line.contains("staged"), "must carry the terms: {line}");
    assert!(line.contains("resize input"), "{line}");

    // Threshold above the estimate: nothing reported, request served.
    let (status, logs) = run(
        &images,
        &[(
            "OXIMG_LOG_DECODED_BYTES_ABOVE",
            (512 * 1024 * 1024).to_string(),
        )],
        "/resize/200/200/costly.png",
    );
    assert_eq!(status, 200);
    assert!(
        !logs.iter().any(|l| l.contains("decoded-bytes")),
        "nothing below the threshold may be reported: {logs:?}"
    );

    // Unset: byte-identical behaviour to before the feature.
    let (status, logs) = run(&images, &[], "/resize/200/200/costly.png");
    assert_eq!(status, 200);
    assert!(!logs.iter().any(|l| l.contains("decoded-bytes")));

    // The knobs are orthogonal, and one request never logs the same
    // terms twice: a refusal names itself with its limit clause and
    // does not also emit the served-request report.
    let (status, logs) = run(
        &images,
        &[
            ("OXIMG_LOG_DECODED_BYTES_ABOVE", "1048576".to_string()),
            ("OXIMG_MAX_DECODED_BYTES", (2 * 1024 * 1024).to_string()),
        ],
        "/resize/200/200/costly.png",
    );
    assert_eq!(status, 413);
    assert!(
        logs.iter().any(|l| l.contains("status=413")),
        "the rejection must be logged: {logs:?}"
    );
    assert!(
        !logs.iter().any(|l| l.contains("decoded-bytes")),
        "a refused request must not also emit the served report: {logs:?}"
    );
}

/// The `phase="fetch"` split (issue #20 follow-up), with issue #22's
/// contract: the origin wait is measured — validated against a *known*
/// injected latency, since a timing metric nobody has checked against
/// a reference is not evidence — and it happens *outside* the CPU
/// permit, so the process phase must NOT contain it.
#[test]
fn fetch_phase_measures_the_origin_wait_outside_the_permit() {
    use std::io::Write;

    // An origin that stalls a known 120ms before answering.
    const DELAY_MS: u64 = 120;
    let fixtures = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let fixtures = fixtures.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                std::thread::sleep(std::time::Duration::from_millis(DELAY_MS));
                let data = std::fs::read(format!("{fixtures}/photo.jpg")).unwrap();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    data.len()
                );
                let _ = stream.write_all(&data);
            });
        }
    });

    let s = Server::start(&[
        (
            "OXIMG_SOURCE_BASE_URL",
            format!("http://127.0.0.1:{origin_port}"),
        ),
        ("OXIMG_METRICS", "1".into()),
    ]);
    for w in [100, 120, 140] {
        assert_eq!(s.get(&format!("/resize/{w}/0/photo.jpg")).unwrap().0, 200);
    }
    let body = String::from_utf8(s.get("/metrics").unwrap().2).unwrap();
    let m = |p: &str| metric(&body, p);

    // One observation per request, same as the other phases.
    assert_eq!(
        m("oximg_request_duration_seconds_count{phase=\"fetch\"}"),
        3.0
    );
    let fetch_mean = m("oximg_request_duration_seconds_sum{phase=\"fetch\"}") / 3.0;
    let process_mean = m("oximg_request_duration_seconds_sum{phase=\"process\"}") / 3.0;
    let injected = DELAY_MS as f64 / 1000.0;
    // The measurement must find the injected wait, and must not invent
    // much beyond it: a local origin adds only connect + loopback (and,
    // since issue #22, the fetch-slot acquire — uncontended here).
    assert!(
        (injected..injected + 0.15).contains(&fetch_mean),
        "fetch mean {fetch_mean:.3}s must reflect the injected {injected:.3}s"
    );
    // Issue #22's contract, pinned: the wait does not hold a CPU
    // permit, so the process phase — the permit's actual hold — must
    // be pure decode+encode, far below the injected latency. (Before
    // the change fetch was a subset of process and this read
    // ~injected + decode.)
    assert!(
        process_mean < injected,
        "process {process_mean:.3}s must not contain the {injected:.3}s origin wait"
    );
    drop(s);

    // Local sources have no fetch phase at all — no stale value from a
    // previous remote request may leak onto a reused thread.
    let local = Server::start(&[("OXIMG_METRICS", "1".into())]);
    assert_eq!(local.get("/resize/100/0/photo.jpg").unwrap().0, 200);
    let body = String::from_utf8(local.get("/metrics").unwrap().2).unwrap();
    assert_eq!(
        metric(
            &body,
            "oximg_request_duration_seconds_count{phase=\"fetch\"}"
        ),
        0.0,
        "a local source records no fetch time"
    );
}

/// Issue #22's throughput mechanism, pinned by wall clock: with ONE
/// CPU permit and a slow origin, a burst of distinct requests must
/// overlap their fetches. Before the change each fetch was serialized
/// behind the single permit (4 requests x 500ms = 2s+ of fetching
/// alone); after it, all four downloads run concurrently and the burst
/// completes in roughly one origin round trip plus four decodes.
#[test]
fn burst_fetches_overlap_despite_one_cpu_permit() {
    use std::io::Write;

    const DELAY_MS: u64 = 500;
    let fixtures = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let fixtures = fixtures.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                std::thread::sleep(std::time::Duration::from_millis(DELAY_MS));
                let data = std::fs::read(format!("{fixtures}/photo.jpg")).unwrap();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    data.len()
                );
                let _ = stream.write_all(&data);
            });
        }
    });

    let s = Server::start(&[
        (
            "OXIMG_SOURCE_BASE_URL",
            format!("http://127.0.0.1:{origin_port}"),
        ),
        ("OXIMG_WORKERS", "1".into()),
    ]);
    // Distinct widths = distinct flight keys, so nothing coalesces.
    let t0 = std::time::Instant::now();
    std::thread::scope(|sc| {
        for w in [100, 110, 120, 130] {
            let s = &s;
            sc.spawn(move || {
                assert_eq!(s.get(&format!("/resize/{w}/0/photo.jpg")).unwrap().0, 200);
            });
        }
    });
    let elapsed = t0.elapsed();
    // Serialized fetches would need >= 4 x 500ms before decode even
    // starts; overlapped ones need ~500ms + 4 small decodes. The bound
    // sits far from both so a slow CI machine cannot flip it.
    assert!(
        elapsed < std::time::Duration::from_millis(2 * DELAY_MS),
        "a 4-burst against a {DELAY_MS}ms origin took {elapsed:?} with one permit — \
         fetches are serializing behind the CPU permit"
    );
}
