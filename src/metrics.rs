//! Prometheus text metrics, in-tree: the exposition format is a few
//! lines of text and every series here is an atomic counter, so a
//! client library would buy nothing but dependencies. Off by default;
//! `OXIMG_METRICS=1` registers `/metrics` (expose it to your scrape
//! network only — the route is deliberately outside the URL-signing
//! scheme).
//!
//! The set is chosen for the failure modes a CDN origin actually has
//! (issue #4): status/format mix, upstream fetch outcomes with
//! timeouts distinct from faults, and — the one an operator cannot get
//! from outside — queue wait separated from processing time. Rising
//! queue wait under flat processing time reads "needs more CPU";
//! both rising reads "sources got bigger".

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use oximg::pipeline::ImageFormat;

const CLASSES: [&str; 4] = ["2xx", "3xx", "4xx", "5xx"];
/// "source" = no explicit/negotiated target (the source's own format);
/// "none" = the request failed before format resolution.
const FORMATS: [&str; 6] = ["jpeg", "png", "webp", "avif", "source", "none"];
/// "rejected" is a key the store can never serve (over-length, or a
/// 400/414 from the origin): a client error, deliberately kept out of
/// "error" so that series stays a signal of upstream health.
const OUTCOMES: [&str; 5] = ["ok", "not_found", "timeout", "rejected", "error"];
/// Prometheus' default duration buckets: request work here spans
/// single-digit milliseconds (cache-warm small images) to seconds
/// (cold AVIF encodes), which is exactly the range these cover.
const BOUNDS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

struct Histogram {
    /// Per-bound increments plus the +Inf overflow slot; cumulated at
    /// render time as the exposition format requires.
    buckets: [AtomicU64; BOUNDS.len() + 1],
    sum_micros: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    const fn new() -> Self {
        Histogram {
            buckets: [const { AtomicU64::new(0) }; BOUNDS.len() + 1],
            sum_micros: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    fn observe(&self, seconds: f64) {
        let slot = BOUNDS
            .iter()
            .position(|b| seconds <= *b)
            .unwrap_or(BOUNDS.len());
        self.buckets[slot].fetch_add(1, Relaxed);
        self.sum_micros.fetch_add((seconds * 1e6) as u64, Relaxed);
        self.count.fetch_add(1, Relaxed);
    }

    fn render(&self, out: &mut String, name: &str, phase: &str) {
        use std::fmt::Write;
        let mut cum = 0u64;
        for (i, b) in BOUNDS.iter().enumerate() {
            cum += self.buckets[i].load(Relaxed);
            let _ = writeln!(out, "{name}_bucket{{phase=\"{phase}\",le=\"{b}\"}} {cum}");
        }
        cum += self.buckets[BOUNDS.len()].load(Relaxed);
        let _ = writeln!(out, "{name}_bucket{{phase=\"{phase}\",le=\"+Inf\"}} {cum}");
        let sum = self.sum_micros.load(Relaxed) as f64 / 1e6;
        let _ = writeln!(out, "{name}_sum{{phase=\"{phase}\"}} {sum}");
        let _ = writeln!(
            out,
            "{name}_count{{phase=\"{phase}\"}} {}",
            self.count.load(Relaxed)
        );
    }
}

pub struct Metrics {
    requests: [[AtomicU64; FORMATS.len()]; CLASSES.len()],
    upstream: [AtomicU64; OUTCOMES.len()],
    queue: Histogram,
    process: Histogram,
    fetch: Histogram,
    coalesced_leaders: AtomicU64,
    coalesced_followers: AtomicU64,
}

pub static METRICS: Metrics = Metrics::new();

/// The format label for a request: `Unresolved` before the explicit
/// token / Accept negotiation ran (early 4xx), `Resolved(None)` for
/// "keep the source format".
#[derive(Clone, Copy)]
pub enum FormatLabel {
    Unresolved,
    Resolved(Option<ImageFormat>),
}

fn format_index(label: FormatLabel) -> usize {
    match label {
        FormatLabel::Resolved(Some(ImageFormat::Jpeg)) => 0,
        FormatLabel::Resolved(Some(ImageFormat::Png)) => 1,
        FormatLabel::Resolved(Some(ImageFormat::Webp)) => 2,
        #[cfg(feature = "avif")]
        FormatLabel::Resolved(Some(ImageFormat::Avif)) => 3,
        FormatLabel::Resolved(None) => 4,
        FormatLabel::Unresolved => 5,
        // ImageFormat is #[non_exhaustive] upstream of this binary:
        // count future formats under "source" rather than dropping them.
        #[allow(unreachable_patterns)]
        FormatLabel::Resolved(Some(_)) => 4,
    }
}

impl Metrics {
    const fn new() -> Self {
        Metrics {
            requests: [const { [const { AtomicU64::new(0) }; FORMATS.len()] }; CLASSES.len()],
            upstream: [const { AtomicU64::new(0) }; OUTCOMES.len()],
            queue: Histogram::new(),
            process: Histogram::new(),
            fetch: Histogram::new(),
            coalesced_leaders: AtomicU64::new(0),
            coalesced_followers: AtomicU64::new(0),
        }
    }

    pub fn record_request(&self, status: u16, format: FormatLabel) {
        let class = match status / 100 {
            2 => 0,
            3 => 1,
            4 => 2,
            _ => 3,
        };
        self.requests[class][format_index(format)].fetch_add(1, Relaxed);
    }

    pub fn record_upstream(&self, outcome: &str) {
        if let Some(i) = OUTCOMES.iter().position(|o| *o == outcome) {
            self.upstream[i].fetch_add(1, Relaxed);
        }
    }

    pub fn observe_queue(&self, seconds: f64) {
        self.queue.observe(seconds);
    }

    pub fn observe_process(&self, seconds: f64) {
        self.process.observe(seconds);
    }

    pub fn observe_fetch(&self, seconds: f64) {
        self.fetch.observe(seconds);
    }

    pub fn record_leader(&self) {
        self.coalesced_leaders.fetch_add(1, Relaxed);
    }

    pub fn record_follower(&self) {
        self.coalesced_followers.fetch_add(1, Relaxed);
    }

    /// The full exposition page. Gauges are computed at scrape time
    /// from live server state (permits, in-flight map), so they need
    /// the caller to pass them in — everything else is owned here.
    pub fn render(&self, workers: usize, permits_available: usize, inflight_keys: usize) -> String {
        use std::fmt::Write;
        let mut out = String::with_capacity(4096);

        let _ = writeln!(
            out,
            "# HELP oximg_requests_total Resize requests by status class and resolved output format."
        );
        let _ = writeln!(out, "# TYPE oximg_requests_total counter");
        for (c, class) in CLASSES.iter().enumerate() {
            for (f, format) in FORMATS.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "oximg_requests_total{{class=\"{class}\",format=\"{format}\"}} {}",
                    self.requests[c][f].load(Relaxed)
                );
            }
        }

        let _ = writeln!(
            out,
            "# HELP oximg_upstream_fetch_total Remote-source fetch outcomes (timeout distinct from error)."
        );
        let _ = writeln!(out, "# TYPE oximg_upstream_fetch_total counter");
        for (i, outcome) in OUTCOMES.iter().enumerate() {
            let _ = writeln!(
                out,
                "oximg_upstream_fetch_total{{outcome=\"{outcome}\"}} {}",
                self.upstream[i].load(Relaxed)
            );
        }

        let _ = writeln!(
            out,
            "# HELP oximg_upstream_retries_total Connection-level fetch failures retried (one retry per fetch)."
        );
        let _ = writeln!(out, "# TYPE oximg_upstream_retries_total counter");
        let _ = writeln!(
            out,
            "oximg_upstream_retries_total {}",
            oximg::pipeline::upstream_retry_count()
        );

        // The decoded-bytes estimate as a histogram: an operator can
        // read a cap off their own corpus instead of guessing one from
        // pixels, which do not map to memory in this pipeline (#17).
        let _ = writeln!(
            out,
            "# HELP oximg_decoded_bytes_estimate Estimated decode-stage allocation per request, in bytes."
        );
        let _ = writeln!(out, "# TYPE oximg_decoded_bytes_estimate histogram");
        let (counts, sum) = oximg::pipeline::decoded_bytes_histogram();
        let mut cum = 0u64;
        for (i, b) in oximg::pipeline::DECODED_BYTES_BOUNDS.iter().enumerate() {
            cum += counts[i];
            let _ = writeln!(
                out,
                "oximg_decoded_bytes_estimate_bucket{{le=\"{b}\"}} {cum}"
            );
        }
        cum += counts[oximg::pipeline::DECODED_BYTES_BOUNDS.len()];
        let _ = writeln!(
            out,
            "oximg_decoded_bytes_estimate_bucket{{le=\"+Inf\"}} {cum}"
        );
        let _ = writeln!(out, "oximg_decoded_bytes_estimate_sum {sum}");
        let _ = writeln!(out, "oximg_decoded_bytes_estimate_count {cum}");

        let _ = writeln!(
            out,
            "# HELP oximg_request_duration_seconds Time split into remote-source fetch (no CPU permit held), CPU-permit queue wait, and processing (permit held)."
        );
        let _ = writeln!(out, "# TYPE oximg_request_duration_seconds histogram");
        self.queue
            .render(&mut out, "oximg_request_duration_seconds", "queue");
        self.process
            .render(&mut out, "oximg_request_duration_seconds", "process");
        // Since issue #22 the fetch *precedes* the permit instead of
        // being a subset of its hold: it covers everything between
        // "ready to fetch" and "source in hand" — the fetch-slot wait,
        // the pool hand-off, and the whole download. A permit's hold
        // time is now the process phase alone, so fetch/process no
        // longer names recoverable throughput; it names what the
        // permit no longer pays for.
        self.fetch
            .render(&mut out, "oximg_request_duration_seconds", "fetch");

        let _ = writeln!(
            out,
            "# HELP oximg_coalesced_requests_total Singleflight roles; followers/leaders is the coalescing hit rate."
        );
        let _ = writeln!(out, "# TYPE oximg_coalesced_requests_total counter");
        let _ = writeln!(
            out,
            "oximg_coalesced_requests_total{{role=\"leader\"}} {}",
            self.coalesced_leaders.load(Relaxed)
        );
        let _ = writeln!(
            out,
            "oximg_coalesced_requests_total{{role=\"follower\"}} {}",
            self.coalesced_followers.load(Relaxed)
        );

        let _ = writeln!(
            out,
            "# HELP oximg_cpu_permits_in_use CPU permits currently held."
        );
        let _ = writeln!(out, "# TYPE oximg_cpu_permits_in_use gauge");
        let _ = writeln!(
            out,
            "oximg_cpu_permits_in_use {}",
            workers.saturating_sub(permits_available)
        );
        let _ = writeln!(
            out,
            "# HELP oximg_cpu_workers Total CPU permits (core count)."
        );
        let _ = writeln!(out, "# TYPE oximg_cpu_workers gauge");
        let _ = writeln!(out, "oximg_cpu_workers {workers}");
        let _ = writeln!(
            out,
            "# HELP oximg_inflight_keys Distinct (w, h, file, format) requests currently processing."
        );
        let _ = writeln!(out, "# TYPE oximg_inflight_keys gauge");
        let _ = writeln!(out, "oximg_inflight_keys {inflight_keys}");

        out
    }
}
