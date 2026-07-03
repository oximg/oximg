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
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::sync::{Semaphore, watch};

type FlightKey = (u32, u32, String);
type FlightResult = Result<(Bytes, &'static str), (StatusCode, String)>;
type FlightMap = Mutex<HashMap<FlightKey, watch::Receiver<Option<FlightResult>>>>;

#[derive(Clone)]
struct App {
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
    fn from_env() -> Option<Self> {
        let decode = |name: &str| -> Option<Vec<u8>> {
            let v = std::env::var(name).ok()?;
            let v = v.trim();
            if v.is_empty() || v.len() % 2 != 0 {
                return None;
            }
            (0..v.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&v[i..i + 2], 16).ok())
                .collect()
        };
        match (decode("OXIMG_KEY"), decode("OXIMG_SALT")) {
            (Some(key), Some(salt)) => Some(Signing { key, salt }),
            (None, None) => None,
            _ => {
                eprintln!(
                    "oximg: OXIMG_KEY and OXIMG_SALT must both be set (hex); signing disabled"
                );
                None
            }
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

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() -> anyhow::Result<()> {
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
        signing: Signing::from_env().map(Arc::new),
    };
    if app.signing.is_some() {
        eprintln!("oximg: URL signing enabled");
    }

    let router = Router::new()
        .route("/health", get(async || "ok"))
        .route("/resize/{w}/{h}/{file}", get(handle_resize))
        .route("/{sig}/resize/{w}/{h}/{file}", get(handle_signed_resize))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    eprintln!(
        "oximg listening on :{port} (images: {}, workers: {workers})",
        images_dir.display()
    );
    axum::serve(listener, router).await?;
    Ok(())
}

async fn handle_signed_resize(
    State(app): State<App>,
    Path((sig, w, h, file)): Path<(String, u32, u32, String)>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let Some(signing) = app.signing.as_ref() else {
        return Err((StatusCode::NOT_FOUND, "signing not configured".into()));
    };
    let path = format!("/resize/{w}/{h}/{file}");
    if !signing.verify(&sig, &path) {
        return Err((StatusCode::FORBIDDEN, "invalid signature".into()));
    }
    serve_resize(app, w, h, file).await
}

async fn handle_resize(
    State(app): State<App>,
    Path((w, h, file)): Path<(u32, u32, String)>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    if app.signing.is_some() {
        return Err((StatusCode::FORBIDDEN, "signature required".into()));
    }
    serve_resize(app, w, h, file).await
}

async fn serve_resize(
    app: App,
    w: u32,
    h: u32,
    file: String,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return Err((StatusCode::BAD_REQUEST, "invalid dimensions".into()));
    }
    if file.contains(['/', '\\']) || file.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "invalid filename".into()));
    }

    let (out, content_type) = singleflight(&app, (w, h, file)).await?;
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=31536000"),
        ],
        out,
    ))
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
        self.map.lock().unwrap().remove(&self.key);
    }
}

/// Process the request, coalescing concurrent duplicates: the first caller
/// (leader) runs the pipeline; followers await its watch channel and share
/// the resulting `Bytes` (O(1) clone). If a leader dies without publishing
/// (panic/cancel), the channel closes and followers retry for leadership.
async fn singleflight(app: &App, key: FlightKey) -> FlightResult {
    for _ in 0..3 {
        let leader_tx = {
            let mut map = app.inflight.lock().unwrap();
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

async fn process_one(app: &App, key: &FlightKey) -> FlightResult {
    let (w, h, file) = key;
    let path = app.images_dir.join(file);

    // CPU concurrency cap = core count; queueing happens here instead of
    // flooding the blocking pool.
    let permit = app
        .cpu_slots
        .clone()
        .acquire_owned()
        .await
        .expect("semaphore closed");

    let params = pipeline::Params {
        max_width: *w,
        max_height: *h,
        quality: app.quality,
        encoder: app.encoder,
        // The resize stage may briefly fan out into row bands without
        // taking semaphore slots — resize is only ~1/4 of request time, so
        // average oversubscription stays <30% in exchange for lower
        // light-load latency.
        parallel: app.resize_threads,
    };
    let source_url = app
        .source_base
        .as_ref()
        .map(|base| format!("{base}/{file}"));
    let out = tokio::task::spawn_blocking(move || {
        let _permit = permit; // hold the CPU slot for the whole processing
        // Streaming decode: the source is never buffered whole on the heap
        // (saves concurrency x file-size for large sources under load);
        // for remote sources decoding overlaps the download.
        match source_url {
            Some(url) => pipeline::process_url(&url, &params),
            None => pipeline::process_path(&path, &params),
        }
    })
    .await
    .map_err(|_| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "image processing panicked (broken image?)".to_string(),
        )
    })?
    .map_err(|e| match e.downcast_ref::<std::io::Error>() {
        Some(io) if io.kind() == std::io::ErrorKind::NotFound => {
            (StatusCode::NOT_FOUND, "image not found".to_string())
        }
        _ => (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
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

    #[test]
    fn signature_verifies_precomputed_vector() {
        // vector computed independently with python hmac/hashlib
        let sig = "lrio_2A_EDYOogJybA7hm-AfXAr5YhjYhXwJ7_K93-U";
        assert!(test_signing().verify(sig, "/resize/100/100/x.jpg"));
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
