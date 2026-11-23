//! The C ABI the gem calls through `ffi` (rubygem/oximg/lib/oximg/native.rb).
//!
//! Every entry point returns a status and writes text into the caller's
//! `msg` buffer: the content type on success (`image/jpeg`, exactly as
//! `oximg probe` prints it), the error message on failure. The header
//! goes to `dims[0..3]`: stored width, stored height, and the EXIF
//! orientation (1..=8; 1 when there is none). The text is
//! NUL-terminated and cut at a UTF-8 boundary to fit `msg_cap`.
//!
//! The `oximg_probe_animation_*` entry points (#187) read the whole
//! source rather than its header, and write nine slots: the three
//! above, then the animation summary (see [`ANIMATION_SLOTS`]).
//!
//! Nothing may unwind into the caller: Ruby's frames are C frames, and
//! a panic crossing them is undefined behavior. The pipeline already
//! turns the libjpeg handler's deliberate panics into errors; anything
//! else is caught at the boundary and reported as [`PANICKED`].

use std::ffi::c_int;

use std::panic::catch_unwind;

use oximg::pipeline::{self, Header, ProbeStep, SourceProbe};

/// The header parsed.
pub const OK: c_int = 0;
/// The source was read and refused, or could not be read: `msg` says why.
pub const FAILED: c_int = 1;
/// A panic reached the boundary: `msg` holds its text.
pub const PANICKED: c_int = 2;
/// [`oximg_probe_step`] needs more of the source: `dims[0]` says how
/// much. Nothing is written to `msg`.
pub const READ: c_int = 3;

/// Bumped on any change to a signature or a status's meaning. The gem
/// refuses a library whose version it does not know, rather than
/// calling it with the wrong arguments.
///
/// 2: `dims` grew a third slot, the orientation (#163).
/// 3: the `oximg_probe_animation_*` entry points (#187). No older
/// signature changed; the bump is so a gem that needs them names the
/// version it wants instead of failing on a missing symbol.
/// 4: the whole-source reads grew a ninth slot, `complete` (#191). A
/// gem of ABI 3 would hand over eight slots for nine to be written.
#[unsafe(no_mangle)]
pub extern "C" fn oximg_probe_abi_version() -> u32 {
    4
}

/// What an `oximg_probe_animation_*` call writes to `out`: the header's
/// three slots, then `frames` (0: the source is not animated, and the
/// rest are 0 too), `duration_ms`, `loop_count` (0: forever),
/// `decode_work`, and whether `decode_work` is priced at all (1 or 0:
/// it is not for an animated WebP). As `pipeline::Animation`. The ninth
/// is `complete` whatever the animation, as `pipeline::SourceProbe`:
/// [`NOT_CHECKED`], [`INCOMPLETE`] or [`COMPLETE`].
pub const ANIMATION_SLOTS: usize = 9;

/// The `complete` slot for a format the walk does not check (JPEG, PNG,
/// AVIF). Zero, so it is never read as "whole" by mistake.
pub const NOT_CHECKED: u64 = 0;
/// The `complete` slot for a GIF or WebP cut short.
pub const INCOMPLETE: u64 = 1;
/// The `complete` slot for a GIF or WebP that arrived whole.
pub const COMPLETE: u64 = 2;

/// Probe the file at `path[..path_len]` (raw bytes, as the OS spells the
/// name; no NUL needed), reading only as much of it as the header
/// takes.
///
/// # Safety
/// `path` is readable for `path_len` bytes, `dims` writable for three
/// `u64`s, and `msg` writable for `msg_cap` bytes.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn oximg_probe_path(
    path: *const u8,
    path_len: usize,
    dims: *mut u64,
    msg: *mut u8,
    msg_cap: usize,
) -> c_int {
    use std::os::unix::ffi::OsStrExt;
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(unsafe {
        std::slice::from_raw_parts(path, path_len)
    }));
    let result = catch_unwind(|| {
        // The same leading context the CLI gives a failed read.
        let file =
            std::fs::File::open(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        // Seek only where seeking works: a FIFO or a pipe-backed
        // /dev/stdin is a path too, and lseek fails on it (ESPIPE), so
        // anything but a regular file keeps the forward-only read.
        let regular = file.metadata().is_ok_and(|m| m.is_file());
        let header = if regular {
            pipeline::probe_header_seek(file)
        } else {
            pipeline::probe_header_reader(file)
        };
        header.map_err(|e| format!("{}: {e:#}", path.display()))
    });
    unsafe { finish(result, dims, msg, msg_cap) }
}

/// Probe the open file `fd` from its start, reading only as much of it
/// as the header takes. The descriptor, not a name, so the answer is
/// the file the caller holds even after its name moved on (replaced,
/// renamed, or relative to a directory the process left); positioned
/// reads (`pread`) on a duplicate, so the caller's offset is untouched
/// and `fd` stays the caller's to close.
///
/// # Safety
/// `fd` is an open descriptor for the duration of the call; `dims`
/// writable for three `u64`s and `msg` for `msg_cap` bytes.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn oximg_probe_fd(
    fd: c_int,
    dims: *mut u64,
    msg: *mut u8,
    msg_cap: usize,
) -> c_int {
    let result = catch_unwind(|| {
        pipeline::probe_header_seek(unsafe { At::start_of(fd) }?).map_err(|e| format!("{e:#}"))
    });
    unsafe { finish(result, dims, msg, msg_cap) }
}

/// One step of a header read the caller drives (`pipeline::probe_step`):
/// `prefix[..len]` is everything read from the source so far, `eof`
/// nonzero once it has ended. Returns [`READ`] with `dims[0]` set to the
/// byte count to read on to (`u64::MAX`: to the end), then call again
/// with all of it; or the answer, as [`oximg_probe_path`] gives it.
///
/// The caller does the reading, so none of its code runs under these
/// frames: an exception, a thread kill or a timeout in the caller's
/// I/O never has a Rust frame to cross.
///
/// # Safety
/// `prefix` is readable for `len` bytes (it may be null when `len` is
/// 0), `dims` writable for three `u64`s, and `msg` for `msg_cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn oximg_probe_step(
    prefix: *const u8,
    len: usize,
    eof: u8,
    dims: *mut u64,
    msg: *mut u8,
    msg_cap: usize,
) -> c_int {
    let prefix = if len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(prefix, len) }
    };
    let step = catch_unwind(|| pipeline::probe_header_step(prefix, eof != 0));
    let result = match step {
        Ok(ProbeStep::Read(want)) => {
            unsafe { *dims = u64::try_from(want).unwrap_or(u64::MAX) };
            return READ;
        }
        Ok(ProbeStep::Done(result)) => Ok(result.map_err(|e| format!("{e:#}"))),
        Err(payload) => Err(payload),
    };
    unsafe { finish(result, dims, msg, msg_cap) }
}

/// An open descriptor read from its start with positioned reads, on a
/// duplicate: the caller's offset is untouched.
#[cfg(unix)]
struct At {
    file: std::fs::File,
    offset: u64,
}
#[cfg(unix)]
impl At {
    /// # Safety
    /// `fd` is an open descriptor for the duration of the call.
    unsafe fn start_of(fd: c_int) -> Result<Self, String> {
        let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        let file = std::fs::File::from(fd.try_clone_to_owned().map_err(|e| format!("read: {e}"))?);
        Ok(At { file, offset: 0 })
    }
}
#[cfg(unix)]
impl std::io::Read for At {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = std::os::unix::fs::FileExt::read_at(&self.file, buf, self.offset)?;
        self.offset += n as u64;
        Ok(n)
    }
}
// Seeking moves only this offset, never the descriptor's: the
// caller's position stays where it was.
#[cfg(unix)]
impl std::io::Seek for At {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        let (base, delta) = match to {
            std::io::SeekFrom::Start(n) => (n, 0),
            std::io::SeekFrom::Current(d) => (self.offset, d),
            std::io::SeekFrom::End(d) => (self.file.metadata()?.len(), d),
        };
        self.offset = base
            .checked_add_signed(delta)
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        Ok(self.offset)
    }
}

/// Probe the file at `path[..path_len]` with its animation summary:
/// the whole file is read (a GIF's frame count is a walk to its
/// trailer), though no pixel is decoded.
///
/// # Safety
/// `path` is readable for `path_len` bytes, `out` writable for
/// [`ANIMATION_SLOTS`] `u64`s, and `msg` writable for `msg_cap` bytes.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn oximg_probe_animation_path(
    path: *const u8,
    path_len: usize,
    out: *mut u64,
    msg: *mut u8,
    msg_cap: usize,
) -> c_int {
    use std::os::unix::ffi::OsStrExt;
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(unsafe {
        std::slice::from_raw_parts(path, path_len)
    }));
    let result = catch_unwind(|| {
        let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        probe_whole(&bytes).map_err(|e| format!("{}: {e}", path.display()))
    });
    unsafe { finish_animation(result, out, msg, msg_cap) }
}

/// Probe the open file `fd` with its animation summary, read whole
/// from its start as [`oximg_probe_fd`] reads the header: positioned
/// reads on a duplicate.
///
/// # Safety
/// `fd` is an open descriptor for the duration of the call; `out`
/// writable for [`ANIMATION_SLOTS`] `u64`s and `msg` for `msg_cap`
/// bytes.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn oximg_probe_animation_fd(
    fd: c_int,
    out: *mut u64,
    msg: *mut u8,
    msg_cap: usize,
) -> c_int {
    let result = catch_unwind(|| {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut unsafe { At::start_of(fd) }?, &mut bytes)
            .map_err(|e| format!("read: {e}"))?;
        probe_whole(&bytes)
    });
    unsafe { finish_animation(result, out, msg, msg_cap) }
}

/// Probe the whole source `bytes[..len]`, which the caller read, with
/// its animation summary.
///
/// # Safety
/// `bytes` is readable for `len` bytes (it may be null when `len` is
/// 0), `out` writable for [`ANIMATION_SLOTS`] `u64`s, and `msg` for
/// `msg_cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn oximg_probe_animation_bytes(
    bytes: *const u8,
    len: usize,
    out: *mut u64,
    msg: *mut u8,
    msg_cap: usize,
) -> c_int {
    let bytes = if len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(bytes, len) }
    };
    let result = catch_unwind(|| probe_whole(bytes));
    unsafe { finish_animation(result, out, msg, msg_cap) }
}

/// The header, the animation summary and completeness of a source held
/// whole, as `oximg probe --json` reads them.
fn probe_whole(bytes: &[u8]) -> Result<(Header, SourceProbe), String> {
    let header = pipeline::probe_header(bytes).map_err(|e| format!("{e:#}"))?;
    let source = pipeline::probe_source(bytes).map_err(|e| format!("{e:#}"))?;
    Ok((header, source))
}

type Probed = Result<Header, String>;

unsafe fn finish(
    result: std::thread::Result<Probed>,
    dims: *mut u64,
    msg: *mut u8,
    msg_cap: usize,
) -> c_int {
    unsafe { report(result, msg, msg_cap, |header| write_header(&header, dims)) }
}

unsafe fn finish_animation(
    result: std::thread::Result<Result<(Header, SourceProbe), String>>,
    out: *mut u64,
    msg: *mut u8,
    msg_cap: usize,
) -> c_int {
    unsafe {
        report(result, msg, msg_cap, |(header, source)| {
            let format = write_header(&header, out);
            let summary = match source.animation {
                Some(a) => [
                    a.frames as u64,
                    a.duration_ms,
                    u64::from(a.loop_count),
                    a.decode_work.unwrap_or(0),
                    u64::from(a.decode_work.is_some()),
                ],
                None => [0; 5],
            };
            let complete = match source.complete {
                None => NOT_CHECKED,
                Some(false) => INCOMPLETE,
                Some(true) => COMPLETE,
            };
            std::ptr::copy_nonoverlapping(summary.as_ptr(), out.add(3), summary.len());
            *out.add(ANIMATION_SLOTS - 1) = complete;
            format
        })
    }
}

/// Writes the header's three slots; returns the content type.
unsafe fn write_header(header: &Header, dims: *mut u64) -> String {
    unsafe {
        *dims = header.width as u64;
        *dims.add(1) = header.height as u64;
        *dims.add(2) = u64::from(header.orientation);
    }
    header.format.content_type().to_string()
}

/// The status of a call, with its text in `msg`: what `ok` returns on
/// success (after writing the numbers), else the error or the panic.
unsafe fn report<T>(
    result: std::thread::Result<Result<T, String>>,
    msg: *mut u8,
    msg_cap: usize,
    ok: impl FnOnce(T) -> String,
) -> c_int {
    let (status, text) = match result {
        Ok(Ok(value)) => (OK, ok(value)),
        Ok(Err(e)) => (FAILED, e),
        Err(payload) => {
            let text = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            (PANICKED, format!("probe panicked: {text}"))
        }
    };
    unsafe { write_text(&text, msg, msg_cap) };
    status
}

/// Copy `text` into `msg` NUL-terminated, cut at a UTF-8 boundary.
unsafe fn write_text(text: &str, msg: *mut u8, msg_cap: usize) {
    if msg_cap == 0 {
        return;
    }
    let mut n = text.len().min(msg_cap - 1);
    while !text.is_char_boundary(n) {
        n -= 1;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), msg, n);
        *msg.add(n) = 0;
    }
}
