//! The library-level remote-source contract: `fetch_url`/`fetch_gcs`
//! (buffered, issue #22) and `process_url`/`process_gcs` (streaming),
//! exercised directly as an embedder would — the server binary stopped
//! calling the streaming pair in #22, so without this file they have
//! no coverage at all.
//!
//! This is the migration contract for any HTTP-client swap: the
//! classified `ErrorKind`s, byte-parity between the streaming and
//! buffered paths, and connection reuse across sequential fetches.
//!
//! Env caveat: the pipeline's knobs resolve once per process
//! (`config()` is a `OnceLock`), so every test funnels through
//! `init()`, which sets one compatible set of values before anything
//! touches the config. Server-binary tests don't have this constraint;
//! library tests do.

#![cfg(feature = "server")]

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use oximg::pipeline::{self, ErrorKind, Params};

fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // The gs:// seams are env-global too, so the fake metadata
        // server and the GCS-speaking origin must exist before their
        // addresses can be pinned into the environment.
        let md_port = metadata_server();
        let (gcs_port, _) = origin();
        // SAFETY: set before any config()/http_agent()/gcs access —
        // every test calls init() first, and Once blocks the racers
        // until the vars are in place.
        unsafe {
            // Admits the ~1.1MB photo.jpg fixture; the over-cap origin
            // streams past this.
            std::env::set_var("OXIMG_MAX_SOURCE_BYTES", "2000000");
            // Keeps the stalled-origin test at seconds, not the 30s
            // default.
            std::env::set_var("OXIMG_UPSTREAM_TIMEOUT", "2");
            std::env::set_var("GCE_METADATA_HOST", format!("127.0.0.1:{md_port}"));
            std::env::set_var("OXIMG_GCS_ENDPOINT", format!("http://127.0.0.1:{gcs_port}"));
        }
    });
}

/// Minimal metadata-token endpoint (the same contract
/// tests/server.rs's fake asserts in full: that suite owns header and
/// path verification; this one just mints tokens).
fn metadata_server() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let body = r#"{"access_token":"remote-api-token","expires_in":3600,"token_type":"Bearer"}"#;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    port
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// A keep-alive origin that counts accepted connections: serves
/// fixtures by name, plus the failure shapes the contract needs. Each
/// connection handles requests in a loop (no `Connection: close`), so
/// reuse is observable as connections < requests.
fn origin() -> (u16, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let conns = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&conns);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            counter.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                loop {
                    let mut buf = [0u8; 2048];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .trim_start_matches('/')
                        .to_string();
                    // The same origin also answers gs:// fetches
                    // ({endpoint}/{bucket}/{key}): drop the bucket
                    // segment and serve the key like any other path.
                    let path = path
                        .strip_prefix("test-bucket/")
                        .map(str::to_string)
                        .unwrap_or(path);
                    if path.starts_with("stall") {
                        std::thread::sleep(std::time::Duration::from_secs(10));
                        return;
                    }
                    if path.starts_with("moved") {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/x\r\nContent-Length: 0\r\n\r\n"
                        );
                        continue;
                    }
                    if path.starts_with("endless") {
                        // Chunked, no Content-Length, streams past any
                        // cap: only the mid-body byte count refuses it.
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
                        );
                        let chunk = [0xAAu8; 65536];
                        for _ in 0..64 {
                            if write!(stream, "{:x}\r\n", chunk.len()).is_err()
                                || stream.write_all(&chunk).is_err()
                                || write!(stream, "\r\n").is_err()
                            {
                                return;
                            }
                        }
                        let _ = write!(stream, "0\r\n\r\n");
                        continue;
                    }
                    match std::fs::read(format!(
                        "{}/tests/fixtures/{path}",
                        env!("CARGO_MANIFEST_DIR")
                    )) {
                        Ok(data) => {
                            let _ = write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                                data.len()
                            );
                            let _ = stream.write_all(&data);
                        }
                        Err(_) => {
                            let _ = write!(
                                stream,
                                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"
                            );
                        }
                    }
                }
            });
        }
    });
    (port, conns)
}

/// The buffered fetch returns the origin's bytes verbatim, and the
/// streaming path produces byte-identical output to buffering-then-
/// processing — the parity that let issue #22 swap the server from one
/// to the other without a behavior change.
#[test]
fn buffered_and_streaming_paths_agree() {
    init();
    let (port, _) = origin();
    let base = format!("http://127.0.0.1:{port}");

    let fetched = pipeline::fetch_url(&format!("{base}/photo.jpg")).unwrap();
    assert_eq!(
        fetched,
        fixture("photo.jpg"),
        "fetch_url returns the source verbatim"
    );

    let p = Params {
        max_width: 100,
        max_height: 100,
        ..Params::default()
    };
    let (streamed, fmt_a) = pipeline::process_url(&format!("{base}/photo.jpg"), &p).unwrap();
    let (buffered, fmt_b) = pipeline::process(&fetched, &p).unwrap();
    assert_eq!(fmt_a, fmt_b);
    assert_eq!(
        streamed, buffered,
        "streaming and buffered decode must be byte-identical"
    );
}

/// The classified kinds an embedder matches on, produced through the
/// public API — not through Error::classify unit tests, which cannot
/// see what error shapes the HTTP client actually emits.
#[test]
fn fetch_error_kinds_survive_the_public_api() {
    init();
    let (port, _) = origin();
    let base = format!("http://127.0.0.1:{port}");

    let kind = |path: &str| {
        pipeline::fetch_url(&format!("{base}/{path}"))
            .unwrap_err()
            .kind()
    };
    assert_eq!(kind("missing.jpg"), ErrorKind::SourceNotFound);
    assert_eq!(
        kind("moved.jpg"),
        ErrorKind::Upstream,
        "redirects are refused"
    );
    assert_eq!(
        kind("endless.jpg"),
        ErrorKind::SourceTooLarge,
        "the cap must hold without a Content-Length to precheck"
    );
    assert_eq!(kind("stall.jpg"), ErrorKind::UpstreamTimeout);

    // The streaming variant classifies identically.
    let p = Params::default();
    let e = pipeline::process_url(&format!("{base}/missing.jpg"), &p).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::SourceNotFound);

    // A connection-refused target is the upstream's fault, not bad
    // client input (no origin listens on the reserved port 1).
    let e = pipeline::fetch_url("http://127.0.0.1:1/x.jpg").unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Upstream);
}

/// The gs:// pair rides the same tails as the HTTP pair (same caps,
/// same hand-off), differing only in auth and status mapping — which
/// tests/server.rs pins end-to-end. Here: the library-level surface
/// returns the object verbatim, streaming/buffered parity holds, and
/// an absent object classifies as SourceNotFound.
#[test]
fn gcs_paths_share_the_http_contract() {
    init();
    let bytes = pipeline::fetch_gcs("test-bucket", "photo.jpg").unwrap();
    assert_eq!(
        bytes,
        fixture("photo.jpg"),
        "fetch_gcs returns the object verbatim"
    );

    let p = Params {
        max_width: 100,
        max_height: 100,
        ..Params::default()
    };
    let (streamed, _) = pipeline::process_gcs("test-bucket", "photo.jpg", &p).unwrap();
    let (buffered, _) = pipeline::process(&bytes, &p).unwrap();
    assert_eq!(
        streamed, buffered,
        "gs:// streaming and buffered decode agree"
    );

    let e = pipeline::fetch_gcs("test-bucket", "missing.jpg").unwrap_err();
    assert_eq!(e.kind(), ErrorKind::SourceNotFound);
}

/// Sequential fetches to one host reuse a pooled connection. Pinned
/// because connection churn is a real cost against TLS origins
/// (~2 RTT + crypto per new connection), and pooling behavior is
/// exactly the kind of thing that silently changes in a client swap.
#[test]
fn sequential_fetches_reuse_the_connection() {
    init();
    let (port, conns) = origin();
    let base = format!("http://127.0.0.1:{port}");

    for _ in 0..4 {
        pipeline::fetch_url(&format!("{base}/photo.jpg")).unwrap();
    }
    assert_eq!(
        conns.load(Ordering::SeqCst),
        1,
        "4 sequential fetches must share one pooled connection"
    );
}
