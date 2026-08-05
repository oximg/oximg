//! `gs://` source fetching (issue #11): read objects from a private
//! GCS bucket with GCP-attached credentials, instead of requiring the
//! bucket to be world-readable for the HTTP mode. Runs on the same
//! shared reqwest client as every other fetch — size caps, deadlines,
//! and the transient retry all apply unchanged, and h2 multiplexes
//! every fetch to the storage endpoint over one connection.
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
use std::time::{Duration, Instant};

use super::{SourceRejected, UpstreamFault};

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

/// One token for the process, refreshed under an async mutex: a
/// refresh is one local metadata-server round trip (single-digit ms
/// on GCP), so single-flighting it behind the lock is simpler than
/// any scheme that lets N tasks race to refresh the same token. The
/// mutex being async matters: waiting fetches yield instead of
/// pinning threads while the refresh is in flight (the sync version
/// held a std::sync::Mutex across the round trip).
static TOKEN: tokio::sync::Mutex<Option<CachedToken>> = tokio::sync::Mutex::const_new(None);

async fn fetch_token() -> Result<CachedToken> {
    let url = format!(
        "http://{}/computeMetadata/v1/instance/service-accounts/default/token",
        metadata_host()
    );
    let resp = super::fetch_client()
        .get(&url)
        .header("Metadata-Flavor", "Google")
        .send()
        .await
        .context("GCP metadata server token request")?;
    let body = resp.text().await.context("read metadata token response")?;
    let v: serde_json::Value =
        serde_json::from_str(&body).context("parse metadata token response")?;
    let token = v["access_token"]
        .as_str()
        .context("metadata token response lacks access_token")?;
    // Clamp to a day: a metadata endpoint (possibly a misconfigured
    // GCE_METADATA_HOST) returning a pathological expires_in must not
    // panic Instant + Duration on overflow — and no real token lives
    // longer anyway.
    let expires_in = v["expires_in"].as_u64().unwrap_or(300).min(24 * 3600);
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
async fn bearer(force: bool) -> Result<String> {
    let mut guard = TOKEN.lock().await;
    if !force
        && let Some(t) = guard.as_ref()
        && t.expires_at > Instant::now()
    {
        return Ok(t.bearer.clone());
    }
    let fresh = fetch_token().await?;
    let bearer = fresh.bearer.clone();
    *guard = Some(fresh);
    Ok(bearer)
}

/// Startup credential probe: fail closed at boot with a clear message
/// instead of a 502 on the first cache miss. Bucket-level permissions
/// are still per-request territory — this only proves credentials
/// exist.
pub(crate) fn startup() -> Result<(), String> {
    super::block_on_fetch(async { bearer(false).await.map(|_| ()) }).map_err(|e| {
        format!(
            "gs:// source needs GCP-attached credentials \
             (metadata server at {:?} unreachable: {e:#}) — GKE Workload \
             Identity, Cloud Run, and GCE provide them; service-account \
             JSON keys are not supported (use the HTTP mode off-GCP)",
            metadata_host()
        )
    })
}

/// GCS caps object names at 1024 bytes of UTF-8 (a documented
/// constant), so an over-length key is checkable locally: the store
/// would reject it, and no round trip can change that. `key` arrives
/// percent-encoded, and every escape this crate emits is a %XX
/// triplet standing for one byte, so the decoded length is the
/// encoded length minus two per escape.
const GCS_MAX_KEY_BYTES: usize = 1024;

fn decoded_key_len(key: &str) -> usize {
    key.len() - 2 * key.bytes().filter(|b| *b == b'%').count()
}

/// Statuses worth one retry, mirroring what the SDKs retry on reads:
/// throttling and transient server-side errors. 401 also retries, but
/// with a forced token refresh.
fn retryable_status(code: u16) -> bool {
    matches!(code, 401 | 429 | 500 | 502 | 503 | 504)
}

/// GET one object, authenticated. `key` is already percent-encoded by
/// the caller (the same segment-wise encoding as the HTTP mode).
pub(crate) async fn fetch(bucket: &str, key: &str) -> Result<reqwest::Response> {
    // Refuse impossible keys before the request leaves: an object
    // with this name cannot exist, so from the requester's side this
    // is indistinguishable from an absent object — and answering it
    // locally spends no round trip on traffic that is, in practice,
    // crawlers fetching whole srcset attributes as one URL (#13).
    if decoded_key_len(key) > GCS_MAX_KEY_BYTES {
        return Err(anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "object name is {} bytes, over the {GCS_MAX_KEY_BYTES}-byte GCS limit",
                decoded_key_len(key)
            ),
        )));
    }
    let url = format!("{}/{bucket}/{key}", endpoint());
    let attempt = async |force_token: bool| -> Result<reqwest::Response> {
        let bearer = bearer(force_token).await?;
        super::fetch_client()
            .get(&url)
            .header("Authorization", &bearer)
            .send()
            .await
            .map_err(anyhow::Error::new)
    };
    // One retry, three triggers, mirroring the plain-HTTP path plus
    // the SDKs' read semantics: connection transients (as-is),
    // retryable statuses (as-is), and 401 with a *forced* token
    // refresh — a revoked token would just 401 again from the cache.
    let resp = match attempt(false).await {
        Ok(resp) if retryable_status(resp.status().as_u16()) => {
            let force_token = resp.status().as_u16() == 401;
            super::UPSTREAM_RETRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(100)).await;
            attempt(force_token).await.map_err(map_transport_err)?
        }
        Ok(resp) => resp,
        Err(e) => {
            let transient = e
                .downcast_ref::<reqwest::Error>()
                .is_some_and(|re| !re.is_timeout() && (re.is_connect() || re.is_request()));
            if !transient {
                return Err(map_transport_err(e));
            }
            super::UPSTREAM_RETRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(100)).await;
            attempt(false).await.map_err(map_transport_err)?
        }
    };
    refuse_status(resp, bucket)
}

/// Transport-level failures (or a bearer that could not be minted):
/// timeouts keep their io shape for classification, everything else
/// indicts the upstream.
fn map_transport_err(e: anyhow::Error) -> anyhow::Error {
    if e.downcast_ref::<reqwest::Error>()
        .is_some_and(reqwest::Error::is_timeout)
    {
        return anyhow::Error::new(std::io::Error::new(std::io::ErrorKind::TimedOut, e));
    }
    e.context("fetch gcs object").context(UpstreamFault)
}

/// GCS status semantics: 404 is an absent object (a client-facing
/// 404); 400/414 is the store refusing an impossible request — the
/// requester's fault, not the store's (#13); 401/403 is a real
/// permission problem — a deployment fault, never the requester's
/// (PermissionDenied classifies as SourceUnreadable, HTTP 500).
/// Redirects are refused like every fetch in this crate. Everything
/// else non-success indicts the origin.
fn refuse_status(resp: reqwest::Response, bucket: &str) -> Result<reqwest::Response> {
    let status = resp.status();
    match status.as_u16() {
        404 => Err(anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "object not found in bucket",
        ))),
        code @ (400 | 414) => Err(
            anyhow::anyhow!("object store rejected the request ({code})").context(SourceRejected),
        ),
        401 | 403 => Err(anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("access to bucket {bucket:?} denied (check the service account's roles)"),
        ))),
        _ if status.is_redirection() => Err(anyhow::anyhow!(
            "object store answered {status} (redirects are not followed)"
        )
        .context(UpstreamFault)),
        _ if !status.is_success() => Err(anyhow::anyhow!("object store answered {status}")
            .context("fetch gcs object")
            .context(UpstreamFault)),
        _ => Ok(resp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decoded-length accounting behind the pre-check: escapes
    /// stand for one byte each, so a percent-heavy key must not be
    /// mistaken for an over-length one.
    #[test]
    fn decoded_key_len_counts_escapes_as_one_byte() {
        assert_eq!(decoded_key_len("photo.jpg"), 9);
        assert_eq!(decoded_key_len("a%20b.jpg"), 7);
        // Three-byte UTF-8 encoded as three escapes decodes to 3 bytes.
        assert_eq!(decoded_key_len("%E4%B8%AD.jpg"), 7);
    }

    /// The GCS limit is inclusive: 1024 bytes is a legal object name,
    /// 1025 is not.
    #[test]
    fn key_length_boundary_is_the_documented_limit() {
        let at = |n: usize| decoded_key_len(&"x".repeat(n)) > GCS_MAX_KEY_BYTES;
        assert!(!at(1023));
        assert!(!at(1024));
        assert!(at(1025));
        // Escaped bytes count once, so this 2048-char key is legal.
        assert!(!(decoded_key_len(&"%20".repeat(341)) > GCS_MAX_KEY_BYTES));
    }
}
