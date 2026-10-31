//! The C ABI the gem calls through `ffi` (rubygem/oximg/lib/oximg/native.rb).
//!
//! Every entry point returns a status and writes text into the caller's
//! `msg` buffer: the content type on success (`image/jpeg`, exactly as
//! `oximg probe` prints it), the error message on failure. Dimensions
//! go to `dims[0..2]` as width and height. The text is NUL-terminated
//! and cut at a UTF-8 boundary to fit `msg_cap`.
//!
//! Nothing may unwind into the caller: Ruby's frames are C frames, and
//! a panic crossing them is undefined behavior. The pipeline already
//! turns the libjpeg handler's deliberate panics into errors; anything
//! else is caught at the boundary and reported as [`PANICKED`].

use std::ffi::c_int;

use std::panic::catch_unwind;

use oximg::pipeline::{self, ImageFormat, ProbeStep};

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
#[unsafe(no_mangle)]
pub extern "C" fn oximg_probe_abi_version() -> u32 {
    1
}

/// Probe the file at `path[..path_len]` (raw bytes, as the OS spells the
/// name; no NUL needed), reading only as much of it as the header
/// takes.
///
/// # Safety
/// `path` is readable for `path_len` bytes, `dims` writable for two
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
        pipeline::probe_reader(file).map_err(|e| format!("{}: {e:#}", path.display()))
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
/// writable for two `u64`s and `msg` for `msg_cap` bytes.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn oximg_probe_fd(
    fd: c_int,
    dims: *mut u64,
    msg: *mut u8,
    msg_cap: usize,
) -> c_int {
    use std::os::fd::BorrowedFd;
    use std::os::unix::fs::FileExt;
    struct At {
        file: std::fs::File,
        offset: u64,
    }
    impl std::io::Read for At {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.file.read_at(buf, self.offset)?;
            self.offset += n as u64;
            Ok(n)
        }
    }
    let result = catch_unwind(|| {
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        let file = std::fs::File::from(fd.try_clone_to_owned().map_err(|e| format!("read: {e}"))?);
        pipeline::probe_reader(At { file, offset: 0 }).map_err(|e| format!("{e:#}"))
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
/// 0), `dims` writable for two `u64`s, and `msg` for `msg_cap` bytes.
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
    let step = catch_unwind(|| pipeline::probe_step(prefix, eof != 0));
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

type Probed = Result<(ImageFormat, usize, usize), String>;

unsafe fn finish(
    result: std::thread::Result<Probed>,
    dims: *mut u64,
    msg: *mut u8,
    msg_cap: usize,
) -> c_int {
    let (status, text) = match result {
        Ok(Ok((format, w, h))) => {
            unsafe {
                *dims = w as u64;
                *dims.add(1) = h as u64;
            }
            (OK, format.content_type().to_string())
        }
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
