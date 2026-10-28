//! Where the signing keys come from, and keeping them fresh.
//!
//! `Chain::from_env` picks the first source whose settings are present,
//! in the order of `SOURCES`, as the AWS SDKs walk their default chain.
//! The keys are cached for the process and fetched again in the
//! background shortly before they expire.

use std::future::Future;
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

pub(super) type KeysFuture<'a> = Pin<Box<dyn Future<Output = Result<Keys, String>> + Send + 'a>>;

/// One place keys can come from.
pub(super) trait Source: Send + Sync {
    /// Shown in errors, so an operator can tell which source failed.
    fn name(&self) -> &'static str;
    fn fetch(&self) -> KeysFuture<'_>;
}

/// A source's constructor: `Ok(None)` when its settings are absent,
/// `Err` when they are present but wrong.
type FromEnv = fn() -> Result<Option<Box<dyn Source>>, String>;

/// The chain, in order. `hint` lists what the source reads, for the
/// error when none of them is set.
const SOURCES: &[(FromEnv, &str)] = &[(
    Static::from_env,
    "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY",
)];

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
    error: String,
}

#[derive(Default)]
struct State {
    cached: Option<Cached>,
    failure: Option<Failure>,
    /// A fetch runs. There is never more than one.
    fetching: bool,
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
    Ready(Result<Arc<Credentials>, String>),
    /// Wait for the running fetch.
    Wait,
    /// No runtime to spawn on: run the fetch here.
    Run(Pin<Box<dyn Future<Output = ()> + Send>>),
}

impl Chain {
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

    /// The keys to sign with.
    ///
    /// Keys that still work are returned at once. When they are inside
    /// the refresh margin, a fetch starts in the background, so no
    /// request waits for it. Only a request without working keys waits,
    /// and all such requests wait for the same fetch.
    ///
    /// A fetch that gives no usable keys (an error, or keys that already
    /// expired) is not tried again for
    /// `min_reuse`. Until then, requests sign with the cached keys while
    /// they have not expired, or get the same error. So a source that is
    /// down or slow costs one fetch per `min_reuse`, not one per request.
    pub(super) async fn current(&self) -> Result<Arc<Credentials>, String> {
        let mut waited = false;
        loop {
            // The lock is released at the end of this block, before any
            // await.
            let step = {
                let mut state = self.inner.lock();
                match self.inner.next(&mut state, waited) {
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
    fn next(self: &Arc<Self>, state: &mut State, waited: bool) -> Step {
        let now = Instant::now();
        let cooling = state
            .failure
            .as_ref()
            .is_some_and(|f| now < f.at + self.min_reuse);
        if let Some(c) = &state.cached
            && !c.expired(SystemTime::now())
        {
            let keys = c.credentials.clone();
            if !cooling
                && !state.fetching
                && refresh_due(c, SystemTime::now(), now, self.min_reuse)
                && let Some(fetch) = self.start(state)
            {
                return Step::Run(fetch);
            }
            return Step::Ready(Ok(keys));
        }
        if (cooling || waited)
            && let Some(f) = &state.failure
        {
            return Step::Ready(Err(f.error.clone()));
        }
        if state.fetching {
            return Step::Wait;
        }
        match self.start(state) {
            Some(fetch) => Step::Run(fetch),
            None => Step::Wait,
        }
    }

    /// Start a fetch on the runtime. Without one, return it for the
    /// caller to run.
    fn start(
        self: &Arc<Self>,
        state: &mut State,
    ) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        state.fetching = true;
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
                    inner.finish(Err("the fetch stopped before it finished".into()));
                }
            }
        }
        let mut unfinished = Unfinished(Some(self.clone()));
        let result = self.source.fetch().await;
        unfinished.0 = None;
        self.finish(result);
    }

    fn finish(&self, result: Result<Keys, String>) {
        let mut state = self.lock();
        state.fetching = false;
        let name = self.source.name();
        let error = match result {
            Ok(keys) if keys.expires_at.is_some_and(|t| t <= SystemTime::now()) => {
                format!(
                    "s3:// credentials from {name}: the source returned keys that have \
                     already expired. Check this host's clock"
                )
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
            Err(e) => format!("s3:// credentials from {name}: {e}"),
        };
        if let Some(c) = &state.cached
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

/// Keys in the JSON shape that the container endpoint and IMDS share:
/// `AccessKeyId`, `SecretAccessKey`, `Token` and `Expiration`. IMDS
/// also sends `Code`, which is `Success` when the keys are good.
#[allow(dead_code)] // the container endpoint is the first user
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
#[allow(dead_code)] // the container endpoint is the first user
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
                    return Err("down".to_string());
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
        chain.current().await.unwrap();
        // The request gets the keys it has; the fetch runs behind it.
        assert_eq!(chain.current().await.unwrap().access_key_id, "key-0");
        settle().await;
        assert_eq!(chain.current().await.unwrap().access_key_id, "key-1");
        assert!(fetches.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn keys_inside_the_margin_are_reused_for_a_minute() {
        let (chain, fetches) = chain_with(counting(Some(Duration::from_secs(4 * 60))), MIN_REUSE);
        chain.current().await.unwrap();
        assert_eq!(chain.current().await.unwrap().access_key_id, "key-0");
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
                tokio::spawn(async move { chain.current().await.unwrap() })
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
        chain.current().await.unwrap();
        let start = Instant::now();
        let calls: Vec<_> = (0..10)
            .map(|_| {
                let chain = chain.clone();
                tokio::spawn(async move { chain.current().await.unwrap() })
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
        let gave_up = tokio::time::timeout(Duration::from_millis(20), chain.current()).await;
        assert!(gave_up.is_err());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(chain.current().await.unwrap().access_key_id, "key-0");
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_keys_that_still_work() {
        let mut source = counting(Some(Duration::from_secs(4 * 60)));
        source.fail_from = 1;
        let (chain, fetches) = chain(source);
        chain.current().await.unwrap();
        assert_eq!(chain.current().await.unwrap().access_key_id, "key-0");
        settle().await;
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
        assert_eq!(chain.current().await.unwrap().access_key_id, "key-0");
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
        chain.current().await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        let calls: Vec<_> = (0..10)
            .map(|_| {
                let chain = chain.clone();
                tokio::spawn(async move { chain.current().await.unwrap() })
            })
            .collect();
        for call in calls {
            assert_eq!(call.await.unwrap().access_key_id, "key-0");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
        chain.current().await.unwrap();
        settle().await;
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
                tokio::spawn(async move { chain.current().await })
            })
            .collect();
        for call in calls {
            let e = call.await.unwrap().err().unwrap();
            assert!(e.contains("counting: down"), "{e}");
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert!(chain.current().await.is_err());
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(chain.current().await.is_err());
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
    }

    /// Keys that arrive already expired (a host clock far ahead) are an
    /// error, are not signed with, and are not fetched per request.
    #[tokio::test]
    async fn expired_keys_from_the_source_are_an_error() {
        let (chain, fetches) = chain_with(counting(Some(Duration::ZERO)), MIN_REUSE);
        for _ in 0..5 {
            let e = chain.current().await.err().unwrap();
            assert!(e.contains("already expired"), "{e}");
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
