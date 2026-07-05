//! End-to-end HTTP tests: spawn the real server binary and exercise the
//! full request path, including content types, error mapping, request
//! coalescing, URL signing, and the remote-source mode.

use std::io::Read;
use std::process::{Child, Command};

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start(port: u16, envs: &[(&str, String)]) -> Server {
        let fixtures = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximg"));
        cmd.env("PORT", port.to_string())
            .env("IMAGES_DIR", fixtures)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawn oximg");
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
    let s = Server::start(47101, &[]);
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
    let s = Server::start(47106, &[]);
    let (status, ct, body) = s.get("/resize/100/100/photo.avif").unwrap();
    assert_eq!(status, 200);
    assert_eq!(ct, "image/avif");
    let (_, w, h) = oximg::pipeline::probe(&body).unwrap();
    assert_eq!((w, h), (100, 75));
}

#[test]
fn error_mapping() {
    let s = Server::start(47102, &[]);
    assert_eq!(s.status_of("/resize/0/100/photo.jpg"), 400);
    assert_eq!(s.status_of("/resize/9000/9000/photo.jpg"), 400);
    assert_eq!(s.status_of("/resize/100/100/missing.jpg"), 404);
    assert_eq!(s.status_of("/resize/100/100/..%2Fsecret"), 400);
}

#[test]
fn concurrent_identical_requests_coalesce_to_identical_bytes() {
    let s = Server::start(47103, &[]);
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

#[test]
fn signing_gate() {
    let key = "deadbeef".repeat(8);
    let salt = "cafebabe".repeat(8);
    let s = Server::start(47104, &[("OXIMG_KEY", key), ("OXIMG_SALT", salt)]);
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
    let s = Server::start(47107, &[]);
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
    let s = Server::start(47108, &[]);
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
    let s = Server::start(47109, &[("OXIMG_KEY", key), ("OXIMG_SALT", salt)]);
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
    let s = Server::start(47110, &[]);
    let (_, ct, vary, _) = s
        .get_accept("/resize/100/100/photo.jpg", Some("image/webp,*/*"))
        .unwrap();
    assert_eq!(ct, "image/jpeg", "negotiation must be opt-in");
    assert_eq!(vary, None, "no Vary when negotiation is off");

    // Negotiation on: Accept steers the format; Vary is emitted on
    // every response (config-static), including non-negotiated ones.
    let s = Server::start(47111, &[("OXIMG_AUTO_FORMAT", "webp".into())]);
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
    let s = Server::start(47112, &[]);
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
    let fused = Server::start(47113, &[("OXIMG_OVERLAP", "1".into())]);
    let serial = Server::start(47114, &[("OXIMG_OVERLAP", "0".into())]);
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

/// mozjpeg presets fuse the decode with the resize on a second thread;
/// like every fused path, their bytes must not depend on the overlap
/// gate.
#[test]
fn preset_bytes_do_not_depend_on_overlap_gate() {
    for (port_a, port_b, preset) in [(47117, 47118, "fast"), (47119, 47120, "small")] {
        let fused = Server::start(
            port_a,
            &[("OXIMG_OVERLAP", "1".into()), ("PRESET", preset.into())],
        );
        let serial = Server::start(
            port_b,
            &[("OXIMG_OVERLAP", "0".into()), ("PRESET", preset.into())],
        );
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
    let fused = Server::start(47115, &[("OXIMG_OVERLAP", "1".into()), fir.clone()]);
    let serial = Server::start(47116, &[("OXIMG_OVERLAP", "0".into()), fir]);
    for url in ["/resize/100/100/photo.jpg@png", "/resize/100/100/photo.jpg"] {
        let a = fused.get(url).unwrap().2;
        let b = serial.get(url).unwrap().2;
        assert_eq!(a, b, "{url}: bytes must not depend on the overlap gate");
    }
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

    let s = Server::start(
        47105,
        &[(
            "OXIMG_SOURCE_BASE_URL",
            format!("http://127.0.0.1:{origin_port}"),
        )],
    );
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
