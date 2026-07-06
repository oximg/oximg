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
    assert_eq!(s.status_of("/resize/0/100/photo.jpg"), 400);
    assert_eq!(s.status_of("/resize/9000/9000/photo.jpg"), 400);
    assert_eq!(s.status_of("/resize/100/100/missing.jpg"), 404);
    assert_eq!(s.status_of("/resize/100/100/..%2Fsecret"), 400);
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
        s.status_of("/resize/100/100/missing.jpg"),
        404,
        "origin 404 passes through"
    );
    // Text served as an image is undecodable client input: 422 with a
    // message (LICENSE is a fixture-relative text file? use README).
    assert_eq!(s.status_of("/resize/100/100/list.txt"), 422);
}

#[test]
fn remote_source_mode_streams_from_http_origin() {
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
