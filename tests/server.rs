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
    // origin 404 passes through
    assert_eq!(s.status_of("/resize/100/100/nope.jpg"), 404);
}
