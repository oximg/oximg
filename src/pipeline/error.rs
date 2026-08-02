//! The typed error surface of the pipeline. Every public entry point
//! returns [`Error`]; its [`ErrorKind`] is the semver-stable
//! classification that used to live as anyhow-downcast conventions in
//! the HTTP layer. The server maps kinds to statuses with an
//! exhaustive match, and library embedders get the same contract.
//!
//! Internals keep using `anyhow` for its context chains; this module
//! classifies once at the public boundary. The chain survives inside
//! [`Error`]: `{e}` prints the top-level message (safe to echo to a
//! client for [`ErrorKind::Undecodable`]), `{e:#}` the full chain
//! (what an operator's log wants).

use std::io::ErrorKind as IoKind;

/// What went wrong, at the resolution a caller can act on.
///
/// Marked `#[non_exhaustive]`: new kinds may appear in minor versions,
/// so external matches need a wildcard arm (treat unknown kinds like
/// [`ErrorKind::Internal`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The source does not exist: local file not found, or the remote
    /// origin answered 404. (HTTP: 404.)
    SourceNotFound,
    /// The source exceeds a configured limit — compressed bytes
    /// (`OXIMG_MAX_SOURCE_BYTES`) or decoded pixels
    /// (`OXIMG_MAX_SRC_PIXELS`). (HTTP: 413.)
    SourceTooLarge,
    /// The source exists but could not be read — permissions, a
    /// directory where a file was expected, local I/O faults. A
    /// deployment problem, not bad input. (HTTP: 500.)
    SourceUnreadable,
    /// A remote origin failed: transport error, non-404 error status,
    /// refused redirect, or a body that died mid-stream. Only produced
    /// by `process_url`. (HTTP: 502.)
    Upstream,
    /// The bytes are not a decodable image, or the request asks for a
    /// capability this build lacks (e.g. AVIF output without the
    /// `avif` feature). The client's fault; the top-level message is
    /// safe to return. (HTTP: 422.)
    Undecodable,
    /// An internal processing fault — encoder, worker infrastructure.
    /// Our fault. (HTTP: 500.)
    Internal,
}

/// A classified pipeline failure. See [`ErrorKind`] for the taxonomy
/// and the module docs for the Display forms.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    inner: anyhow::Error,
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Classify an internal error at the public boundary. `remote` is
    /// true on the `process_url` path, where connection-shaped I/O
    /// failures indict the origin; the same failures on a local source
    /// mean a truncated file — bad input.
    pub(crate) fn classify(inner: anyhow::Error, remote: bool) -> Error {
        let kind = if let Some(io) = inner.downcast_ref::<std::io::Error>() {
            match io.kind() {
                IoKind::NotFound => ErrorKind::SourceNotFound,
                IoKind::FileTooLarge => ErrorKind::SourceTooLarge,
                IoKind::PermissionDenied | IoKind::IsADirectory => ErrorKind::SourceUnreadable,
                IoKind::ConnectionReset
                | IoKind::ConnectionAborted
                | IoKind::BrokenPipe
                | IoKind::UnexpectedEof
                | IoKind::TimedOut
                    if remote =>
                {
                    ErrorKind::Upstream
                }
                _ => ErrorKind::Undecodable,
            }
        } else if inner.downcast_ref::<super::UpstreamFault>().is_some() {
            ErrorKind::Upstream
        } else if inner.downcast_ref::<super::ServerFault>().is_some() {
            ErrorKind::Internal
        } else {
            ErrorKind::Undecodable
        };
        Error { kind, inner }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if f.alternate() {
            write!(f, "{:#}", self.inner)
        } else {
            write!(f, "{}", self.inner)
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Skip the chain's head: it is what Display already prints, and
        // repeating it as its own source would double the first line in
        // any reporter that walks the chain (anyhow's `{:#}` included).
        self.inner.chain().nth(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io(kind: IoKind) -> anyhow::Error {
        anyhow::Error::new(std::io::Error::new(kind, "io-level detail"))
    }

    /// The full classification table, including the remote/local split
    /// for connection-shaped failures.
    #[test]
    fn classification_table() {
        for (err, remote, want) in [
            (io(IoKind::NotFound), false, ErrorKind::SourceNotFound),
            (io(IoKind::NotFound), true, ErrorKind::SourceNotFound),
            (io(IoKind::FileTooLarge), false, ErrorKind::SourceTooLarge),
            (io(IoKind::FileTooLarge), true, ErrorKind::SourceTooLarge),
            (
                io(IoKind::PermissionDenied),
                false,
                ErrorKind::SourceUnreadable,
            ),
            (io(IoKind::IsADirectory), false, ErrorKind::SourceUnreadable),
            // connection-shaped: the origin's fault only when remote
            (io(IoKind::UnexpectedEof), true, ErrorKind::Upstream),
            (io(IoKind::UnexpectedEof), false, ErrorKind::Undecodable),
            (io(IoKind::ConnectionReset), true, ErrorKind::Upstream),
            (io(IoKind::TimedOut), true, ErrorKind::Upstream),
            (io(IoKind::TimedOut), false, ErrorKind::Undecodable),
            // markers attached as context
            (
                anyhow::anyhow!("origin 500").context(super::super::UpstreamFault),
                false,
                ErrorKind::Upstream,
            ),
            (
                anyhow::anyhow!("worker died").context(super::super::ServerFault),
                false,
                ErrorKind::Internal,
            ),
            // plain message: undecodable client input
            (
                anyhow::anyhow!("bogus bytes"),
                false,
                ErrorKind::Undecodable,
            ),
            (anyhow::anyhow!("bogus bytes"), true, ErrorKind::Undecodable),
        ] {
            assert_eq!(Error::classify(err, remote).kind(), want, "remote={remote}");
        }
    }

    /// An io::Error buried under context layers still classifies —
    /// that is how the pipeline actually produces them.
    #[test]
    fn classification_sees_through_context() {
        let err = io(IoKind::NotFound).context("open source");
        assert_eq!(
            Error::classify(err, false).kind(),
            ErrorKind::SourceNotFound
        );
    }

    /// `{e}` is the top-level message (client-safe), `{e:#}` the chain
    /// (operator logs), and source() starts at the first cause so
    /// reporters do not print the head twice.
    #[test]
    fn display_forms_and_source() {
        let e = Error::classify(io(IoKind::NotFound).context("open source"), false);
        assert_eq!(format!("{e}"), "open source");
        assert_eq!(format!("{e:#}"), "open source: io-level detail");
        let src = std::error::Error::source(&e).expect("has a source");
        assert_eq!(src.to_string(), "io-level detail");
    }
}
