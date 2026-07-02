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
type FlightResult = Result<Bytes, (StatusCode, String)>;
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
    };

    let router = Router::new()
        .route("/health", get(async || "ok"))
        .route("/resize/{w}/{h}/{file}", get(handle_resize))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    eprintln!(
        "oximg listening on :{port} (images: {}, workers: {workers})",
        images_dir.display()
    );
    axum::serve(listener, router).await?;
    Ok(())
}

async fn handle_resize(
    State(app): State<App>,
    Path((w, h, file)): Path<(u32, u32, String)>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return Err((StatusCode::BAD_REQUEST, "invalid dimensions".into()));
    }
    if file.contains(['/', '\\']) || file.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "invalid filename".into()));
    }

    let out = singleflight(&app, (w, h, file)).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/jpeg"),
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

    Ok(Bytes::from(out))
}
