use oximg::pipeline;

use std::collections::HashMap;
use std::path::PathBuf;

// glibc's per-thread arenas inflate RSS several-fold on Linux under a
// multi-threaded allocation-heavy load; mimalloc returns memory promptly
// and behaves consistently across threads.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::{HeaderValue, StatusCode, header, request::Parts};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::sync::{Semaphore, watch};

use oximg::pipeline::ImageFormat;

// The format is the one *resolved* before keying (explicit @fmt token,
// else Accept negotiation, else None = source format), never the raw
// Accept header — so cardinality stays bounded and negotiated requests
// coalesce with explicit ones. The filename is token-stripped. Known
// boundary: None never coalesces with Some(X) even when X is the actual
// source format — the source format is untrusted before sniffing, so
// merging them pre-sniff would mislabel Content-Types; cost is capped
// at one extra flight per hot (w, h, file).
type FlightKey = (u32, u32, String, Option<ImageFormat>);
type FlightResult = Result<(Bytes, &'static str), (StatusCode, String)>;
type FlightMap = Mutex<HashMap<FlightKey, watch::Receiver<Option<FlightResult>>>>;

#[derive(Clone)]
struct App {
    /// OXIMG_LOG=request also logs successes; failures always log.
    log_requests: bool,
    images_dir: Arc<PathBuf>,
    // When set (OXIMG_SOURCE_BASE_URL), sources are fetched from
    // `<base>/<file>` over HTTP instead of the local filesystem. The base
    // is operator-configured, so user input never chooses the host (no
    // SSRF surface).
    source_base: Option<Arc<str>>,
    cpu_slots: Arc<Semaphore>,
    quality: f32,
    encoder: pipeline::Encoder,
    resize_threads: usize,
    // Singleflight: concurrent identical requests are processed once and
    // share the result, absorbing cache stampedes on hot images.
    inflight: Arc<FlightMap>,
    signing: Option<Arc<Signing>>,
    // OXIMG_AUTO_FORMAT preference order for Accept negotiation; empty =
    // negotiation off (and no Vary header, exactly the pre-feature
    // response shape).
    auto_format: Arc<[ImageFormat]>,
}

/// imgproxy-style URL signing: base64url(HMAC-SHA256(key, salt || path)),
/// with key and salt supplied hex-encoded. When configured, only
/// /{signature}/resize/... URLs are served.
#[derive(Clone)]
struct Signing {
    key: Vec<u8>,
    salt: Vec<u8>,
}

impl Signing {
    /// A security knob must fail closed: any set-but-undecodable
    /// key/salt is a fatal configuration error, never a silently
    /// unsigned server. Unset or empty values mean "signing off".
    fn from_env() -> Result<Option<Self>, String> {
        Self::from_values(
            std::env::var("OXIMG_KEY").ok().as_deref(),
            std::env::var("OXIMG_SALT").ok().as_deref(),
        )
    }

    fn from_values(key: Option<&str>, salt: Option<&str>) -> Result<Option<Self>, String> {
        fn decode(name: &str, v: Option<&str>) -> Result<Option<Vec<u8>>, String> {
            let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) else {
                return Ok(None);
            };
            if v.len() % 2 != 0 {
                return Err(format!("{name} is not valid hex (odd length)"));
            }
            (0..v.len())
                .step_by(2)
                .map(|i| {
                    u8::from_str_radix(&v[i..i + 2], 16)
                        .map_err(|_| format!("{name} is not valid hex"))
                })
                .collect::<Result<Vec<u8>, String>>()
                .map(Some)
        }
        match (decode("OXIMG_KEY", key)?, decode("OXIMG_SALT", salt)?) {
            (Some(key), Some(salt)) => Ok(Some(Signing { key, salt })),
            (None, None) => Ok(None),
            _ => Err("OXIMG_KEY and OXIMG_SALT must both be set to enable signing".into()),
        }
    }

    fn verify(&self, signature: &str, path: &str) -> bool {
        use hmac::Mac;
        use hmac::digest::KeyInit;
        let Ok(mut mac) = hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.key) else {
            return false;
        };
        mac.update(&self.salt);
        mac.update(path.as_bytes());
        let Some(sig) = base64url_decode(signature) else {
            return false;
        };
        mac.verify_slice(&sig).is_ok()
    }
}

fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut rev = [255u8; 256];
    for (i, &c) in ALPHABET.iter().enumerate() {
        rev[c as usize] = i as u8;
    }
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        let v = rev[c as usize];
        if v == 255 {
            return None;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Startup setting: unset means the default, set-but-unparseable is a
/// fatal configuration error (fail closed, like the signing config).
fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    match std::env::var(key) {
        Err(_) => default,
        Ok(v) if v.trim().is_empty() => default,
        Ok(v) => v.trim().parse().unwrap_or_else(|_| {
            eprintln!("oximg: fatal: {key}={v:?} is not a valid value");
            std::process::exit(2);
        }),
    }
}

mod cli;

fn main() -> anyhow::Result<()> {
    // Minimal, dependency-free subcommand dispatch. `serve` is the
    // default — bare `oximg` keeps every existing deployment and the
    // Docker CMD working; the server takes all its real configuration
    // from the environment. `resize`/`probe` are the one-shot CLI over
    // the same pipeline.
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("serve") => {
            if args.len() > 1 {
                eprintln!(
                    "oximg: serve takes no arguments (configuration is via \
                     environment variables; try --help)"
                );
                std::process::exit(2);
            }
        }
        Some("resize") => return cli::resize(&args[1..]),
        Some("probe") => return cli::probe(&args[1..]),
        Some("-V" | "--version") => {
            println!("oximg {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("-h" | "--help") => {
            cli::print_help();
            return Ok(());
        }
        Some(other) => {
            eprintln!("oximg: unknown command {other:?} (try --help)");
            std::process::exit(2);
        }
    }
    if let Err(e) = oximg::config_validate() {
        eprintln!("oximg: fatal: {e}");
        std::process::exit(2);
    }
    let workers = std::thread::available_parallelism()?.get();
    // Cap the blocking pool at CPU slots + a little IO headroom: this
    // bounds the number of thread-local scratch copies (tokio's default of
    // 512 threads would multiply RSS).
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(workers + 4)
        .build()?
        .block_on(async_main(workers))
}

async fn async_main(workers: usize) -> anyhow::Result<()> {
    let port: u16 = env_or("PORT", 8081);
    let images_dir =
        PathBuf::from(std::env::var("IMAGES_DIR").unwrap_or_else(|_| "./images".to_string()));

    let app = App {
        images_dir: Arc::new(images_dir.clone()),
        source_base: std::env::var("OXIMG_SOURCE_BASE_URL")
            .ok()
            .map(|s| Arc::from(s.trim_end_matches('/'))),
        cpu_slots: Arc::new(Semaphore::new(workers)),
        quality: env_or("QUALITY", 80.0),
        encoder: pipeline::Encoder::from_preset(
            std::env::var("PRESET").as_deref().unwrap_or("jpegli"),
        ),
        resize_threads: env_or("OXIMG_PAR", 1),
        inflight: Arc::new(Mutex::new(HashMap::new())),
        log_requests: std::env::var("OXIMG_LOG").as_deref() == Ok("request"),
        signing: Signing::from_env()
            .unwrap_or_else(|e| {
                eprintln!("oximg: fatal: {e}");
                std::process::exit(2);
            })
            .map(Arc::new),
        auto_format: auto_format_from_env().into(),
    };
    if app.signing.is_some() {
        eprintln!("oximg: URL signing enabled");
    }
    if !app.auto_format.is_empty() {
        eprintln!(
            "oximg: Accept negotiation enabled ({})",
            app.auto_format
                .iter()
                .map(|f| f.content_type())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let router = Router::new()
        .route("/health", get(async || "ok"))
        // {*file} spans path separators, so sources organized in
        // directories (IMAGES_DIR trees, S3-style prefixes behind
        // OXIMG_SOURCE_BASE_URL) are addressable; validate_source_path
        // guards what the wider capture lets in.
        .route("/resize/{w}/{h}/{*file}", get(handle_resize))
        .route("/{sig}/resize/{w}/{h}/{*file}", get(handle_signed_resize))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    // Report the *bound* port: PORT=0 asks the OS for a free one (the
    // test harness relies on this line to discover it).
    let bound = listener.local_addr()?.port();
    eprintln!(
        "oximg listening on :{bound} (images: {}, workers: {workers})",
        images_dir.display()
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    eprintln!("oximg: shutdown complete");
    Ok(())
}

/// Resolves when the process is asked to stop: SIGTERM (what `docker
/// stop`, Kubernetes, and Cloud Run send) or SIGINT (terminal ctrl-C).
/// axum then stops accepting connections, finishes in-flight requests,
/// and `serve` returns for a clean exit 0. No drain timeout of our own:
/// every orchestrator escalates to SIGKILL after its grace period
/// (docker stop and Cloud Run 10s, Kubernetes
/// terminationGracePeriodSeconds), which backstops a response that
/// never finishes.
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    let name = tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
    };
    eprintln!("oximg: {name} received, draining in-flight requests");
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("install ctrl-C handler");
    eprintln!("oximg: ctrl-C received, draining in-flight requests");
}

/// OXIMG_AUTO_FORMAT: comma-separated output formats to negotiate from
/// the Accept header, in preference order (e.g. "avif,webp"). Unknown
/// or build-unavailable entries are skipped with a warning so one
/// config works across builds.
fn auto_format_from_env() -> Vec<ImageFormat> {
    let Ok(list) = std::env::var("OXIMG_AUTO_FORMAT") else {
        return Vec::new();
    };
    list.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .filter_map(|t| {
            let fmt = ImageFormat::from_token(t);
            match fmt {
                Some(ImageFormat::Avif) if cfg!(not(feature = "avif")) => {
                    eprintln!("oximg: OXIMG_AUTO_FORMAT: avif not enabled in this build; skipped");
                    None
                }
                Some(f) => Some(f),
                None => {
                    eprintln!("oximg: OXIMG_AUTO_FORMAT: unknown format {t:?}; skipped");
                    None
                }
            }
        })
        .collect()
}

/// The request's Accept value, cloned by itself so the hot path never
/// clones the whole header map.
struct AcceptHeader(Option<HeaderValue>);

impl<S: Send + Sync> FromRequestParts<S> for AcceptHeader {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(AcceptHeader(parts.headers.get(header::ACCEPT).cloned()))
    }
}

async fn handle_signed_resize(
    State(app): State<App>,
    Path((sig, w, h, file)): Path<(String, u32, u32, String)>,
    accept: AcceptHeader,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let Some(signing) = app.signing.as_ref() else {
        return Err((StatusCode::NOT_FOUND, "signing not configured".into()));
    };
    // Signed material is the raw file capture, so an explicit @fmt token
    // is covered by the signature: photo.jpg's signature does not
    // authorize photo.jpg@avif and its heavier encode. The capture spans
    // path separators; the canonical form clients must sign is the
    // percent-DECODED multi-segment path.
    let path = format!("/resize/{w}/{h}/{file}");
    if !signing.verify(&sig, &path) {
        return Err((StatusCode::FORBIDDEN, "invalid signature".into()));
    }
    serve_resize(app, w, h, file, accept).await
}

async fn handle_resize(
    State(app): State<App>,
    Path((w, h, file)): Path<(u32, u32, String)>,
    accept: AcceptHeader,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    if app.signing.is_some() {
        return Err((StatusCode::FORBIDDEN, "signature required".into()));
    }
    serve_resize(app, w, h, file, accept).await
}

/// The source path is client input spanning multiple segments (already
/// percent-decoded by the extractor), and it flows into a filesystem
/// join or an upstream URL — so validate component-wise: every
/// `/`-separated component must be a plain name. Rejecting empty
/// components refuses absolute paths, `//` (which an upstream would
/// read as a protocol-relative authority), and trailing slashes;
/// rejecting `.`/`..` components refuses traversal in both the
/// filesystem and the URL sense (interior dots like `my..file.jpg` are
/// fine — the old substring check was coarser). `\`, `?`, `#`, and
/// control bytes have no place in a source name in any mode.
fn validate_source_path(file: &str) -> Result<(), (StatusCode, String)> {
    let bad_component = |c: &str| c.is_empty() || c == "." || c == "..";
    if file.contains(['\\', '?', '#'])
        || file.bytes().any(|b| b < 0x20 || b == 0x7f)
        || file.split('/').any(bad_component)
    {
        return Err((StatusCode::BAD_REQUEST, "invalid source path".into()));
    }
    Ok(())
}

/// Split a trailing imgproxy-style `@{fmt}` output-format token off the
/// filename. Only exact known tokens count — any other suffix is part
/// of the filename (`photo@2x.jpg` keeps working; a file literally
/// named `x.jpg@webp` becomes unreachable, a documented trade). "jxl"
/// is reserved so the future encoder slots in with a clear error today.
/// Only the last path segment is considered: a `@` in a directory name
/// is never a token by design, not just because its "token" would
/// contain `/`.
fn split_format(file: &str) -> Result<(&str, Option<ImageFormat>), (StatusCode, String)> {
    let last_start = file.rfind('/').map_or(0, |i| i + 1);
    let Some((seg_base, token)) = file[last_start..].rsplit_once('@') else {
        return Ok((file, None));
    };
    if seg_base.is_empty() {
        return Ok((file, None));
    }
    let base = &file[..last_start + seg_base.len()];
    match ImageFormat::from_token(token) {
        Some(ImageFormat::Avif) if cfg!(not(feature = "avif")) => Err((
            StatusCode::BAD_REQUEST,
            "avif output is not enabled in this build".into(),
        )),
        Some(fmt) => Ok((base, Some(fmt))),
        None if token == "jxl" => Err((
            StatusCode::BAD_REQUEST,
            "jxl output is not supported in this build".into(),
        )),
        None => Ok((file, None)),
    }
}

/// First OXIMG_AUTO_FORMAT entry the Accept header names. Substring
/// match without q-value parsing — the imgproxy/imagor de-facto
/// standard, and allocation-free. With negotiation off (the default),
/// the header is never even scanned.
fn negotiate(auto: &[ImageFormat], accept: &AcceptHeader) -> Option<ImageFormat> {
    if auto.is_empty() {
        return None;
    }
    let accept = accept.0.as_ref()?.to_str().ok()?;
    auto.iter()
        .copied()
        .find(|f| accept.contains(f.content_type()))
}

/// Logging wrapper: one structured stderr line per failure (always)
/// or per request (OXIMG_LOG=request), with a process-unique id so
/// concurrent requests interleave legibly.
async fn serve_resize(
    app: App,
    w: u32,
    h: u32,
    file: String,
    accept: AcceptHeader,
) -> Result<Response, (StatusCode, String)> {
    static REQ_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let req = REQ_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let log_requests = app.log_requests;
    let t0 = std::time::Instant::now();
    let path = format!("/resize/{w}/{h}/{file}");
    let result = serve_resize_inner(app, w, h, file, accept).await;
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    match &result {
        Err((status, msg)) => {
            eprintln!("oximg: req={req} status={status} ms={ms:.1} path={path:?} err={msg:?}");
        }
        Ok(_) if log_requests => {
            eprintln!("oximg: req={req} status=200 ms={ms:.1} path={path:?}");
        }
        Ok(_) => {}
    }
    result
}

async fn serve_resize_inner(
    app: App,
    w: u32,
    h: u32,
    file: String,
    accept: AcceptHeader,
) -> Result<Response, (StatusCode, String)> {
    // 0 on one axis means "unconstrained": /resize/750/0/... is
    // width-only (height follows the aspect ratio), the reverse is
    // height-only. This replaces the sentinel-height workaround
    // (h=8192), which silently narrowed sources taller than the
    // sentinel's aspect ratio — corrupting srcset width descriptors.
    // Both axes zero stays an error: "no constraint at all" is not a
    // resize request.
    if (w == 0 && h == 0) || w > 8192 || h > 8192 {
        return Err((StatusCode::BAD_REQUEST, "invalid dimensions".into()));
    }
    validate_source_path(&file)?;
    let (base, explicit) = split_format(&file)?;
    // Precedence: explicit @fmt > Accept negotiation > source format.
    let target = explicit.or_else(|| negotiate(&app.auto_format, &accept));
    let vary_accept = !app.auto_format.is_empty();

    // base is always a prefix of file, so truncating in place moves the
    // already-owned String into the key — no allocation on the bare-URL
    // path (which strips nothing).
    let base_len = base.len();
    let mut file = file;
    file.truncate(base_len);
    let (out, content_type) = singleflight(&app, (w, h, file, target)).await?;
    let headers = [
        (header::CONTENT_TYPE, content_type),
        (header::CACHE_CONTROL, "public, max-age=31536000"),
    ];
    // Vary is config-static — emitted on every 200 whenever negotiation
    // is enabled, including explicit-@fmt and non-negotiated outcomes.
    // Outcome-conditional Vary poisons CDN caches under the 1-year
    // max-age (a served no-Vary response is cached for all Accepts).
    if vary_accept {
        Ok((headers, [(header::VARY, "Accept")], out).into_response())
    } else {
        Ok((headers, out).into_response())
    }
}

/// Removes the in-flight map entry when dropped, so a cancelled leader
/// (client disconnect drops the handler future mid-await) can never leave
/// a stale entry that would strand followers.
struct FlightGuard {
    map: Arc<FlightMap>,
    key: FlightKey,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        // A poisoned lock only means another request panicked while
        // holding it; the map itself (URL -> leader slot) stays
        // structurally sound, so clean up rather than panicking inside
        // a Drop — which during unwind would abort the whole process.
        let mut map = match self.map.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        map.remove(&self.key);
    }
}

/// Process the request, coalescing concurrent duplicates: the first caller
/// (leader) runs the pipeline; followers await its watch channel and share
/// the resulting `Bytes` (O(1) clone). If a leader dies without publishing
/// (panic/cancel), the channel closes and followers retry for leadership.
async fn singleflight(app: &App, key: FlightKey) -> FlightResult {
    for _ in 0..3 {
        let leader_tx = {
            let mut map = match app.inflight.lock() {
                Ok(g) => g,
                // See FlightGuard::drop: the map survives a poisoning
                // panic intact; refusing every future request over one
                // is strictly worse.
                Err(poisoned) => poisoned.into_inner(),
            };
            match map.get(&key) {
                Some(rx) => Err(rx.clone()),
                None => {
                    let (tx, rx) = watch::channel(None);
                    map.insert(key.clone(), rx);
                    Ok(tx)
                }
            }
        };
        match leader_tx {
            Ok(tx) => {
                let guard = FlightGuard {
                    map: Arc::clone(&app.inflight),
                    key: key.clone(),
                };
                let result = process_one(app, &key).await;
                // Remove the entry before publishing so late arrivals start
                // fresh instead of reading a stale channel.
                drop(guard);
                tx.send_replace(Some(result.clone()));
                return result;
            }
            Err(mut rx) => loop {
                if let Some(result) = rx.borrow_and_update().as_ref() {
                    return result.clone();
                }
                if rx.changed().await.is_err() {
                    break; // leader died before publishing; retry for leadership
                }
            },
        }
    }
    Err((
        StatusCode::SERVICE_UNAVAILABLE,
        "request coalescing failed repeatedly".into(),
    ))
}

/// Re-encode the (extractor-decoded) source path for the upstream URL.
/// Bytes outside RFC 3986 pchar are percent-encoded and `/` is kept as
/// the separator — so the origin sees exactly the segments the client
/// addressed. Crucially `%` itself is encoded: the upstream applies its
/// own decode, and a raw pass-through would let a double-encoded
/// `%252e%252e` arrive there as `..` (a traversal the component checks
/// on the decoded form cannot see).
fn encode_upstream_path(file: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(file.len());
    for &b in file.as_bytes() {
        let pchar = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b':'
                    | b'@'
                    | b'/'
            );
        if pchar {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xf) as usize] as char);
        }
    }
    out
}

/// Defense in depth for the filesystem mode: component validation
/// already makes the joined path lexically inescapable, so this only
/// fires when a symlink inside IMAGES_DIR points outside it — refused
/// as 404 (indistinguishable from absent, revealing nothing). A path
/// that fails to resolve falls through to the pipeline, whose open()
/// classifies it (404/500) with a proper context chain.
fn verify_within_root(
    root: &std::path::Path,
    path: &std::path::Path,
) -> Result<(), (StatusCode, String)> {
    let (Ok(resolved), Ok(root)) = (path.canonicalize(), root.canonicalize()) else {
        return Ok(());
    };
    if resolved.starts_with(&root) {
        Ok(())
    } else {
        Err((StatusCode::NOT_FOUND, "image not found".into()))
    }
}

async fn process_one(app: &App, key: &FlightKey) -> FlightResult {
    let (w, h, file, output) = key;
    let path = app.images_dir.join(file);

    // CPU concurrency cap = core count; queueing happens here instead of
    // flooding the blocking pool.
    let permit = app
        .cpu_slots
        .clone()
        .acquire_owned()
        .await
        .expect("semaphore closed");

    // URL 0 = unconstrained axis; the library spelling for that is
    // u32::MAX (Params::default's "no downscale bound"). The output
    // stays bounded by the source's own dimensions (the pipeline never
    // enlarges) and by the decode-time pixel caps.
    let unbounded = |d: u32| if d == 0 { u32::MAX } else { d };
    let params = pipeline::Params {
        max_width: unbounded(*w),
        max_height: unbounded(*h),
        quality: app.quality,
        encoder: app.encoder,
        // The resize stage may briefly fan out into row bands without
        // taking semaphore slots — resize is only ~1/4 of request time, so
        // average oversubscription stays <30% in exchange for lower
        // light-load latency.
        parallel: app.resize_threads,
        output: *output,
        // Override fields stay None: the server's knobs are the
        // process-global OXIMG_* environment, validated at startup.
        ..Default::default()
    };
    let source_url = app
        .source_base
        .as_ref()
        .map(|base| format!("{base}/{}", encode_upstream_path(file)));
    let images_root = Arc::clone(&app.images_dir);
    // The explicit return type pins `?`'s error to (StatusCode, String)
    // — a dependency's blanket From impls otherwise make the inference
    // ambiguous here.
    type Processed = Result<(Vec<u8>, ImageFormat), pipeline::Error>;
    let out = tokio::task::spawn_blocking(move || -> Result<Processed, (StatusCode, String)> {
        let _permit = permit; // hold the CPU slot for the whole processing
        // Streaming decode: the source is never buffered whole on the heap
        // (saves concurrency x file-size for large sources under load);
        // for remote sources decoding overlaps the download.
        match source_url {
            Some(url) => Ok(pipeline::process_url(&url, &params)),
            None => {
                verify_within_root(&images_root, &path)?;
                Ok(pipeline::process_path(&path, &params))
            }
        }
    })
    .await
    .map_err(|e| {
        eprintln!("oximg: error status=500 file={file:?} panic={e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "image processing failed".to_string(),
        )
    })??
    // The pipeline classifies its own failures (pipeline::ErrorKind);
    // this match only assigns statuses. Faults on our side (unreadable
    // source, upstream, internal) answer with generic bodies — the
    // detail (full context chain, {e:#}) goes to stderr, where an
    // operator can see it, instead of to the client. Undecodable client
    // input returns its top-level message, which is safe and useful.
    .map_err(|e| {
        use pipeline::ErrorKind;
        match e.kind() {
            ErrorKind::SourceNotFound => (StatusCode::NOT_FOUND, "image not found".to_string()),
            ErrorKind::SourceTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "source image exceeds the configured size limit".to_string(),
            ),
            ErrorKind::SourceUnreadable => {
                eprintln!("oximg: error status=500 file={file:?} err={e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "source could not be read".to_string(),
                )
            }
            ErrorKind::Upstream => {
                eprintln!("oximg: error status=502 file={file:?} err={e:#}");
                (
                    StatusCode::BAD_GATEWAY,
                    "upstream image fetch failed".to_string(),
                )
            }
            ErrorKind::Internal => {
                eprintln!("oximg: error status=500 file={file:?} err={e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal image-processing error".to_string(),
                )
            }
            ErrorKind::Undecodable => (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
            // ErrorKind is #[non_exhaustive] (the binary is a consumer
            // of the library crate like any embedder): treat kinds this
            // binary predates as internal faults — log, generic body.
            _ => {
                eprintln!("oximg: error status=500 file={file:?} err={e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal image-processing error".to_string(),
                )
            }
        }
    })?;

    let (bytes, format) = out;
    Ok((Bytes::from(bytes), format.content_type()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_decodes_known_vectors() {
        assert_eq!(
            base64url_decode("aGVsbG8").as_deref(),
            Some(b"hello".as_slice())
        );
        assert_eq!(
            base64url_decode("aGVsbG8=").as_deref(),
            Some(b"hello".as_slice())
        );
        // '-' and '_' are the URL-safe substitutions for '+' and '/'
        assert_eq!(
            base64url_decode("-_8").as_deref(),
            Some([0xfb, 0xff].as_slice())
        );
        assert_eq!(base64url_decode("bad!"), None);
    }

    fn test_signing() -> Signing {
        let hex = |s: &str| -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        };
        Signing {
            key: hex(&"deadbeef".repeat(8)),
            salt: hex(&"cafebabe".repeat(8)),
        }
    }

    /// Every from_values state: signing on, off, and — the security
    /// property — fail-closed on anything set but undecodable.
    #[test]
    fn signing_config_fails_closed() {
        // both valid → on
        assert!(
            Signing::from_values(Some("deadbeef"), Some("cafebabe"))
                .unwrap()
                .is_some()
        );
        // both unset (or set-but-empty/whitespace) → off
        assert!(Signing::from_values(None, None).unwrap().is_none());
        assert!(
            Signing::from_values(Some(""), Some("  "))
                .unwrap()
                .is_none()
        );
        // undecodable values must be fatal, not silently unsigned
        assert!(Signing::from_values(Some("xyz!"), Some("cafebabe")).is_err());
        assert!(Signing::from_values(Some("abc"), Some("cafebabe")).is_err()); // odd length
        assert!(Signing::from_values(Some("xyz!"), Some("also-bad")).is_err());
        // half-configured is fatal too
        assert!(Signing::from_values(Some("deadbeef"), None).is_err());
        assert!(Signing::from_values(None, Some("cafebabe")).is_err());
    }

    #[test]
    fn signature_verifies_precomputed_vector() {
        // vector computed independently with python hmac/hashlib
        let sig = "lrio_2A_EDYOogJybA7hm-AfXAr5YhjYhXwJ7_K93-U";
        assert!(test_signing().verify(sig, "/resize/100/100/x.jpg"));
    }

    #[test]
    fn split_format_token_grammar() {
        // plain names pass through untouched
        assert_eq!(split_format("photo.jpg"), Ok(("photo.jpg", None)));
        // '@' suffixes that aren't format tokens stay part of the filename
        assert_eq!(split_format("photo@2x.jpg"), Ok(("photo@2x.jpg", None)));
        assert_eq!(
            split_format("photo.jpg@bogus"),
            Ok(("photo.jpg@bogus", None))
        );
        assert_eq!(split_format("@webp"), Ok(("@webp", None)));
        // known tokens strip and resolve
        for (token, fmt) in [
            ("jpg", ImageFormat::Jpeg),
            ("jpeg", ImageFormat::Jpeg),
            ("png", ImageFormat::Png),
            ("webp", ImageFormat::Webp),
        ] {
            assert_eq!(
                split_format(&format!("photo.png@{token}")),
                Ok(("photo.png", Some(fmt))),
                "@{token}"
            );
        }
        // reserved: jxl errors clearly instead of 404ing as a filename
        assert_eq!(
            split_format("photo.jpg@jxl").unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        #[cfg(feature = "avif")]
        assert_eq!(
            split_format("photo.jpg@avif"),
            Ok(("photo.jpg", Some(ImageFormat::Avif)))
        );
        #[cfg(not(feature = "avif"))]
        assert_eq!(
            split_format("photo.jpg@avif").unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    /// The full accept/reject table for multi-segment source paths: what
    /// nesting lets in, and every escape the wider capture must not.
    #[test]
    fn source_path_validation_table() {
        let ok = |p: &str| validate_source_path(p).is_ok();
        // plain and nested names pass
        assert!(ok("photo.jpg"));
        assert!(ok("albums/2026/photo.jpg"));
        assert!(ok("attachment/public_image/uuid-1/uuid-2.png"));
        // interior dots are legitimate names, not traversal (the old
        // substring check rejected these)
        assert!(ok("my..file.jpg"));
        assert!(ok("dir.d/...jpg"));
        // '@' in a directory name is fine (token handling is separate)
        assert!(ok("ver@2/photo.jpg"));
        // traversal components, in any position
        assert!(!ok(".."));
        assert!(!ok("../secret.jpg"));
        assert!(!ok("a/../secret.jpg"));
        assert!(!ok("a/b/.."));
        assert!(!ok("./a.jpg"));
        assert!(!ok("a/./b.jpg"));
        // empty components: absolute paths, '//' (a protocol-relative
        // authority once joined onto a base URL), trailing slash
        assert!(!ok("/etc/passwd"));
        assert!(!ok("a//b.jpg"));
        assert!(!ok("a/b/"));
        // rejected bytes anywhere
        assert!(!ok("a\\b.jpg"));
        assert!(!ok("a/b?.jpg"));
        assert!(!ok("a/b#.jpg"));
        assert!(!ok("a/b\x00.jpg"));
        assert!(!ok("a/b\x7f.jpg"));
    }

    /// The upstream URL re-encode: typical names pass through
    /// byte-identical, everything outside pchar is escaped, and '%' is
    /// escaped so the origin's own decode cannot manufacture characters
    /// the validation never saw (double-decode traversal).
    #[test]
    fn upstream_path_encoding() {
        assert_eq!(encode_upstream_path("photo.jpg"), "photo.jpg");
        assert_eq!(
            encode_upstream_path("albums/2026/photo@2x.jpg"),
            "albums/2026/photo@2x.jpg"
        );
        assert_eq!(encode_upstream_path("a b.jpg"), "a%20b.jpg");
        assert_eq!(encode_upstream_path("a%2e%2e/x.jpg"), "a%252e%252e/x.jpg");
        // non-ASCII goes out as encoded UTF-8 bytes
        assert_eq!(encode_upstream_path("café.jpg"), "caf%C3%A9.jpg");
    }

    /// @fmt tokens live on the last segment only; directory names with
    /// '@' never participate.
    #[test]
    fn split_format_on_nested_paths() {
        assert_eq!(
            split_format("a/b/photo.png@webp"),
            Ok(("a/b/photo.png", Some(ImageFormat::Webp)))
        );
        assert_eq!(
            split_format("ver@2/photo.jpg"),
            Ok(("ver@2/photo.jpg", None))
        );
        assert_eq!(
            split_format("ver@webp/photo.jpg"),
            Ok(("ver@webp/photo.jpg", None))
        );
        // '@token' with an empty base in the last segment is a filename
        assert_eq!(split_format("dir/@webp"), Ok(("dir/@webp", None)));
        #[cfg(feature = "avif")]
        assert_eq!(
            split_format("a/b/photo.jpg@avif"),
            Ok(("a/b/photo.jpg", Some(ImageFormat::Avif)))
        );
        #[cfg(not(feature = "avif"))]
        assert_eq!(
            split_format("a/b/photo.jpg@avif").unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn negotiate_picks_first_acceptable() {
        let auto = [ImageFormat::Avif, ImageFormat::Webp];
        let accept = |v: &str| AcceptHeader(Some(HeaderValue::from_str(v).unwrap()));
        assert_eq!(
            negotiate(&auto, &accept("image/avif,image/webp,*/*")),
            Some(ImageFormat::Avif)
        );
        assert_eq!(
            negotiate(&auto, &accept("image/webp,*/*")),
            Some(ImageFormat::Webp)
        );
        assert_eq!(negotiate(&auto, &accept("image/apng,*/*")), None);
        assert_eq!(negotiate(&auto, &AcceptHeader(None)), None);
        assert_eq!(negotiate(&[], &accept("image/webp")), None);
    }

    #[test]
    fn signature_rejects_wrong_path_and_garbage() {
        let s = test_signing();
        let sig = "lrio_2A_EDYOogJybA7hm-AfXAr5YhjYhXwJ7_K93-U";
        assert!(!s.verify(sig, "/resize/100/101/x.jpg"));
        assert!(!s.verify("AAAA", "/resize/100/100/x.jpg"));
        assert!(!s.verify("!!!not-base64!!!", "/resize/100/100/x.jpg"));
        assert!(!s.verify("", "/resize/100/100/x.jpg"));
    }
}
