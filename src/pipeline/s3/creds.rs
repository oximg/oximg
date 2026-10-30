//! Where the signing keys come from, and keeping them fresh.
//!
//! `Chain::from_env` picks the first source whose settings are present,
//! in the order of `SOURCES`, as the AWS SDKs walk their default chain.
//! The keys are cached for the process and fetched again in the
//! background shortly before they expire.

use std::future::Future;
use std::net::Ipv6Addr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use super::Credentials;

/// Fetch keys this long before they expire. EC2 publishes new
/// instance-profile keys at least this early, so a shorter margin could
/// reuse keys that are about to stop working.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// Keys and when they stop working. `None` never expires.
pub(super) struct Keys {
    pub(super) credentials: Credentials,
    pub(super) expires_at: Option<SystemTime>,
}

pub(super) type KeysFuture<'a> = Pin<Box<dyn Future<Output = Result<Keys, CredError>> + Send + 'a>>;

/// Why keys could not be had. The fault decides the status a request
/// gets, as for the store itself: our settings are the deployment's
/// fault (500), a source that does not answer in time is a timeout
/// (504), and a source that is down is an upstream fault (502).
#[derive(Clone, Debug)]
pub(super) struct CredError {
    pub(super) fault: Fault,
    pub(super) message: String,
    /// The `<Code>` of an AWS XML error answer, as STS sends.
    pub(super) code: Option<String>,
    /// The HTTP status of an answer that was not 2xx.
    pub(super) status: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fault {
    Settings,
    TimedOut,
    Unavailable,
}

impl CredError {
    /// A fault in our settings or in what the source answered: a missing
    /// file, a role without keys, an answer we cannot read. Status 500.
    pub(super) fn settings(message: impl Into<String>) -> Self {
        Self::new(Fault::Settings, message.into())
    }

    fn new(fault: Fault, message: String) -> Self {
        Self {
            fault,
            message,
            code: None,
            status: None,
        }
    }

    /// The same error with `what` in front of the message.
    pub(super) fn context(self, what: impl std::fmt::Display) -> Self {
        Self {
            message: format!("{what}: {}", self.message),
            ..self
        }
    }
}

impl std::fmt::Display for CredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// One place keys can come from.
pub(super) trait Source: Send + Sync {
    /// Shown in errors, so an operator can tell which source failed.
    fn name(&self) -> &'static str;
    /// A boot warning about these settings, such as a plain-HTTP
    /// endpoint.
    fn warning(&self) -> Option<String> {
        None
    }
    fn fetch(&self) -> KeysFuture<'_>;
    /// Whether a second fetch can return different keys.
    fn refreshes(&self) -> bool {
        true
    }
}

/// A source's constructor: `Ok(None)` when its settings are absent,
/// `Err` when they are present but wrong.
type FromEnv = fn() -> Result<Option<Box<dyn Source>>, String>;

/// The chain, in order. `hint` lists what the source reads, for the
/// error when none of them is set.
const SOURCES: &[(FromEnv, &str)] = &[
    (
        Static::from_env,
        "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY",
    ),
    (
        super::container::from_env,
        "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI or _FULL_URI (ECS, EKS Pod Identity)",
    ),
    (
        super::imds::from_env,
        "an EC2 instance role (IMDS, off with AWS_EC2_METADATA_DISABLED=true)",
    ),
];

struct Cached {
    credentials: Arc<Credentials>,
    expires_at: Option<SystemTime>,
    fetched_at: Instant,
}

impl Cached {
    fn expired(&self, now: SystemTime) -> bool {
        self.expires_at.is_some_and(|t| now >= t)
    }
}

/// The last fetch that gave no usable keys.
struct Failure {
    at: Instant,
    error: CredError,
}

#[derive(Default)]
struct State {
    cached: Option<Cached>,
    failure: Option<Failure>,
    /// A fetch runs. There is never more than one.
    fetching: bool,
    /// Keys the store refused as expired while a fetch runs. That fetch
    /// must not hand them out again.
    refused: Option<Arc<Credentials>>,
}

pub(super) struct Chain {
    inner: Arc<Inner>,
}

/// What the fetch task shares with the requests. The task owns a clone,
/// so a request that stops waiting does not stop the fetch.
struct Inner {
    source: Box<dyn Source>,
    state: Mutex<State>,
    /// Wakes the requests that wait for a fetch.
    done: tokio::sync::Notify,
    /// `MIN_REUSE`, except in tests.
    min_reuse: Duration,
}

/// What a request does next.
enum Step {
    Ready(Result<Arc<Credentials>, CredError>),
    /// Wait for the running fetch.
    Wait,
    /// No runtime to spawn on: run the fetch here.
    Run(Pin<Box<dyn Future<Output = ()> + Send>>),
}

impl Chain {
    pub(super) fn warning(&self) -> Option<String> {
        self.inner.source.warning()
    }

    pub(super) fn from_env() -> Result<Self, String> {
        for (from_env, _) in SOURCES {
            if let Some(source) = from_env()? {
                return Ok(Self::new(source));
            }
        }
        let hints: Vec<&str> = SOURCES.iter().map(|(_, hint)| *hint).collect();
        Err(format!(
            "s3:// needs credentials: set {}",
            hints.join(", or ")
        ))
    }

    pub(super) fn new(source: Box<dyn Source>) -> Self {
        Self::with_min_reuse(source, MIN_REUSE)
    }

    fn with_min_reuse(source: Box<dyn Source>, min_reuse: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                source,
                state: Mutex::new(State::default()),
                done: tokio::sync::Notify::new(),
                min_reuse,
            }),
        }
    }

    pub(super) fn source_name(&self) -> &'static str {
        self.inner.source.name()
    }

    pub(super) fn refreshes(&self) -> bool {
        self.inner.source.refreshes()
    }

    /// The keys to sign with.
    ///
    /// Keys that still work are returned at once. When they are inside
    /// the refresh margin, a fetch starts in the background, so no
    /// request waits for it. Only a request without working keys waits,
    /// and all such requests wait for the same fetch.
    ///
    /// `refused` is the keys a request just used when the store said they
    /// expired. They are replaced only if the cache still holds them: when
    /// many requests fail at once, the first one fetches and the rest get
    /// its keys.
    ///
    /// A fetch that gives no usable keys (an error, keys that already
    /// expired, or the keys the store just refused) is not tried again for
    /// `min_reuse`. Until then, requests sign with the cached keys while
    /// they have not expired, or get the same error. So a source that is
    /// down or slow costs one fetch per `min_reuse`, not one per request.
    pub(super) async fn current(
        &self,
        refused: Option<&Arc<Credentials>>,
    ) -> Result<Arc<Credentials>, CredError> {
        let mut waited = false;
        loop {
            // The lock is released at the end of this block, before any
            // await.
            let step = {
                let mut state = self.inner.lock();
                match self.inner.next(&mut state, refused, waited) {
                    Step::Ready(result) => return result,
                    Step::Run(fetch) => Err(fetch),
                    Step::Wait => Ok(self.inner.done.notified()),
                }
            };
            match step {
                Ok(wake) => wake.await,
                Err(fetch) => fetch.await,
            }
            waited = true;
        }
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `waited` says a fetch has finished since the request asked. Its
    /// result is the answer, so a failed fetch is not started again.
    fn next(
        self: &Arc<Self>,
        state: &mut State,
        refused: Option<&Arc<Credentials>>,
        waited: bool,
    ) -> Step {
        let now = Instant::now();
        let cooling = state
            .failure
            .as_ref()
            .is_some_and(|f| now < f.at + self.min_reuse);
        if let Some(c) = &state.cached {
            let usable = match refused {
                Some(r) => !Arc::ptr_eq(r, &c.credentials),
                None => !c.expired(SystemTime::now()),
            };
            if usable {
                let keys = c.credentials.clone();
                if refused.is_none()
                    && !cooling
                    && !state.fetching
                    && refresh_due(c, SystemTime::now(), now, self.min_reuse)
                    && let Some(fetch) = self.start(state, None)
                {
                    return Step::Run(fetch);
                }
                return Step::Ready(Ok(keys));
            }
        }
        if (cooling || waited)
            && let Some(f) = &state.failure
        {
            return Step::Ready(Err(f.error.clone()));
        }
        if state.fetching {
            if refused.is_some() {
                state.refused = refused.cloned();
            }
            return Step::Wait;
        }
        match self.start(state, refused.cloned()) {
            Some(fetch) => Step::Run(fetch),
            None => Step::Wait,
        }
    }

    /// Start a fetch on the runtime. Without one, return it for the
    /// caller to run.
    fn start(
        self: &Arc<Self>,
        state: &mut State,
        refused: Option<Arc<Credentials>>,
    ) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        state.fetching = true;
        state.refused = refused;
        let fetch = Box::pin(self.clone().fetch());
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(fetch);
                None
            }
            Err(_) => Some(fetch),
        }
    }

    async fn fetch(self: Arc<Self>) {
        /// Records a failure if the fetch ends without a result: a
        /// panic, or a runtime that shuts down. Otherwise every request
        /// would wait for a fetch that never finishes.
        struct Unfinished(Option<Arc<Inner>>);
        impl Drop for Unfinished {
            fn drop(&mut self) {
                if let Some(inner) = self.0.take() {
                    inner.finish(Err(CredError::new(
                        Fault::Unavailable,
                        "the fetch stopped before it finished".into(),
                    )));
                }
            }
        }
        let mut unfinished = Unfinished(Some(self.clone()));
        let result = self.source.fetch().await;
        unfinished.0 = None;
        self.finish(result);
    }

    fn finish(&self, result: Result<Keys, CredError>) {
        let mut state = self.lock();
        state.fetching = false;
        let refused = state.refused.take();
        let name = self.source.name();
        let error = match result {
            Ok(keys)
                if refused
                    .as_ref()
                    .is_some_and(|r| same_keys(r, &keys.credentials)) =>
            {
                CredError::settings(format!(
                    "s3:// credentials from {name}: the source returned the keys that the \
                     store refused as expired"
                ))
            }
            Ok(keys) if keys.expires_at.is_some_and(|t| t <= SystemTime::now()) => {
                CredError::settings(format!(
                    "s3:// credentials from {name}: the source returned keys that have \
                     already expired. Check this host's clock"
                ))
            }
            Ok(keys) => {
                state.cached = Some(Cached {
                    credentials: Arc::new(keys.credentials),
                    expires_at: keys.expires_at,
                    fetched_at: Instant::now(),
                });
                state.failure = None;
                drop(state);
                self.done.notify_waiters();
                return;
            }
            Err(e) => e.context(format_args!("s3:// credentials from {name}")),
        };
        if refused.is_none()
            && let Some(c) = &state.cached
            && !c.expired(SystemTime::now())
        {
            eprintln!("oximg: warning: {error}; using the cached keys until they expire");
        }
        state.failure = Some(Failure {
            at: Instant::now(),
            error,
        });
        drop(state);
        self.done.notify_waiters();
    }
}

/// Keys fetched less than this long ago are not fetched again, even
/// inside the refresh margin, and a fetch that failed is not tried again
/// for this long. Keys that arrive with less than the margin left (a fast
/// host clock, or a source that refreshes late) or a source that is down
/// would otherwise cost one fetch per request.
const MIN_REUSE: Duration = Duration::from_secs(60);

/// Whether the cached keys are due for a fetch: inside the margin, and
/// not fetched within `min_reuse`.
fn refresh_due(c: &Cached, now: SystemTime, at: Instant, min_reuse: Duration) -> bool {
    c.expires_at.is_some_and(|t| now + REFRESH_MARGIN >= t) && at >= c.fetched_at + min_reuse
}

/// Whether a fetch returned the keys a request already used.
fn same_keys(a: &Credentials, b: &Credentials) -> bool {
    a.access_key_id == b.access_key_id && a.session_token == b.session_token
}

/// `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, plus
/// `AWS_SESSION_TOKEN` for temporary keys. They do not expire as far as
/// we know: a session token that runs out fails the request, and a new
/// fetch would only read the same values again.
struct Static(Credentials);

impl Static {
    fn from_env() -> Result<Option<Box<dyn Source>>, String> {
        let id = super::aws_var(std::env::var("AWS_ACCESS_KEY_ID"));
        let secret = super::aws_var(std::env::var("AWS_SECRET_ACCESS_KEY"));
        match (id, secret) {
            (Some(access_key_id), Some(secret_access_key)) => {
                Ok(Some(Box::new(Static(Credentials {
                    access_key_id,
                    secret_access_key,
                    session_token: super::aws_var(std::env::var("AWS_SESSION_TOKEN")),
                }))))
            }
            (None, None) => Ok(None),
            _ => Err("s3:// needs both AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY".into()),
        }
    }
}

impl Source for Static {
    fn name(&self) -> &'static str {
        "static keys"
    }

    fn refreshes(&self) -> bool {
        false
    }

    fn fetch(&self) -> KeysFuture<'_> {
        let credentials = self.0.clone();
        Box::pin(async move {
            Ok(Keys {
                credentials,
                expires_at: None,
            })
        })
    }
}

/// Credential answers are well under a kilobyte. A larger one is a
/// fault, not keys.
const BODY_LIMIT: usize = 16 * 1024;

/// The client for a credential endpoint at `url`.
///
/// The agents and IMDS live on link-local or loopback addresses, or on
/// their `fd00:ec2::` IPv6 addresses. Requests to them skip `HTTP_PROXY` and
/// friends: a proxy would see the session token, the authorization
/// token and the keys in clear, and these addresses are only reachable
/// from the host anyway. Any other host (STS, an https container
/// endpoint) goes through the shared client, proxy and all, because
/// some networks reach the internet only through one. Neither follows
/// redirects.
pub(super) fn client_for(url: &str) -> &'static reqwest::Client {
    if host_is_local(url) {
        static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
        CLIENT.get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("construct the credential client")
        })
    } else {
        super::super::fetch_client()
    }
}

/// The EKS Pod Identity Agent's IPv6 address.
pub(super) const EKS_AGENT_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x23);

/// IMDS's IPv6 address, on Nitro instances.
const IMDS_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254);

/// Whether `url`'s host is loopback, link-local, the EKS agent or IMDS.
pub(super) fn host_is_local(url: &str) -> bool {
    use std::net::IpAddr;
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    if host == "localhost" {
        return true;
    }
    match host_ip(host) {
        Some(IpAddr::V4(ip)) => ip.is_loopback() || ip.is_link_local(),
        Some(IpAddr::V6(ip)) => {
            ip.is_loopback() || ip.is_unicast_link_local() || [EKS_AGENT_V6, IMDS_V6].contains(&ip)
        }
        None => false,
    }
}

/// `host` as an IP address. An IPv6 host comes in brackets.
pub(super) fn host_ip(host: &str) -> Option<std::net::IpAddr> {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// Send one request to a credential endpoint and read its answer. Each
/// request has its own `timeout`, so a stalled endpoint fails the fetch
/// instead of leaving every request that needs keys waiting.
///
/// A connection error, 429 or 5xx gets one more try, as S3 requests do
/// in `send_with_retry`. A failed fetch is not tried again for
/// `MIN_REUSE`, so without this one blip would cost a minute without
/// keys. A timeout is not tried again: a stalled source costs one
/// `timeout`, not two.
pub(super) async fn send(
    req: reqwest::RequestBuilder,
    timeout: Duration,
) -> Result<Vec<u8>, CredError> {
    // A form or empty body always clones.
    let again = req.try_clone();
    let first = req.timeout(timeout).send().await;
    let retry = match &first {
        Ok(resp) => resp.status().as_u16() == 429 || resp.status().is_server_error(),
        Err(e) => !e.is_timeout() && (e.is_connect() || e.is_request()),
    };
    let resp = match again {
        Some(again) if retry => {
            // Release the failed response before waiting.
            drop(first);
            tokio::time::sleep(Duration::from_millis(100)).await;
            again.timeout(timeout).send().await
        }
        _ => first,
    }
    .map_err(transport)?;
    read_body(resp).await
}

/// A transport error: a timeout, or a source we could not reach.
fn transport(e: reqwest::Error) -> CredError {
    let fault = if e.is_timeout() {
        Fault::TimedOut
    } else {
        Fault::Unavailable
    };
    // reqwest puts the full URL in its message, userinfo and query
    // included. The caller adds a URL without them.
    CredError::new(fault, format!("{:#}", anyhow::Error::new(e.without_url())))
}

/// Read a credential endpoint's answer, refusing anything over
/// `BODY_LIMIT` and any status other than 2xx. 429 and 5xx are the
/// source being down; other statuses are our settings.
async fn read_body(resp: reqwest::Response) -> Result<Vec<u8>, CredError> {
    let status = resp.status();
    let body = super::read_capped(resp, BODY_LIMIT)
        .await
        .map_err(|e| match e {
            super::ReadError::TooLarge => CredError::new(
                Fault::Unavailable,
                format!("{status} answer is over {BODY_LIMIT} bytes"),
            ),
            super::ReadError::Transport(e) => transport(e),
        })?;
    if !status.is_success() {
        let fault = if status.as_u16() == 429 || status.is_server_error() {
            Fault::Unavailable
        } else {
            Fault::Settings
        };
        // An AWS XML error (STS): show its code and message, not the XML.
        let parsed = super::parse_error_body(&body);
        let mut e = match parsed.code {
            Some(code) => {
                let message = parsed.message.unwrap_or_default();
                let mut e = CredError::new(fault, format!("{status} {code}: {message}"));
                e.code = Some(code);
                e
            }
            None => {
                let text = String::from_utf8_lossy(&body);
                let shown: String = text
                    .chars()
                    .filter(|c| c.is_ascii_graphic() || *c == ' ')
                    .take(200)
                    .collect();
                CredError::new(fault, format!("{status}: {shown}"))
            }
        };
        e.status = Some(status.as_u16());
        return Err(e);
    }
    Ok(body)
}

/// Keys in the JSON shape that the container endpoint and IMDS share:
/// `AccessKeyId`, `SecretAccessKey`, `Token` and `Expiration`. IMDS
/// also sends `Code`, which is `Success` when the keys are good.
pub(super) fn keys_from_json(body: &[u8]) -> Result<Keys, String> {
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("answer is not JSON ({e})"))?;
    if let Some(code) = v["Code"].as_str()
        && code != "Success"
    {
        return Err(format!("answer has Code {code:?}"));
    }
    let field = |name: &str| -> Result<String, String> {
        v[name]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("answer has no {name}"))
    };
    Ok(Keys {
        credentials: Credentials {
            access_key_id: field("AccessKeyId")?,
            secret_access_key: field("SecretAccessKey")?,
            session_token: field("Token").ok(),
        },
        expires_at: Some(parse_expiration(&field("Expiration")?)?),
    })
}

/// Parse an `Expiration` such as `2026-10-28T13:00:00Z`. AWS sends UTC
/// with a `Z`, sometimes with fractional seconds, which we drop.
pub(super) fn parse_expiration(text: &str) -> Result<SystemTime, String> {
    let bad = || format!("Expiration {text:?} is not a UTC time");
    let rest = text.strip_suffix('Z').ok_or_else(bad)?;
    let rest = rest.split_once('.').map_or(rest, |(whole, _)| whole);
    let (date, time) = rest.split_once('T').ok_or_else(bad)?;
    let num = |s: &str| s.parse::<i64>().map_err(|_| bad());
    let d: Vec<&str> = date.split('-').collect();
    let t: Vec<&str> = time.split(':').collect();
    let ([y, mo, da], [h, mi, se]) = (d.as_slice(), t.as_slice()) else {
        return Err(bad());
    };
    let (y, mo, da, h, mi, se) = (num(y)?, num(mo)?, num(da)?, num(h)?, num(mi)?, num(se)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || h > 23 || mi > 59 || se > 60 {
        return Err(bad());
    }
    let secs = days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60 + se;
    let secs = u64::try_from(secs).map_err(|_| bad())?;
    Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

/// Days since 1970-01-01 for a proleptic Gregorian date. The inverse of
/// `civil_from_days` in `s3.rs`.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Fixed keys, for tests that build `Settings` by hand.
#[cfg(test)]
pub(super) fn fixed(credentials: Credentials) -> Chain {
    Chain::new(Box::new(Static(credentials)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Hands out `key-0`, `key-1`, ... Fetch number `fail_from` and later
    /// ones fail.
    struct Counting {
        fetches: Arc<AtomicUsize>,
        lifetime: Option<Duration>,
        delay: Duration,
        fail_from: usize,
    }

    impl Source for Counting {
        fn name(&self) -> &'static str {
            "counting"
        }

        fn fetch(&self) -> KeysFuture<'_> {
            let n = self.fetches.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                tokio::time::sleep(self.delay).await;
                if n >= self.fail_from {
                    return Err(CredError::new(Fault::Unavailable, "down".into()));
                }
                Ok(Keys {
                    credentials: Credentials {
                        access_key_id: format!("key-{n}"),
                        secret_access_key: "secret".into(),
                        session_token: None,
                    },
                    expires_at: self.lifetime.map(|d| SystemTime::now() + d),
                })
            })
        }
    }

    fn counting(lifetime: Option<Duration>) -> Counting {
        Counting {
            fetches: Arc::new(AtomicUsize::new(0)),
            lifetime,
            delay: Duration::ZERO,
            fail_from: usize::MAX,
        }
    }

    /// A chain with the given minimum reuse, and its fetch counter.
    fn chain_with(source: Counting, min_reuse: Duration) -> (Arc<Chain>, Arc<AtomicUsize>) {
        let fetches = source.fetches.clone();
        let chain = Chain::with_min_reuse(Box::new(source), min_reuse);
        (Arc::new(chain), fetches)
    }

    /// A chain with no minimum reuse, so every call checks the margin.
    fn chain(source: Counting) -> (Arc<Chain>, Arc<AtomicUsize>) {
        chain_with(source, Duration::ZERO)
    }

    /// Let a background fetch run to its end.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn keys_inside_the_margin_are_fetched_again() {
        // Four minutes left is inside the margin.
        let (chain, fetches) = chain(counting(Some(Duration::from_secs(4 * 60))));
        chain.current(None).await.unwrap();
        // The request gets the keys it has; the fetch runs behind it.
        assert_eq!(chain.current(None).await.unwrap().access_key_id, "key-0");
        settle().await;
        assert_eq!(chain.current(None).await.unwrap().access_key_id, "key-1");
        assert!(fetches.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn keys_inside_the_margin_are_reused_for_a_minute() {
        let (chain, fetches) = chain_with(counting(Some(Duration::from_secs(4 * 60))), MIN_REUSE);
        chain.current(None).await.unwrap();
        assert_eq!(chain.current(None).await.unwrap().access_key_id, "key-0");
        settle().await;
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_fetch() {
        let mut source = counting(Some(Duration::from_secs(60 * 60)));
        source.delay = Duration::from_millis(50);
        let (chain, fetches) = chain(source);
        let calls: Vec<_> = (0..10)
            .map(|_| {
                let chain = chain.clone();
                tokio::spawn(async move { chain.current(None).await.unwrap() })
            })
            .collect();
        for call in calls {
            assert_eq!(call.await.unwrap().access_key_id, "key-0");
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    /// Keys that still work, inside the margin, and a slow source: no
    /// request waits for the refresh, and only one refresh runs.
    #[tokio::test]
    async fn a_refresh_does_not_hold_up_requests() {
        let mut source = counting(Some(Duration::from_secs(4 * 60)));
        source.delay = Duration::from_millis(300);
        let (chain, fetches) = chain(source);
        chain.current(None).await.unwrap();
        let start = Instant::now();
        let calls: Vec<_> = (0..10)
            .map(|_| {
                let chain = chain.clone();
                tokio::spawn(async move { chain.current(None).await.unwrap() })
            })
            .collect();
        for call in calls {
            assert_eq!(call.await.unwrap().access_key_id, "key-0");
        }
        assert!(start.elapsed() < Duration::from_millis(150));
        settle().await;
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
    }

    /// A request that stops waiting does not stop the fetch: its result
    /// is kept, and the next request does not fetch again.
    #[tokio::test]
    async fn a_dropped_request_does_not_lose_the_fetch() {
        let mut source = counting(Some(Duration::from_secs(60 * 60)));
        source.delay = Duration::from_millis(100);
        let (chain, fetches) = chain_with(source, MIN_REUSE);
        let gave_up = tokio::time::timeout(Duration::from_millis(20), chain.current(None)).await;
        assert!(gave_up.is_err());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(chain.current(None).await.unwrap().access_key_id, "key-0");
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_keys_are_replaced_once() {
        let (chain, fetches) = chain(counting(Some(Duration::from_secs(60 * 60))));
        let first = chain.current(None).await.unwrap();
        for _ in 0..5 {
            let k = chain.current(Some(&first)).await.unwrap();
            assert_eq!(k.access_key_id, "key-1");
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
    }

    /// Many requests that S3 told "expired" at once, as after keys are
    /// replaced early: one fetch, and they all sign with its keys.
    #[tokio::test]
    async fn concurrent_failures_share_one_fetch() {
        let mut source = counting(Some(Duration::from_secs(60 * 60)));
        source.delay = Duration::from_millis(50);
        let (chain, fetches) = chain(source);
        let first = chain.current(None).await.unwrap();
        let calls: Vec<_> = (0..10)
            .map(|_| {
                let (chain, first) = (chain.clone(), first.clone());
                tokio::spawn(async move { chain.current(Some(&first)).await.unwrap() })
            })
            .collect();
        for call in calls {
            assert_eq!(call.await.unwrap().access_key_id, "key-1");
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_keys_that_still_work() {
        let mut source = counting(Some(Duration::from_secs(4 * 60)));
        source.fail_from = 1;
        let (chain, fetches) = chain(source);
        let first = chain.current(None).await.unwrap();
        assert_eq!(chain.current(None).await.unwrap().access_key_id, "key-0");
        settle().await;
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
        assert_eq!(chain.current(None).await.unwrap().access_key_id, "key-0");
        // Keys the store refused are not kept.
        assert!(chain.current(Some(&first)).await.is_err());
    }

    /// The source fails, slowly, while the cached keys still work: ten
    /// requests at once sign with the cached keys while one fetch fails,
    /// and the next request does not fetch at all.
    #[tokio::test]
    async fn a_failed_refresh_is_not_tried_again_at_once() {
        let mut source = counting(Some(Duration::from_secs(4 * 60)));
        source.fail_from = 1;
        source.delay = Duration::from_millis(50);
        let (chain, fetches) = chain_with(source, Duration::from_millis(200));
        chain.current(None).await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        let calls: Vec<_> = (0..10)
            .map(|_| {
                let chain = chain.clone();
                tokio::spawn(async move { chain.current(None).await.unwrap() })
            })
            .collect();
        for call in calls {
            assert_eq!(call.await.unwrap().access_key_id, "key-0");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
        chain.current(None).await.unwrap();
        settle().await;
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
    }

    /// A source that hands out the same keys every time, as IMDS does
    /// until it rotates them.
    struct Same(Arc<AtomicUsize>);

    impl Source for Same {
        fn name(&self) -> &'static str {
            "same"
        }

        fn fetch(&self) -> KeysFuture<'_> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(Keys {
                    credentials: Credentials {
                        access_key_id: "key".into(),
                        secret_access_key: "secret".into(),
                        session_token: Some("token".into()),
                    },
                    expires_at: Some(SystemTime::now() + Duration::from_secs(60 * 60)),
                })
            })
        }
    }

    /// The store refused the keys, and the source returns them again:
    /// an error, and requests that follow do not ask the source again.
    #[tokio::test]
    async fn refused_keys_from_the_source_are_not_fetched_per_request() {
        let fetches = Arc::new(AtomicUsize::new(0));
        let chain = Chain::new(Box::new(Same(fetches.clone())));
        let first = chain.current(None).await.unwrap();
        for _ in 0..5 {
            let e = chain.current(Some(&first)).await.err().unwrap();
            assert!(e.message.contains("refused as expired"), "{e}");
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
    }

    /// The source fails, slowly, and there are no keys to fall back on:
    /// ten requests at once get the error after one fetch, and the next
    /// request gets it without a fetch.
    #[tokio::test]
    async fn a_failure_without_keys_is_not_tried_again_at_once() {
        let mut source = counting(None);
        source.fail_from = 0;
        source.delay = Duration::from_millis(50);
        let (chain, fetches) = chain_with(source, Duration::from_millis(200));
        let calls: Vec<_> = (0..10)
            .map(|_| {
                let chain = chain.clone();
                tokio::spawn(async move { chain.current(None).await })
            })
            .collect();
        for call in calls {
            let e = call.await.unwrap().err().unwrap();
            assert!(e.message.contains("counting: down"), "{e}");
            assert_eq!(e.fault, Fault::Unavailable);
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert!(chain.current(None).await.is_err());
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(chain.current(None).await.is_err());
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
    }

    /// Keys that arrive already expired (a host clock far ahead) are an
    /// error, are not signed with, and are not fetched per request.
    #[tokio::test]
    async fn expired_keys_from_the_source_are_an_error() {
        let (chain, fetches) = chain_with(counting(Some(Duration::ZERO)), MIN_REUSE);
        for _ in 0..5 {
            let e = chain.current(None).await.err().unwrap();
            assert!(e.message.contains("already expired"), "{e}");
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn refresh_follows_the_margin_and_the_minimum() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        // Ahead of the real clock, so subtracting never underflows.
        let at = Instant::now() + Duration::from_secs(1_000_000);
        let min = Duration::from_secs(60);
        let cached = |expires_in: Option<u64>, fetched_ago: u64| Cached {
            credentials: Arc::new(Credentials {
                access_key_id: String::new(),
                secret_access_key: String::new(),
                session_token: None,
            }),
            expires_at: expires_in.map(|s| now + Duration::from_secs(s)),
            fetched_at: at - Duration::from_secs(fetched_ago),
        };
        let margin = REFRESH_MARGIN.as_secs();
        assert!(!refresh_due(&cached(None, 999_999), now, at, min));
        assert!(!refresh_due(&cached(Some(margin + 1), 999), now, at, min));
        assert!(refresh_due(&cached(Some(margin), 999), now, at, min));
        // Inside the margin, but fetched 30 s ago.
        assert!(!refresh_due(&cached(Some(margin), 30), now, at, min));
        assert!(refresh_due(&cached(Some(margin), 60), now, at, min));
        assert!(cached(Some(0), 0).expired(now));
        assert!(!cached(Some(1), 0).expired(now));
    }

    #[tokio::test]
    async fn answers_are_classed_by_status() {
        let resp = |status: u16, body: String| {
            reqwest::Response::from(
                axum::http::Response::builder()
                    .status(status)
                    .body(body)
                    .unwrap(),
            )
        };
        assert_eq!(read_body(resp(200, "{}".into())).await.unwrap(), b"{}");
        for (status, fault) in [
            (503, Fault::Unavailable),
            (500, Fault::Unavailable),
            (429, Fault::Unavailable),
            (403, Fault::Settings),
            (404, Fault::Settings),
        ] {
            let e = read_body(resp(status, "no".into())).await.unwrap_err();
            assert_eq!(e.fault, fault, "{status}");
            assert!(e.message.starts_with(&status.to_string()), "{e}");
        }
        let big = "x".repeat(BODY_LIMIT + 1);
        let e = read_body(resp(200, big)).await.unwrap_err();
        assert_eq!(e.fault, Fault::Unavailable);

        // An AWS XML error shows its code and message, not the XML.
        let xml = "<ErrorResponse><Error><Type>Sender</Type><Code>AccessDenied</Code>\
                   <Message>Not authorized</Message></Error></ErrorResponse>";
        let e = read_body(resp(403, xml.into())).await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("AccessDenied"));
        assert_eq!(e.message, "403 Forbidden AccessDenied: Not authorized");
    }

    /// An answer without a length that grows past the limit is refused
    /// while it streams.
    #[tokio::test]
    async fn a_chunked_answer_over_the_limit_is_refused() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
            let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
            let _ = stream.write_all(head.as_bytes()).await;
            let chunk = format!("1000\r\n{}\r\n", "x".repeat(0x1000));
            for _ in 0..5 {
                if stream.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n").await;
        });
        let url = format!("http://127.0.0.1:{port}/");
        let e = send(client_for(&url).get(&url), Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(e.fault, Fault::Unavailable);
        assert!(e.message.contains("over"), "{e}");
    }

    /// A source that answers 503 once gets one more try, and the fetch
    /// succeeds. A 503 every time fails after two requests.
    #[tokio::test]
    async fn a_503_is_tried_once_more() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn serve(statuses: &'static [u16]) -> (String, Arc<AtomicUsize>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            let hits = Arc::new(AtomicUsize::new(0));
            let counter = hits.clone();
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    let status = statuses[n.min(statuses.len() - 1)];
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf).await;
                    let head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                }
            });
            (url, hits)
        }
        let (url, hits) = serve(&[503, 200]).await;
        let body = send(client_for(&url).get(&url), Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(body, b"ok");
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        let (url, hits) = serve(&[503]).await;
        let e = send(client_for(&url).get(&url), Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(e.fault, Fault::Unavailable);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    /// A source that does not answer in time is not tried again.
    #[tokio::test]
    async fn a_timeout_is_not_tried_again() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                // Hold the connection open and never answer.
                held.push(stream);
            }
        });
        let e = send(client_for(&url).get(&url), Duration::from_millis(200))
            .await
            .unwrap_err();
        assert_eq!(e.fault, Fault::TimedOut);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// reqwest names the URL in its errors. The secret parts of a
    /// container endpoint URL must not reach the log.
    #[tokio::test]
    async fn transport_errors_do_not_name_the_url() {
        // Nothing listens on port 9 here; the connection is refused.
        let url = "http://user:pass-secret@127.0.0.1:9/creds?sig=query-secret";
        let e = send(client_for(url).get(url), Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(!e.message.contains("secret"), "{e}");
        assert_eq!(e.fault, Fault::Unavailable);
    }

    #[test]
    fn only_local_hosts_skip_the_proxy() {
        for local in [
            "http://169.254.169.254/latest",
            "http://169.254.170.2/v2/credentials/x",
            "http://169.254.170.23/v1/credentials",
            "http://[fd00:ec2::23]/v1/credentials",
            "http://[fd00:ec2::254]/latest",
            "http://[fe80::1]/x",
            "http://127.0.0.1:8080/creds",
            "http://localhost/creds",
        ] {
            assert!(host_is_local(local), "{local}");
        }
        for remote in [
            "https://creds.example.com/x",
            "https://sts.us-east-1.amazonaws.com/",
            "http://10.0.0.1/x",
            "http://[fd00:ec2::1]/x",
            "nope",
        ] {
            assert!(!host_is_local(remote), "{remote}");
        }
    }

    #[test]
    fn expirations_parse_as_utc() {
        let t = parse_expiration("2026-10-28T13:00:00Z").unwrap();
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(super::super::amz_datetime(secs), "20261028T130000Z");
        assert_eq!(parse_expiration("2026-10-28T13:00:00.123Z").unwrap(), t);
        assert_eq!(
            parse_expiration("2024-02-29T00:00:00Z").unwrap(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_709_164_800)
        );
        for bad in ["2026-10-28T13:00:00", "2026-13-01T00:00:00Z", "soon", ""] {
            assert!(parse_expiration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn json_keys_need_an_id_a_secret_and_an_expiry() {
        let keys = keys_from_json(
            br#"{"AccessKeyId":"AKID","SecretAccessKey":"S","Token":"T","Expiration":"2026-10-28T13:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(keys.credentials.access_key_id, "AKID");
        assert_eq!(keys.credentials.session_token.as_deref(), Some("T"));
        assert!(keys.expires_at.is_some());
        for body in [
            &br#"{"SecretAccessKey":"S","Expiration":"2026-10-28T13:00:00Z"}"#[..],
            br#"{"AccessKeyId":"AKID","SecretAccessKey":"S"}"#,
            b"not json",
            br#"{"Code":"AssumeRoleUnauthorizedAccess","AccessKeyId":"AKID","SecretAccessKey":"S","Expiration":"2026-10-28T13:00:00Z"}"#,
        ] {
            assert!(keys_from_json(body).is_err());
        }
    }
}
