//! `s3://` source fetching (issue #11): read objects from a private
//! S3 or S3-compatible bucket (AWS S3, Cloudflare R2, MinIO). It runs
//! on the same shared reqwest client as every other fetch, so size
//! caps and deadlines apply unchanged. `fetch` has its own copy of the
//! one-retry rule.
//!
//! Credentials: static keys from the environment (`AWS_ACCESS_KEY_ID`
//! and `AWS_SECRET_ACCESS_KEY`, plus `AWS_SESSION_TOKEN` for temporary
//! keys). The `~/.aws` profile files are out of scope.
//! `aws configure export-credentials --format env` turns a profile
//! into static keys.
//!
//! Requests are signed with AWS Signature Version 4. The signer uses
//! `hmac` and `sha2`, which the `server` feature already has for URL
//! signing. It does not use an AWS SDK: we only sign one kind of
//! request, a GET with an empty body.
//!
//! S3 differs from the generic SigV4 rules in one place. The canonical
//! URI is the path encoded once, with no normalization. Repeated
//! slashes are part of the key, and we sign the path exactly as we send
//! it. `.` and `..` segments are refused before signing: reqwest would
//! remove them from the URL, and the signature would no longer match.
//!
//! Settings are read once, on first use. `AWS_REGION` is required.
//! `OXIMG_S3_ENDPOINT` defaults to `https://s3.<region>.amazonaws.com`.
//! `OXIMG_S3_PATH_STYLE` is explained at `Settings::path_style`.
//! Temporary keys are not refreshed. When `AWS_SESSION_TOKEN` expires,
//! every fetch fails until the process restarts with new keys.

use anyhow::Result;
use hmac::Mac;
use hmac::digest::KeyInit;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{SourceRejected, UpstreamFault};

type HmacSha256 = hmac::Hmac<Sha256>;

/// SHA-256 of an empty body. Every request this module signs is a GET,
/// so this is always the payload hash.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

struct Credentials {
    access_key_id: String,
    secret_access_key: String,
    /// Present for temporary keys. Signed as `x-amz-security-token`.
    session_token: Option<String>,
}

/// Percent-encode raw path bytes for a SigV4 canonical URI. Only RFC 3986
/// unreserved bytes stay as they are. Every other byte becomes an
/// upper-case `%XX`. This includes the sub-delims that
/// `encode_upstream_path` keeps for the HTTP mode. `/` stays as the
/// separator. We use the result as the request path and as the
/// canonical URI, so the two always match.
fn uri_encode_path(raw: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(raw.len());
    for &b in raw {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xf) as usize] as char);
        }
    }
    out
}

/// The `x-amz-date` value (`YYYYMMDDTHHMMSSZ`) for a Unix time in
/// seconds. The first eight characters are the credential-scope date.
fn amz_datetime(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Convert days since 1970-01-01 to a Gregorian (year, month, day).
/// This is Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    // HMAC accepts a key of any length, so this cannot fail.
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    hmac(&k, b"aws4_request")
}

/// One request to sign. `headers` lists the headers to sign. Their
/// names must be lower-case, and their order does not matter.
/// `canonical_uri` must be the same encoded path that we send.
struct Request<'a> {
    method: &'a str,
    canonical_uri: &'a str,
    canonical_query: &'a str,
    headers: &'a [(&'a str, &'a str)],
    payload_hash: &'a str,
}

/// Return the `Authorization` header value for `req`. `datetime` must
/// be the request's `x-amz-date` value, and `x-amz-date` must be one of
/// the signed headers.
fn authorization(
    req: &Request<'_>,
    creds: &Credentials,
    region: &str,
    service: &str,
    datetime: &str,
) -> String {
    let mut headers: Vec<(&str, String)> = req
        .headers
        .iter()
        .map(|(name, value)| (*name, canonical_header_value(value)))
        .collect();
    headers.sort_by(|a, b| a.0.cmp(b.0));
    let signed_headers = headers
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();
    let canonical_request = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
        req.method, req.canonical_uri, req.canonical_query, req.payload_hash
    );
    let date = &datetime[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{datetime}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let key = signing_key(&creds.secret_access_key, date, region, service);
    let signature = hex(&hmac(&key, string_to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    )
}

/// SigV4 trims a header value and folds each run of spaces into one.
fn canonical_header_value(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

struct Settings {
    /// `http` or `https`.
    scheme: String,
    /// The endpoint's host, with the port only when it is not the
    /// default one. This is what reqwest sends as `Host`, so it is also
    /// what we sign.
    host: String,
    region: String,
    /// `OXIMG_S3_PATH_STYLE`, when set.
    path_style: Option<bool>,
    /// Whether `OXIMG_S3_ENDPOINT` is set. A custom endpoint defaults to
    /// path style.
    custom_endpoint: bool,
    credentials: Credentials,
}

impl Settings {
    /// Path style puts the bucket in the path, virtual-host style in
    /// the host name. Without `OXIMG_S3_PATH_STYLE`, a custom endpoint
    /// uses path style, because MinIO only accepts that by default. On
    /// AWS, a bucket name with a `.` also uses path style, as the AWS
    /// SDKs do: `a.b.s3.<region>.amazonaws.com` is not covered by the
    /// `*.s3.<region>.amazonaws.com` certificate, so TLS would fail.
    fn path_style(&self, bucket: &str) -> bool {
        self.path_style
            .unwrap_or(self.custom_endpoint || bucket.contains('.'))
    }
}

/// Read an `AWS_*` variable the way `config::var` reads ours: trimmed,
/// and blank reads as unset. It is not `config::var` itself because the
/// env inventory test in `config.rs` finds non-`OXIMG_` reads by their
/// `std::env::var("…")` call.
fn aws_var(value: Result<String, std::env::VarError>) -> Option<String> {
    value
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn load_settings() -> Result<Settings, String> {
    let region = aws_var(std::env::var("AWS_REGION")).ok_or(
        "s3:// needs AWS_REGION, the bucket's region (R2 accepts `auto`). \
             AWS_DEFAULT_REGION is not read",
    )?;
    // The region goes into the host name and the signature scope.
    if !region
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(format!("AWS_REGION={region:?} is not a region name"));
    }
    let custom = crate::config::var("OXIMG_S3_ENDPOINT");
    if custom.is_none() && region == "auto" {
        return Err(
            "AWS_REGION=auto is for R2. Set OXIMG_S3_ENDPOINT to the R2 endpoint, \
                    or set AWS_REGION to the AWS bucket's region"
                .into(),
        );
    }
    let endpoint = custom
        .clone()
        .unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com"));
    let (scheme, host) = parse_endpoint(&endpoint)?;
    // `config::validate` refuses other values at boot. A library caller
    // that skips it gets the default.
    let path_style = match crate::config::var("OXIMG_S3_PATH_STYLE").as_deref() {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    };
    // Virtual-host style puts the bucket in front of the host name. An
    // IP address cannot take a prefix like that. Without the setting, a
    // custom endpoint already uses path style.
    if path_style == Some(false) && is_ip_literal(&host) {
        return Err(format!(
            "OXIMG_S3_ENDPOINT={endpoint:?} is an IP address, so it needs path style \
             (OXIMG_S3_PATH_STYLE=1)"
        ));
    }
    let (Some(access_key_id), Some(secret_access_key)) = (
        aws_var(std::env::var("AWS_ACCESS_KEY_ID")),
        aws_var(std::env::var("AWS_SECRET_ACCESS_KEY")),
    ) else {
        return Err("s3:// needs AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY".into());
    };
    Ok(Settings {
        scheme,
        host,
        region,
        path_style,
        custom_endpoint: custom.is_some(),
        credentials: Credentials {
            access_key_id,
            secret_access_key,
            session_token: aws_var(std::env::var("AWS_SESSION_TOKEN")),
        },
    })
}

/// Split an endpoint URL into its scheme and its `Host` value. Only
/// `scheme://host[:port]` is accepted: a path, a query or user info in
/// the endpoint would change what we sign.
fn parse_endpoint(raw: &str) -> Result<(String, String), String> {
    let bad = |why: &str| format!("OXIMG_S3_ENDPOINT={raw:?} {why}");
    let url = reqwest::Url::parse(raw).map_err(|e| bad(&format!("is not a URL ({e})")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(bad("must be http:// or https://"));
    }
    if url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(bad(
            "must be scheme://host[:port], with nothing after the host",
        ));
    }
    let host = url.host_str().ok_or_else(|| bad("has no host"))?;
    let host = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    Ok((url.scheme().to_string(), host))
}

/// True for an IPv4 or bracketed IPv6 host, with or without a port.
fn is_ip_literal(host: &str) -> bool {
    if host.starts_with('[') {
        return true;
    }
    let name = host.rsplit_once(':').map_or(host, |(name, _)| name);
    name.parse::<std::net::Ipv4Addr>().is_ok()
}

/// The settings, read on first use. An error stays cached: the
/// environment does not change while the process runs.
fn settings() -> Result<&'static Settings, String> {
    static SETTINGS: OnceLock<Result<Settings, String>> = OnceLock::new();
    SETTINGS
        .get_or_init(load_settings)
        .as_ref()
        .map_err(Clone::clone)
}

/// Where one object lives: the URL to fetch, the `Host` we sign, and
/// the encoded path, which is both the URL path and the canonical URI.
struct Target {
    url: String,
    host: String,
    path: String,
}

impl Target {
    fn new(s: &Settings, bucket: &str, raw_key: &[u8]) -> Self {
        let key = uri_encode_path(raw_key);
        let (host, path) = if s.path_style(bucket) {
            (s.host.clone(), format!("/{bucket}/{key}"))
        } else {
            (format!("{bucket}.{}", s.host), format!("/{key}"))
        };
        Self {
            url: format!("{}://{host}{path}", s.scheme),
            host,
            path,
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Sign and send one GET. Every attempt is signed again, so a retry
/// carries a fresh `x-amz-date`.
async fn send_signed(s: &Settings, t: &Target) -> reqwest::Result<reqwest::Response> {
    let datetime = amz_datetime(unix_now());
    let mut headers = vec![
        ("host", t.host.as_str()),
        ("x-amz-content-sha256", EMPTY_SHA256),
        ("x-amz-date", datetime.as_str()),
    ];
    if let Some(token) = &s.credentials.session_token {
        headers.push(("x-amz-security-token", token));
    }
    let req = Request {
        method: "GET",
        canonical_uri: &t.path,
        canonical_query: "",
        headers: &headers,
        payload_hash: EMPTY_SHA256,
    };
    let auth = authorization(&req, &s.credentials, &s.region, "s3", &datetime);
    // reqwest sets `Host` from the URL. We signed the same value.
    let mut builder = super::fetch_client()
        .get(&t.url)
        .header("authorization", auth);
    for (name, value) in headers.iter().filter(|(name, _)| *name != "host") {
        builder = builder.header(*name, *value);
    }
    builder.send().await
}

/// S3 caps keys at 1024 bytes of UTF-8, the same as GCS. The store
/// would reject a longer key, so we answer it locally, like `gs://`
/// does (#13).
const S3_MAX_KEY_BYTES: usize = 1024;

/// True if a key has a `.` or `..` segment. reqwest parses the URL and
/// removes such segments, so the path it sends would differ from the
/// path we signed, and S3 would answer 403. The server already refuses
/// these paths. A library caller could still pass one.
fn has_dot_segment(raw_key: &[u8]) -> bool {
    raw_key
        .split(|b| *b == b'/')
        .any(|segment| segment == b"." || segment == b"..")
}

/// Undo the caller's segment-wise percent-encoding. A `%` that does not
/// start a valid escape stays as it is.
fn percent_decode(encoded: &str) -> Vec<u8> {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(&[hi, lo]) = bytes.get(i + 1..i + 3)
            && hi.is_ascii_hexdigit()
            && lo.is_ascii_hexdigit()
        {
            // Two hex digits, so the value always fits in a byte.
            let digit = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
            out.push(digit(hi) << 4 | digit(lo));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Statuses worth one retry, as the SDKs retry reads: throttling and
/// temporary server errors. Unlike `gs://`, 401 is not here. Static
/// keys do not refresh, so a second try would fail the same way.
fn retryable_status(code: u16) -> bool {
    matches!(code, 429 | 500 | 502 | 503 | 504)
}

/// GET one object, signed. `key` is already percent-encoded by the
/// caller (the same segment-wise encoding as the HTTP mode). We decode
/// it and encode it again with the stricter SigV4 rules.
pub(crate) async fn fetch(bucket: &str, key: &str) -> Result<reqwest::Response> {
    let raw_key = percent_decode(key);
    if raw_key.len() > S3_MAX_KEY_BYTES {
        return Err(anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "object key is {} bytes, over the {S3_MAX_KEY_BYTES}-byte S3 limit",
                raw_key.len()
            ),
        )));
    }
    if has_dot_segment(&raw_key) {
        return Err(anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "object key has a . or .. segment, which cannot be fetched as signed",
        )));
    }
    // A settings error is a deployment fault, the same class as the
    // store reporting one: `PermissionDenied`, so `SourceUnreadable`.
    let s = settings().map_err(|e| {
        anyhow::Error::new(std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))
    })?;
    let target = Target::new(s, bucket, &raw_key);
    // One retry, on connection transients and on `retryable_status`.
    // Unlike `gs://`, a 401 is not retried.
    let first = send_signed(s, &target).await;
    let retry = match &first {
        Ok(resp) => retryable_status(resp.status().as_u16()),
        Err(e) => !e.is_timeout() && (e.is_connect() || e.is_request()),
    };
    let resp = if retry {
        super::UPSTREAM_RETRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(100)).await;
        send_signed(s, &target).await
    } else {
        first
    }
    .map_err(map_transport_err)?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = read_error_body(resp).await;
    Err(status_error(status, body.code.as_deref(), bucket))
}

/// Transport failures: timeouts keep their io shape for
/// classification, everything else indicts the upstream.
fn map_transport_err(e: reqwest::Error) -> anyhow::Error {
    if e.is_timeout() {
        return anyhow::Error::new(std::io::Error::new(std::io::ErrorKind::TimedOut, e));
    }
    anyhow::Error::new(e)
        .context("fetch s3 object")
        .context(UpstreamFault)
}

/// Error `<Code>` values that are the requester's fault: the key is
/// not a valid object name. Each store has its own code for this (AWS,
/// R2, MinIO, in that order). The local key-length check usually
/// answers before the store can.
const REQUESTER_CODES: &[&str] = &[
    "KeyTooLongError",
    "InvalidObjectName",
    "XMinioInvalidObjectName",
];

/// Map an error answer to the crate's error shapes. We read the
/// `<Code>` because the status alone is not enough. The stores also use
/// 400 for faults in our own settings, such as a wrong region or a
/// malformed access key (`MEASURED` in the tests lists what each store
/// sends). So a 4xx is a deployment fault unless the code says the key
/// is at fault.
///
/// - A 404 is an absent object: a 404 to the client. The exception is
///   404 `NoSuchBucket`, a deployment fault.
/// - 400/414 with a code from `REQUESTER_CODES`, or with no code, is
///   the store refusing an impossible key: the requester's fault, as in
///   `gs://` (#13).
/// - Any other 4xx, and any redirect, is a deployment fault. On AWS, a
///   key without `s3:ListBucket` gets 403 for a missing object, and the
///   code cannot tell that apart from a real permission problem.
/// - 429 (after the retry) and 5xx are upstream faults.
///
/// Deployment faults use `PermissionDenied`, which classifies as
/// `SourceUnreadable` (HTTP 500).
fn status_error(status: reqwest::StatusCode, code: Option<&str>, bucket: &str) -> anyhow::Error {
    let answer = match code {
        Some(code) => format!("{status} {code}"),
        None => status.to_string(),
    };
    let deployment = |hint: String| {
        anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("object store answered {answer}: {hint}"),
        ))
    };
    let requester =
        || anyhow::anyhow!("object store rejected the key ({answer})").context(SourceRejected);
    let upstream = || {
        anyhow::anyhow!("object store answered {answer}")
            .context("fetch s3 object")
            .context(UpstreamFault)
    };
    match (status.as_u16(), code) {
        (404, Some("NoSuchBucket")) => {
            deployment(format!("bucket {bucket:?} does not exist at this endpoint"))
        }
        (404, _) => anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "object not found in bucket",
        )),
        (400 | 414, None) => requester(),
        (400 | 414, Some(code)) if REQUESTER_CODES.contains(&code) => requester(),
        _ if status.is_redirection() => {
            deployment("check AWS_REGION and OXIMG_S3_ENDPOINT (redirects are not followed)".into())
        }
        (429, _) => upstream(),
        (400..=499, _) => deployment(deployment_hint(status.as_u16(), code, bucket)),
        _ => upstream(),
    }
}

/// What an operator should check, by error code.
fn deployment_hint(status: u16, code: Option<&str>, bucket: &str) -> String {
    match code {
        Some("ExpiredToken") => "AWS_SESSION_TOKEN has expired. Temporary keys are not \
                                 refreshed, so restart with new keys"
            .into(),
        Some("RequestTimeTooSkewed") => {
            "this host's clock is more than 15 minutes off. Check its time sync".into()
        }
        Some(
            "AuthorizationHeaderMalformed"
            | "InvalidRegionName"
            | "InvalidArgument"
            | "InvalidAccessKeyId"
            | "SignatureDoesNotMatch"
            | "InvalidToken",
        ) => "check AWS_REGION, OXIMG_S3_ENDPOINT and the access keys".into(),
        _ if matches!(status, 401 | 403) => format!(
            "access to bucket {bucket:?} denied (check that the bucket exists, and that the key \
             has s3:GetObject, and s3:ListBucket so that a missing object answers 404)"
        ),
        _ => "the store refused the request. This is not the requester's fault".into(),
    }
}

/// The parts of an S3 error body we use.
#[derive(Default)]
struct ErrorBody {
    code: Option<String>,
    message: Option<String>,
}

/// Error bodies are a few hundred bytes. We stop reading after this
/// many, so a broken store cannot make us buffer a large body.
const ERROR_BODY_LIMIT: usize = 16 * 1024;

async fn read_error_body(mut resp: reqwest::Response) -> ErrorBody {
    let mut body = Vec::new();
    while body.len() < ERROR_BODY_LIMIT
        && let Ok(Some(chunk)) = resp.chunk().await
    {
        body.extend_from_slice(&chunk);
    }
    parse_error_body(&body)
}

/// Read `<Code>` and `<Message>` from an S3 error body. These two
/// elements never nest, so a plain search is enough, and we need no XML
/// crate. Both values end up in logs, so we only keep
/// printable ASCII, and the code must look like a code.
fn parse_error_body(body: &[u8]) -> ErrorBody {
    let text = String::from_utf8_lossy(body);
    let element = |name: &str| -> Option<String> {
        let open = format!("<{name}>");
        let start = text.find(&open)? + open.len();
        let end = start + text[start..].find(&format!("</{name}>"))?;
        Some(unescape_xml(&text[start..end]))
    };
    let code = element("Code").filter(|c| {
        !c.is_empty() && c.len() <= 64 && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
    });
    let message = element("Message").map(|m| {
        m.chars()
            .filter(|c| c.is_ascii_graphic() || *c == ' ')
            .take(300)
            .collect()
    });
    ErrorBody { code, message }
}

/// Replace the five predefined XML entities and the numeric form that
/// stores use for `'` (MinIO sends `&#39;`). Other `&` sequences stay as
/// they are.
fn unescape_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let entity = [
            ("&lt;", '<'),
            ("&gt;", '>'),
            ("&amp;", '&'),
            ("&quot;", '"'),
            ("&apos;", '\''),
            ("&#39;", '\''),
        ]
        .into_iter()
        .find(|(name, _)| rest.starts_with(name));
        match entity {
            Some((name, ch)) => {
                out.push(ch);
                rest = &rest[name.len()..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The key the boot probe asks for. It should not exist. If it does,
/// the probe still passes.
const PROBE_KEY: &str = ".oximg-startup-probe";

/// Boot probe: fail closed with a clear message instead of a 500 on the
/// first cache miss. Unlike `gs://`, which only proves that credentials
/// exist, a signed GET of a missing key also checks the endpoint, the
/// region and the keys, and on AWS and MinIO the bucket. The expected
/// answer is 404 `NoSuchKey`. The key is under the prefix, where reads
/// go, so a key limited to that prefix passes.
///
/// One answer only warns. 403 `AccessDenied` is what AWS sends for a
/// missing key when the key lacks `s3:ListBucket`, and reads can still
/// work. It can also mean that the key cannot read this bucket, or (on
/// R2) that the bucket does not exist, so the warning says both.
pub(crate) fn startup(bucket: &str, prefix: Option<&str>) -> Result<(), String> {
    let s = settings()?;
    if s.scheme == "http" {
        eprintln!(
            "oximg: warning: OXIMG_S3_ENDPOINT uses http://, so requests, session tokens and \
             images are not encrypted, and a signed request can be replayed. Use https:// \
             outside a trusted network."
        );
    }
    if prefix.is_some_and(|p| has_dot_segment(p.as_bytes())) {
        return Err(format!(
            "s3:// prefix {prefix:?} has a . or .. segment, which cannot be fetched as signed"
        ));
    }
    let key = match prefix {
        Some(p) => format!("{p}/{PROBE_KEY}"),
        None => PROBE_KEY.to_string(),
    };
    let target = Target::new(s, bucket, key.as_bytes());
    // `block_on_fetch` runs the future on another thread, so it must
    // own what it uses.
    super::block_on_fetch(probe(s, bucket.to_string(), target))
}

async fn probe(s: &'static Settings, bucket: String, target: Target) -> Result<(), String> {
    let fail = |what: String| {
        format!(
            "s3:// boot probe of bucket {bucket:?} at {}://{} (region {:?}) {what}",
            s.scheme, s.host, s.region
        )
    };
    let resp = send_signed(s, &target).await.map_err(|e| {
        fail(format!(
            "could not reach the endpoint: {:#}",
            anyhow::Error::new(e)
        ))
    })?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = read_error_body(resp).await;
    match (status.as_u16(), body.code.as_deref()) {
        (404, Some("NoSuchKey")) => Ok(()),
        (403, Some("AccessDenied")) => {
            eprintln!(
                "oximg: warning: s3:// boot probe of bucket {bucket:?} got 403 AccessDenied \
                 for a missing key. If the key lacks s3:ListBucket, reads work but a missing \
                 object answers 500, not 404. If it lacks s3:GetObject on this bucket or \
                 prefix, or the bucket does not exist (R2 answers 403 for that), every read \
                 will fail."
            );
            Ok(())
        }
        _ => {
            let code = body.code.as_deref().unwrap_or("(no code)");
            let message = body.message.as_deref().unwrap_or("");
            Err(fail(format!("got {status} {code}: {message}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These vectors come from the AWS SigV4 test suite in aws-c-auth
    // (Apache-2.0), in tests/aws-signing-test-suite/v4/<name>/. They all
    // use the suite's example credentials, us-east-1, the service name
    // "service", and the time 2015-08-30T12:36:00Z.
    const KEY_ID: &str = "AKIDEXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const DATETIME: &str = "20150830T123600Z";
    const HOST: &str = "example.amazonaws.com";

    fn creds(token: Option<&str>) -> Credentials {
        Credentials {
            access_key_id: KEY_ID.into(),
            secret_access_key: SECRET.into(),
            session_token: token.map(Into::into),
        }
    }

    /// Sign a suite request. It is a GET of `raw_path` with no body. It
    /// signs `host` and `x-amz-date`, and the token when there is one.
    fn sign_suite(raw_path: &str, token: Option<&str>) -> String {
        let uri = uri_encode_path(raw_path.as_bytes());
        let mut headers = vec![("host", HOST), ("x-amz-date", DATETIME)];
        if let Some(t) = token {
            headers.push(("x-amz-security-token", t));
        }
        let req = Request {
            method: "GET",
            canonical_uri: &uri,
            canonical_query: "",
            headers: &headers,
            payload_hash: EMPTY_SHA256,
        };
        authorization(&req, &creds(token), "us-east-1", "service", DATETIME)
    }

    fn signature_of(auth: &str) -> &str {
        auth.rsplit_once("Signature=").unwrap().1
    }

    #[test]
    fn get_vanilla() {
        let auth = sign_suite("/", None);
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn get_vanilla_with_session_token() {
        let token = "6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267";
        let auth = sign_suite("/", Some(token));
        assert!(auth.contains("SignedHeaders=host;x-amz-date;x-amz-security-token,"));
        assert_eq!(
            signature_of(&auth),
            "07ec1639c89043aa0e3e2de82b96708f198cceab042d4a97044c66dd9f74e7f8"
        );
    }

    #[test]
    fn get_unreserved() {
        let path = "/-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
        assert_eq!(uri_encode_path(path.as_bytes()), path);
        assert_eq!(
            signature_of(&sign_suite(path, None)),
            "07ef7494c76fa4850883e2b006601f940f8a34d404d0cfa977f52a65bbf5f24f"
        );
    }

    #[test]
    fn get_utf8() {
        assert_eq!(uri_encode_path("/\u{1234}".as_bytes()), "/%E1%88%B4");
        assert_eq!(
            signature_of(&sign_suite("/\u{1234}", None)),
            "8318018e0b0f223aa2bbf98705b62bb787dc9c0e678f255a891fd03141be5d85"
        );
    }

    // The "unnormalized" vectors follow the S3 rule. The signer keeps
    // `//` and `..` in the path and does not collapse them.

    #[test]
    fn get_space_unnormalized() {
        assert_eq!(uri_encode_path(b"/example space/"), "/example%20space/");
        assert_eq!(
            signature_of(&sign_suite("/example space/", None)),
            "652487583200325589f1fba4c7e578f72c47cb61beeca81406b39ddec1366741"
        );
    }

    #[test]
    fn get_slashes_unnormalized() {
        assert_eq!(
            signature_of(&sign_suite("//example//", None)),
            "87cca117541a147f6df867677d98a7d80dff226d2bfca9e4ffa899665623c7e5"
        );
    }

    #[test]
    fn get_relative_relative_unnormalized() {
        assert_eq!(
            signature_of(&sign_suite("/example1/example2/../..", None)),
            "dc33e0856fd4baca4d7aa2146c38958283844764f38c74252a333df5e613003b"
        );
    }

    /// The GET Object example from the S3 API reference ("Signature
    /// Version 4: authenticating requests using the Authorization
    /// header"). Unlike the suite vectors, it uses the service name
    /// `s3` and signs `x-amz-content-sha256`, as every request of ours
    /// does. It also signs a `range` header, which we never send.
    #[test]
    fn s3_get_object_example() {
        let creds = Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let datetime = "20130524T000000Z";
        let headers = [
            ("host", "examplebucket.s3.amazonaws.com"),
            ("range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", datetime),
        ];
        let req = Request {
            method: "GET",
            canonical_uri: "/test.txt",
            canonical_query: "",
            headers: &headers,
            payload_hash: EMPTY_SHA256,
        };
        assert_eq!(
            authorization(&req, &creds, "us-east-1", "s3", datetime),
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    /// Header order does not change the signature, because the signer
    /// sorts the headers.
    #[test]
    fn header_order_does_not_matter() {
        let req = |headers: &[(&str, &str)]| {
            let r = Request {
                method: "GET",
                canonical_uri: "/",
                canonical_query: "",
                headers,
                payload_hash: EMPTY_SHA256,
            };
            authorization(&r, &creds(None), "us-east-1", "service", DATETIME)
        };
        assert_eq!(
            req(&[("x-amz-date", DATETIME), ("host", HOST)]),
            req(&[("host", HOST), ("x-amz-date", DATETIME)])
        );
    }

    /// The HTTP mode keeps the sub-delims as they are. Here we must
    /// encode them. If we do not, S3 builds a different canonical URI
    /// and answers 403.
    #[test]
    fn sub_delims_are_encoded() {
        assert_eq!(
            uri_encode_path(b"/a+b!$&'()*,;=:@.jpg"),
            "/a%2Bb%21%24%26%27%28%29%2A%2C%3B%3D%3A%40.jpg"
        );
        assert_eq!(uri_encode_path(b"/100%.jpg"), "/100%25.jpg");
        // A key cannot add a query or a fragment to the URL we send.
        assert_eq!(uri_encode_path(b"/a?b#c.jpg"), "/a%3Fb%23c.jpg");
    }

    #[test]
    fn header_values_are_trimmed_and_folded() {
        assert_eq!(canonical_header_value("  a   b  c "), "a b c");
    }

    #[test]
    fn amz_datetime_formats_utc() {
        assert_eq!(amz_datetime(0), "19700101T000000Z");
        // The suite's timestamp, 2015-08-30T12:36:00Z.
        assert_eq!(amz_datetime(1_440_938_160), DATETIME);
        // Leap day, and the last second of a leap year.
        assert_eq!(amz_datetime(951_782_400), "20000229T000000Z");
        assert_eq!(amz_datetime(1_735_689_599), "20241231T235959Z");
    }

    fn settings_for(scheme: &str, host: &str, path_style: Option<bool>) -> Settings {
        Settings {
            scheme: scheme.into(),
            host: host.into(),
            region: "us-east-1".into(),
            path_style,
            custom_endpoint: false,
            credentials: creds(None),
        }
    }

    #[test]
    fn target_path_and_virtual_host_style() {
        let path = Target::new(
            &settings_for("http", "127.0.0.1:9000", Some(true)),
            "pics",
            b"a/b c.jpg",
        );
        assert_eq!(path.url, "http://127.0.0.1:9000/pics/a/b%20c.jpg");
        assert_eq!(path.host, "127.0.0.1:9000");
        assert_eq!(path.path, "/pics/a/b%20c.jpg");

        let vhost = Target::new(
            &settings_for("https", "s3.ap-northeast-1.amazonaws.com", None),
            "pics",
            b"a/b c.jpg",
        );
        assert_eq!(
            vhost.url,
            "https://pics.s3.ap-northeast-1.amazonaws.com/a/b%20c.jpg"
        );
        assert_eq!(vhost.host, "pics.s3.ap-northeast-1.amazonaws.com");
        assert_eq!(vhost.path, "/a/b%20c.jpg");

        // On AWS, a dotted bucket falls back to path style, unless
        // OXIMG_S3_PATH_STYLE says otherwise.
        let aws = settings_for("https", "s3.us-east-1.amazonaws.com", None);
        let dotted = Target::new(&aws, "img.example.com", b"x.jpg");
        assert_eq!(
            dotted.url,
            "https://s3.us-east-1.amazonaws.com/img.example.com/x.jpg"
        );
        let forced = settings_for("https", "s3.us-east-1.amazonaws.com", Some(false));
        assert_eq!(
            Target::new(&forced, "img.example.com", b"x.jpg").host,
            "img.example.com.s3.us-east-1.amazonaws.com"
        );
        // A custom endpoint defaults to path style.
        let custom = Settings {
            custom_endpoint: true,
            ..settings_for("https", "minio.internal", None)
        };
        assert_eq!(Target::new(&custom, "pics", b"x.jpg").path, "/pics/x.jpg");
    }

    #[test]
    fn dot_segments_are_found() {
        assert!(has_dot_segment(b"a/../b.jpg"));
        assert!(has_dot_segment(b"./b.jpg"));
        assert!(has_dot_segment(b"a/.."));
        assert!(!has_dot_segment(b"a/.b/c..jpg"));
        assert!(!has_dot_segment(b"a//b.jpg"));
    }

    #[test]
    fn endpoint_must_be_scheme_and_host() {
        assert_eq!(
            parse_endpoint("http://127.0.0.1:9000").unwrap(),
            ("http".into(), "127.0.0.1:9000".into())
        );
        // A trailing slash is fine. A default port is dropped, because
        // reqwest drops it from `Host` too.
        assert_eq!(
            parse_endpoint("https://example.r2.cloudflarestorage.com:443/").unwrap(),
            ("https".into(), "example.r2.cloudflarestorage.com".into())
        );
        for bad in [
            "s3.amazonaws.com",
            "ftp://host",
            "https://host/bucket",
            "https://host/?x=1",
            "https://user:pw@host",
        ] {
            assert!(parse_endpoint(bad).is_err(), "{bad} should be refused");
        }
    }

    /// The caller's encoding is undone byte for byte, and SigV4's
    /// encoding is applied on top. A lone `%` survives as a literal.
    #[test]
    fn caller_encoding_round_trips_to_sigv4() {
        assert_eq!(percent_decode("a%20b%2Bc+d"), b"a b+c+d");
        assert_eq!(percent_decode("%E4%B8%AD.jpg"), "\u{4e2d}.jpg".as_bytes());
        assert_eq!(percent_decode("100%.jpg"), b"100%.jpg");
        assert_eq!(percent_decode("bad%zzhex%4"), b"bad%zzhex%4");
        // `from_str_radix` would read "+1" as a number. This must not.
        assert_eq!(percent_decode("%+1"), b"%+1");
        assert_eq!(
            uri_encode_path(&percent_decode("plus+sign.jpg")),
            "plus%2Bsign.jpg"
        );
    }

    #[test]
    fn error_body_code_and_message() {
        let body = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>NoSuchKey</Code>\
            <Message>The specified key does not exist.</Message><Key>x.jpg</Key></Error>";
        let parsed = parse_error_body(body);
        assert_eq!(parsed.code.as_deref(), Some("NoSuchKey"));
        assert_eq!(
            parsed.message.as_deref(),
            Some("The specified key does not exist.")
        );
        // A code that does not look like a code is dropped, so a broken
        // store cannot put arbitrary text in our logs through it.
        let odd = parse_error_body(b"<Error><Code>a\nb</Code></Error>");
        assert_eq!(odd.code, None);
        assert_eq!(parse_error_body(b"").code, None);
    }

    #[test]
    fn xml_entities_are_unescaped() {
        assert_eq!(
            unescape_xml("expecting &#39;us-east-1&#39;"),
            "expecting 'us-east-1'"
        );
        assert_eq!(
            unescape_xml("a &lt;b&gt; &amp; &quot;c&quot;"),
            "a <b> & \"c\""
        );
        assert_eq!(unescape_xml("50% &unknown; &"), "50% &unknown; &");
        let body = b"<Error><Code>X</Code><Message>it&apos;s</Message></Error>";
        assert_eq!(parse_error_body(body).message.as_deref(), Some("it's"));
    }

    #[test]
    fn ip_literal_hosts() {
        assert!(is_ip_literal("127.0.0.1:9000"));
        assert!(is_ip_literal("10.0.0.5"));
        assert!(is_ip_literal("[::1]:9000"));
        assert!(!is_ip_literal("s3.us-east-1.amazonaws.com"));
        assert!(!is_ip_literal("minio.internal:9000"));
    }

    fn kind_of(status: u16, code: Option<&str>) -> crate::pipeline::ErrorKind {
        let status = reqwest::StatusCode::from_u16(status).unwrap();
        crate::pipeline::Error::classify(status_error(status, code, "pics"), true).kind()
    }

    /// What AWS S3, Cloudflare R2 and MinIO answered to signed requests
    /// (curl --aws-sigv4, 2026-09-29 to 10-01), one row per store where
    /// they differ. The same fault comes back as 400 from one store and
    /// 403 from another, so the status alone cannot say whose fault it
    /// is. The `<Code>` can.
    #[rustfmt::skip]
    const MEASURED: &[(&str, &str, u16, &str, crate::pipeline::ErrorKind)] = {
        use crate::pipeline::ErrorKind::*;
        &[
            ("missing key",                   "all three",  404, "NoSuchKey",                    SourceNotFound),
            ("missing key, no s3:ListBucket", "AWS",        403, "AccessDenied",                 SourceUnreadable),
            ("missing bucket",                "AWS, MinIO", 404, "NoSuchBucket",                 SourceUnreadable),
            ("missing bucket",                "R2",         403, "AccessDenied",                 SourceUnreadable),
            ("wrong region",                  "AWS, MinIO", 400, "AuthorizationHeaderMalformed", SourceUnreadable),
            ("wrong region",                  "R2",         400, "InvalidRegionName",            SourceUnreadable),
            ("malformed access key",          "AWS, MinIO", 403, "InvalidAccessKeyId",           SourceUnreadable),
            ("malformed access key",          "R2",         400, "InvalidArgument",              SourceUnreadable),
            ("wrong secret",                  "all three",  403, "SignatureDoesNotMatch",        SourceUnreadable),
            ("1029-byte key",                 "AWS",        400, "KeyTooLongError",              SourceRejected),
            ("1029-byte key",                 "R2",         400, "InvalidObjectName",            SourceRejected),
            ("1029-byte key",                 "MinIO",      400, "XMinioInvalidObjectName",      SourceRejected),
        ]
    };

    /// Codes from the S3 error code list that we could not cause on the
    /// three stores.
    #[rustfmt::skip]
    const FROM_THE_LIST: &[(&str, u16, &str, crate::pipeline::ErrorKind)] = {
        use crate::pipeline::ErrorKind::*;
        &[
            ("expired session token",        400, "ExpiredToken",         SourceUnreadable),
            ("clock skew",                   403, "RequestTimeTooSkewed", SourceUnreadable),
            ("SSE-C object without its key", 400, "InvalidRequest",       SourceUnreadable),
            ("throttling",                   503, "SlowDown",             Upstream),
        ]
    };

    #[test]
    fn statuses_map_by_code() {
        for &(case, store, status, code, want) in MEASURED {
            assert_eq!(kind_of(status, Some(code)), want, "{case} on {store}");
        }
        for &(case, status, code, want) in FROM_THE_LIST {
            assert_eq!(kind_of(status, Some(code)), want, "{case}");
        }
        use crate::pipeline::ErrorKind::*;
        // Without a code, the status decides, as in gs://.
        assert_eq!(kind_of(404, None), SourceNotFound);
        assert_eq!(kind_of(400, None), SourceRejected);
        assert_eq!(kind_of(414, None), SourceRejected);
        assert_eq!(kind_of(429, None), Upstream);
        assert_eq!(kind_of(500, None), Upstream);
        // Redirects are not followed: the region or endpoint is wrong.
        assert_eq!(kind_of(301, Some("PermanentRedirect")), SourceUnreadable);
        assert_eq!(kind_of(307, None), SourceUnreadable);
    }

    #[test]
    fn deployment_hints_name_the_cause() {
        assert!(deployment_hint(400, Some("ExpiredToken"), "b").contains("restart"));
        assert!(deployment_hint(403, Some("RequestTimeTooSkewed"), "b").contains("clock"));
        assert!(deployment_hint(403, Some("SignatureDoesNotMatch"), "b").contains("AWS_REGION"));
        let denied = deployment_hint(403, Some("AccessDenied"), "b");
        assert!(denied.contains("s3:ListBucket") && denied.contains("bucket exists"));
        assert!(deployment_hint(400, Some("InvalidRequest"), "b").contains("not the requester"));
    }

    #[test]
    fn retries_match_the_sdk_read_semantics() {
        for code in [429, 500, 502, 503, 504] {
            assert!(retryable_status(code));
        }
        for code in [400, 401, 403, 404] {
            assert!(!retryable_status(code));
        }
    }
}
