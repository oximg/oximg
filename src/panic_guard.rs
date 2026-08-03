//! Unwind guards for decoders that panic on malformed input.
//!
//! Two upstream paths turn attacker-supplied bytes into panics rather
//! than errors: avif-parse's internal parser-state assertions, and
//! mozjpeg's error handler, which reports fatal libjpeg errors by
//! unwinding through its `extern "C-unwind"` callback (the crate's
//! documented default — `Decompress`'s `Drop` calls
//! `jpeg_destroy_decompress`, so the C state is released on the way
//! out). Malformed input must surface as a classified error, never a
//! crash: under `panic = "abort"` an uncaught one takes the process
//! down, and even unwinding it would fail the request as a panic
//! (HTTP 500) instead of the 422 it is.
//!
//! The panic hook is filtered so these deliberate catches stay silent:
//! without it every malformed source would print a crash-shaped trace
//! and, under `RUST_BACKTRACE`, serialize on the global backtrace lock.

use anyhow::Result;

thread_local! {
    /// Set while a deliberately caught call runs.
    static SUPPRESS_PANIC_LOG: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Install (once) a panic hook that skips logging for panics caught
/// here on purpose and delegates to the previous hook for everything
/// else.
fn install_quiet_panic_hook() {
    static HOOK: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    HOOK.get_or_init(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !SUPPRESS_PANIC_LOG.with(|s| s.get()) {
                prev(info);
            }
        }));
    });
}

/// Run `f`, converting a panic into an error whose message keeps the
/// panic text — it identifies which upstream check fired, which is the
/// only breadcrumb an operator gets for a malformed-source report.
///
/// `what` names the operation for the error message. The closure must
/// leave no torn state behind on unwind: both current callers hand a
/// shared slice to a parser that owns everything else it touches.
pub(crate) fn catch_unwind_as_error<T>(what: &str, f: impl FnOnce() -> T) -> Result<T> {
    install_quiet_panic_hook();
    struct Unsuppress;
    impl Drop for Unsuppress {
        fn drop(&mut self) {
            SUPPRESS_PANIC_LOG.with(|s| s.set(false));
        }
    }
    SUPPRESS_PANIC_LOG.with(|s| s.set(true));
    let _guard = Unsuppress;
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => Ok(v),
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic payload");
            Err(anyhow::anyhow!("{what} panicked: {msg}"))
        }
    }
}
