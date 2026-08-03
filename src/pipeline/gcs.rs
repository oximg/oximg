//! `gs://` source fetching (issue #11): read objects from a private
//! GCS bucket with GCP-attached credentials, instead of requiring the
//! bucket to be world-readable for the HTTP mode. In-tree on the same
//! blocking ureq stack as every other fetch — streaming decode, size
//! caps, deadlines, and the transient retry all apply unchanged.
//!
//! Credentials, v1: the GCP metadata server only — which is what GKE
//! Workload Identity, Cloud Run, and GCE all provide. `service_account`
//! JSON keys need RS256 signing (an RSA dependency) and are deliberately
//! out of scope; off-GCP deployments keep using the HTTP mode.
//!
//! Test/emulator seams, both honored at startup: `GCE_METADATA_HOST`
//! (the same override Google's own SDKs honor) and `OXIMG_GCS_ENDPOINT`
//! (default `https://storage.googleapis.com`; also useful for Private
//! Service Connect endpoints).

use anyhow::{Context, Result};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::UpstreamFault;

fn endpoint() -> String {
    std::env::var("OXIMG_GCS_ENDPOINT")
        .ok()
        .map(|v| v.trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://storage.googleapis.com".to_string())
}

fn metadata_host() -> String {
    std::env::var("GCE_METADATA_HOST")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "metadata.google.internal".to_string())
}

struct CachedToken {
    bearer: String,
    expires_at: Instant,
}

/// One token for the process, refreshed under the lock — a refresh is
/// one local metadata-server round trip (single-digit ms on GCP), so
/// single-flighting it behind a Mutex is simpler than any scheme that
/// lets N threads race to refresh the same token.
static TOKEN: Mutex<Option<CachedToken>> = Mutex::new(None);

fn fetch_token() -> Result<CachedToken> {
    let url = format!(
        "http://{}/computeMetadata/v1/instance/service-accounts/default/token",
        metadata_host()
    );
    let mut resp = super::http_agent()
        .get(&url)
        .header("Metadata-Flavor", "Google")
        .call()
        .context("GCP metadata server token request")?;
    let body = resp
        .body_mut()
        .read_to_string()
        .context("read metadata token response")?;
    let v: serde_json::Value =
        serde_json::from_str(&body).context("parse metadata token response")?;
    let token = v["access_token"]
        .as_str()
        .context("metadata token response lacks access_token")?;
    let expires_in = v["expires_in"].as_u64().unwrap_or(300);
    Ok(CachedToken {
        bearer: format!("Bearer {token}"),
        // Refresh a minute early so a token never expires mid-fetch;
        // the floor keeps a pathological expires_in from thrashing.
        expires_at: Instant::now() + Duration::from_secs(expires_in.saturating_sub(60).max(10)),
    })
}

/// The current Bearer header value, refreshing if needed. `force`
/// discards the cache first (after a 401: the token may have been
/// revoked before its stated expiry).
fn bearer(force: bool) -> Result<String> {
    let mut guard = match TOKEN.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !force
        && let Some(t) = guard.as_ref()
        && t.expires_at > Instant::now()
    {
        return Ok(t.bearer.clone());
    }
    let fresh = fetch_token()?;
    let bearer = fresh.bearer.clone();
    *guard = Some(fresh);
    Ok(bearer)
}

/// Startup credential probe: fail closed at boot with a clear message
/// instead of a 502 on the first cache miss. Bucket-level permissions
/// are still per-request territory — this only proves credentials
/// exist.
pub(crate) fn startup() -> Result<(), String> {
    bearer(false).map(|_| ()).map_err(|e| {
        format!(
            "gs:// source needs GCP-attached credentials \
             (metadata server at {:?} unreachable: {e:#}) — GKE Workload \
             Identity, Cloud Run, and GCE provide them; service-account \
             JSON keys are not supported (use the HTTP mode off-GCP)",
            metadata_host()
        )
    })
}

/// Statuses worth one retry, mirroring what the SDKs retry on reads:
/// throttling and transient server-side errors. 401 also retries, but
/// with a forced token refresh.
fn retryable_status(code: u16) -> bool {
    matches!(code, 401 | 429 | 500 | 502 | 503 | 504)
}

/// GET one object, authenticated. `key` is already percent-encoded by
/// the caller (the same segment-wise encoding as the HTTP mode).
pub(crate) fn fetch(bucket: &str, key: &str) -> Result<ureq::http::Response<ureq::Body>> {
    let url = format!("{}/{bucket}/{key}", endpoint());
    let attempt = |force_token: bool| -> Result<_, anyhow::Error> {
        let bearer = bearer(force_token)?;
        super::http_agent()
            .get(&url)
            .header("Authorization", &bearer)
            .call()
            .map_err(anyhow::Error::new)
    };
    let first = attempt(false);
    let resp = match first {
        Ok(r) => r,
        Err(e) => {
            let retry = match e.downcast_ref::<ureq::Error>() {
                Some(ue) if super::transient_fetch_error(ue) => Some(false),
                Some(ureq::Error::StatusCode(code)) if retryable_status(*code) => {
                    Some(*code == 401)
                }
                _ => None,
            };
            match retry {
                Some(force_token) => {
                    super::UPSTREAM_RETRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(100));
                    attempt(force_token).map_err(|e| map_fetch_err(e, bucket))?
                }
                None => return Err(map_fetch_err(e, bucket)),
            }
        }
    };
    Ok(resp)
}

/// GCS status semantics: 404 is an absent object (a client-facing
/// 404); 401/403 is a real permission problem — a deployment fault,
/// never the requester's (PermissionDenied classifies as
/// SourceUnreadable, HTTP 500). Everything else indicts the origin.
fn map_fetch_err(e: anyhow::Error, bucket: &str) -> anyhow::Error {
    match e.downcast_ref::<ureq::Error>() {
        Some(ureq::Error::StatusCode(404)) => anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "object not found in bucket",
        )),
        Some(ureq::Error::StatusCode(401 | 403)) => anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("access to bucket {bucket:?} denied (check the service account's roles)"),
        )),
        _ => e.context("fetch gcs object").context(UpstreamFault),
    }
}
