//! The C ABI the gem calls through `ffi` (rubygem/oximg/lib/oximg/native.rb).
//!
//! Every entry point returns a status and writes text into the caller's
//! `msg` buffer: the content type on success (`image/jpeg`, exactly as
//! `oximg probe` prints it), the error message on failure. Dimensions
//! go to `dims[0..2]` as width and height. The text is NUL-terminated
//! and cut at a UTF-8 boundary to fit `msg_cap`.
//!
//! Nothing may unwind into the caller: Ruby's frames are C frames, and
//! a panic crossing them is undefined behavior. `probe_reader` already
//! turns the libjpeg handler's deliberate panics into errors; anything
//! else is caught at the boundary and reported as [`PANICKED`].

use std::ffi::c_int;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};

use oximg::pipeline::{self, ImageFormat};

/// The header parsed.
pub const OK: c_int = 0;
/// The source was read and refused, or could not be read: `msg` says why.
pub const FAILED: c_int = 1;
/// A panic reached the boundary: `msg` holds its text.
pub const PANICKED: c_int = 2;
/// The caller's read callback reported a failure. Nothing is written to
/// `msg`: the caller holds the real error (in Ruby, the exception its
/// `IO#read` raised).
pub const READ_FAILED: c_int = 3;

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

/// `read(ctx, buf, cap)` fills up to `cap` bytes of `buf` and returns
/// how many it wrote, 0 at end of input, or a negative number on
/// failure.
pub type ReadFn =
    unsafe extern "C" fn(ctx: *mut std::ffi::c_void, buf: *mut u8, cap: usize) -> isize;

/// Probe a stream the caller reads for us through `read`, pulling only
/// as much of it as the header takes. `read` is called on this thread,
/// and never after this function returns.
///
/// # Safety
/// `read` honors the [`ReadFn`] contract for `ctx`; `dims` is writable
/// for two `u64`s and `msg` for `msg_cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn oximg_probe_read(
    read: ReadFn,
    ctx: *mut std::ffi::c_void,
    dims: *mut u64,
    msg: *mut u8,
    msg_cap: usize,
) -> c_int {
    let mut source = Callback {
        read,
        ctx,
        failed: false,
    };
    let result = catch_unwind(AssertUnwindSafe(|| {
        pipeline::probe_reader(&mut source).map_err(|e| format!("{e:#}"))
    }));
    if source.failed {
        return READ_FAILED;
    }
    unsafe { finish(result, dims, msg, msg_cap) }
}

struct Callback {
    read: ReadFn,
    ctx: *mut std::ffi::c_void,
    failed: bool,
}

impl Read for Callback {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = unsafe { (self.read)(self.ctx, buf.as_mut_ptr(), buf.len()) };
        // A count past `cap` would claim bytes nobody wrote.
        match usize::try_from(n) {
            Ok(n) if n <= buf.len() => Ok(n),
            _ => {
                self.failed = true;
                Err(std::io::Error::other("read callback failed"))
            }
        }
    }
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
