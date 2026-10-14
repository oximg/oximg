//! Local S3 transport harness shared by integration tests and the standalone tool.
//! Checks signatures independently of the production signer. Replies can split,
//! delay or truncate any part of HTTP, including error bodies.

#![allow(dead_code)] // Each harness consumer uses a different subset.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// An HTTP response as timed writes. Missing bytes deliberately simulate EOF.
#[derive(Clone)]
pub struct Reply {
    parts: Vec<(Duration, Vec<u8>)>,
}

impl Reply {
    pub fn raw(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            parts: vec![(Duration::ZERO, bytes.into())],
        }
    }

    pub fn error(status: &str, code: &str) -> Self {
        Self::raw(s3_error(status, code))
    }

    pub fn photo() -> Self {
        let photo = include_bytes!("../fixtures/photo.jpg");
        let mut bytes = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            photo.len()
        )
        .into_bytes();
        bytes.extend_from_slice(photo);
        Self::raw(bytes)
    }

    /// Append another write after a delay. An empty write keeps the socket open.
    pub fn then(mut self, delay: Duration, bytes: impl Into<Vec<u8>>) -> Self {
        self.parts.push((delay, bytes.into()));
        self
    }

    pub fn delay_headers(mut self, delay: Duration) -> Self {
        self.parts[0].0 = delay;
        self
    }
}

pub fn s3_error(status: &str, code: &str) -> String {
    let body = format!("<Error><Code>{code}</Code><Message>test</Message></Error>");
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

struct Shutdown {
    stopped: Mutex<bool>,
    changed: Condvar,
}

impl Shutdown {
    fn wait(&self, delay: Duration) -> bool {
        let guard = self.stopped.lock().unwrap();
        *self
            .changed
            .wait_timeout_while(guard, delay, |stop| !*stop)
            .unwrap()
            .0
    }
}

/// Owns its listener and workers. Drop cancels delays and joins the threads.
pub struct S3 {
    port: u16,
    pub hits: Arc<AtomicUsize>,
    paths: Arc<Mutex<Vec<String>>>,
    shutdown: Arc<Shutdown>,
    worker: Option<JoinHandle<()>>,
}

impl S3 {
    pub fn spawn(route: impl Fn(&str, &str) -> Reply + Send + Sync + 'static) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let hits = Arc::new(AtomicUsize::new(0));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(Shutdown {
            stopped: Mutex::new(false),
            changed: Condvar::new(),
        });
        let (counter, seen, stop) = (hits.clone(), paths.clone(), shutdown.clone());
        let route = Arc::new(route);
        let worker = std::thread::spawn(move || {
            let mut workers: Vec<JoinHandle<()>> = Vec::new();
            while !stop.wait(Duration::from_millis(2)) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let (counter, seen, stop, route) =
                            (counter.clone(), seen.clone(), stop.clone(), route.clone());
                        workers.push(std::thread::spawn(move || {
                            let Ok(req) = read_request(&mut stream, &stop) else {
                                return;
                            };
                            counter.fetch_add(1, Ordering::SeqCst);
                            let path = req.split_whitespace().nth(1).unwrap_or("/");
                            seen.lock().unwrap().push(path.to_string());
                            let lower = req.to_ascii_lowercase();
                            let response = if signed_with_test_keys(&req) {
                                route(path, &lower)
                            } else {
                                Reply::error("403 Forbidden", "SignatureDoesNotMatch")
                            };
                            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                            for (delay, bytes) in response.parts {
                                if stop.wait(delay) || stream.write_all(&bytes).is_err() {
                                    break;
                                }
                            }
                        }));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }
                let mut i = 0;
                while i < workers.len() {
                    if workers[i].is_finished() {
                        workers.swap_remove(i).join().expect("S3 handler panicked");
                    } else {
                        i += 1;
                    }
                }
            }
            for worker in workers {
                worker.join().expect("S3 handler panicked");
            }
        });
        Ok(Self {
            port,
            hits,
            paths,
            shutdown,
            worker: Some(worker),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn requests(&self) -> Vec<String> {
        self.paths.lock().unwrap().clone()
    }
}

impl Drop for S3 {
    fn drop(&mut self) {
        *self.shutdown.stopped.lock().unwrap() = true;
        self.shutdown.changed.notify_all();
        if let Some(worker) = self.worker.take() {
            worker.join().expect("S3 listener panicked");
        }
    }
}

fn read_request(stream: &mut TcpStream, stop: &Shutdown) -> io::Result<String> {
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    let mut buf = [0; 1024];
    while bytes.len() < 64 * 1024 && Instant::now() < deadline && !stop.wait(Duration::ZERO) {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                bytes.extend_from_slice(&buf[..n]);
                if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                    return String::from_utf8(bytes)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "incomplete or oversized HTTP headers",
    ))
}

fn signed_with_test_keys(req: &str) -> bool {
    let lower = req.to_ascii_lowercase();
    let header = |name: &str| {
        lower.lines().find_map(|line| {
            line.split_once(':')
                .filter(|(n, _)| *n == name)
                .map(|(_, v)| v.trim())
        })
    };
    let signed = if header("x-amz-security-token").is_some() {
        "signedheaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token,"
    } else {
        "signedheaders=host;x-amz-content-sha256;x-amz-date,"
    };
    req.starts_with("GET ")
        && header("authorization").is_some_and(|auth| {
            auth.starts_with("aws4-hmac-sha256 credential=akidtest/")
                && auth.contains("/us-east-1/s3/aws4_request,")
                && auth.contains(signed)
        })
        && header("x-amz-content-sha256") == Some(EMPTY_SHA256)
        && sigv4_is_valid(req, "test-secret")
}

#[test]
fn fragmented_headers_are_read_before_rejecting_an_unsigned_request() {
    let s3 = S3::spawn(|_, _| panic!("unsigned request reached the fixture router")).unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", s3.port())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(b"GET /test-bucket/x HTTP/1.1\r\nHo")
        .unwrap();
    std::thread::sleep(Duration::from_millis(20));
    stream
        .write_all(b"st: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 403 Forbidden"), "{reply}");
    assert!(reply.contains("SignatureDoesNotMatch"));
    assert_eq!(s3.requests(), ["/test-bucket/x"]);
}

/// Check a SigV4 signature against the request as it arrived: the
/// request line and the signed headers on the wire. This is a second,
/// small implementation that does not call oximg's signer, so it fails
/// when the request we send differs from the request we signed (a path
/// encoded again, another `Host`). The AWS test vectors in
/// `src/pipeline/s3.rs` pin the algorithm itself.
fn sigv4_is_valid(req: &str, secret: &str) -> bool {
    use hmac::Mac;
    use hmac::digest::KeyInit;
    use sha2::{Digest, Sha256};
    let hmac = |key: &[u8], data: &str| {
        let mut mac = hmac::Hmac::<Sha256>::new_from_slice(key).unwrap();
        mac.update(data.as_bytes());
        mac.finalize().into_bytes().to_vec()
    };
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut lines = req.split("\r\n");
    let mut request_line = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(path)) = (request_line.next(), request_line.next()) else {
        return false;
    };
    let headers: Vec<(String, String)> = lines
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| {
            (
                n.trim().to_ascii_lowercase(),
                v.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect();
    let get = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    let (Some(auth), Some(datetime), Some(payload)) = (
        get("authorization"),
        get("x-amz-date"),
        get("x-amz-content-sha256"),
    ) else {
        return false;
    };
    let field = |name: &str| {
        auth.trim_start_matches("AWS4-HMAC-SHA256 ")
            .split(", ")
            .find_map(|f| f.strip_prefix(name))
    };
    let (Some(credential), Some(signed), Some(signature)) = (
        field("Credential="),
        field("SignedHeaders="),
        field("Signature="),
    ) else {
        return false;
    };
    let scope = credential.split_once('/').map_or("", |(_, scope)| scope);
    let names: Vec<&str> = signed.split(';').collect();
    if !scope.starts_with(datetime.get(..8).unwrap_or("-")) || !names.is_sorted() {
        return false;
    }
    let mut canonical_headers = String::new();
    for name in &names {
        let Some(value) = get(name) else { return false };
        canonical_headers.push_str(&format!("{name}:{value}\n"));
    }
    let canonical = format!("{method}\n{path}\n\n{canonical_headers}\n{signed}\n{payload}");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{datetime}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    // The scope is date/region/service/aws4_request, in signing order.
    let mut key = format!("AWS4{secret}").into_bytes();
    for part in scope.split('/') {
        key = hmac(&key, part);
    }
    hex(&hmac(&key, &string_to_sign)) == signature
}

/// The verifier must accept a signature that AWS published, or it
/// proves nothing about ours. This is the GET Object example from the
/// S3 API reference.
#[test]
fn sigv4_verifier_accepts_the_aws_example() {
    let req = "GET /test.txt HTTP/1.1\r\n\
        Host: examplebucket.s3.amazonaws.com\r\n\
        Authorization: AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
        SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
        Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41\r\n\
        Range: bytes=0-9\r\n\
        x-amz-content-sha256: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\r\n\
        x-amz-date: 20130524T000000Z\r\n\r\n";
    let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    assert!(sigv4_is_valid(req, secret));
    assert!(!sigv4_is_valid(
        &req.replace("/test.txt", "/test2.txt"),
        secret
    ));
    assert!(!sigv4_is_valid(req, "another-secret"));
}

/// The env every s3:// test starts from. Blank values read as unset,
/// so this also clears whatever the developer's shell has exported.
pub fn s3_env(endpoint_port: u16) -> Vec<(&'static str, String)> {
    vec![
        (
            "OXIMG_S3_ENDPOINT",
            format!("http://127.0.0.1:{endpoint_port}"),
        ),
        ("AWS_REGION", "us-east-1".into()),
        ("AWS_ACCESS_KEY_ID", "AKIDTEST".into()),
        ("AWS_SECRET_ACCESS_KEY", "test-secret".into()),
        ("AWS_SESSION_TOKEN", String::new()),
        ("OXIMG_S3_PATH_STYLE", String::new()),
    ]
}
