//! The jpegli encoder, driven through `jpegli-sys` directly. The `jpegli`
//! crate keeps its `cinfo` private, and the one setting it cannot reach
//! is the one that matters here: the progressive scan script.
//!
//! jpegli's own progressive script (`jpegli_simple_progression`, level
//! 2) is five scans per component, two of them successive-approximation
//! refinements, and tokenizing those is most of the encoder's CPU at
//! thumbnail sizes. [`SCAN_SCRIPT`] drops successive approximation and
//! only splits the spectrum. A scan script is a lossless reordering of
//! the same quantized coefficients, so decoded pixels are unchanged.
//! Measured on DIV2K (Zen 4), server CPU per request against level 2:
//! -15% at fit 1024, -11% at 512, -3% at 256, for files +0.69%, +0.43%
//! and -0.03% larger.
//!
//! Errors unwind out of the C code as panics, like the mozjpeg and
//! jpegli crates' default error managers; `Drop` releases the encoder.

use jpegli_sys as ffi;
use std::mem;
use std::os::raw::c_int;
use std::ptr;

/// The APP2 marker code, which carries ICC profile chunks.
pub(super) const JPEG_APP2: c_int = ffi::JPEG_APP0 as c_int + 2;

/// Rows per `jpegli_write_scanlines` call, as the `jpegli` crate batched.
const ROW_BATCH: usize = 16;

/// One scan: components, spectral band, successive-approximation bits.
const fn scan(comps: &[c_int], ss: c_int, se: c_int) -> ffi::jpegli_scan_info {
    let mut component_index = [0; 4];
    let mut i = 0;
    while i < comps.len() {
        component_index[i] = comps[i];
        i += 1;
    }
    ffi::jpegli_scan_info {
        comps_in_scan: comps.len() as c_int,
        component_index,
        Ss: ss,
        Se: se,
        Ah: 0,
        Al: 0,
    }
}

/// Interleaved DC, then luma AC in three bands and each chroma's AC in
/// two. Chosen from 17 candidates on the DIV2K CPU/size front: dropping
/// successive approximation is what saves the CPU, and the low bands
/// split out (1-2, then 3-10 on luma) win back most of the bytes.
static SCAN_SCRIPT: [ffi::jpegli_scan_info; 8] = [
    scan(&[0, 1, 2], 0, 0),
    scan(&[0], 1, 2),
    scan(&[0], 3, 10),
    scan(&[0], 11, 63),
    scan(&[1], 1, 2),
    scan(&[1], 3, 63),
    scan(&[2], 1, 2),
    scan(&[2], 3, 63),
];

/// Output sink: jpegli fills the spare capacity of `buf` and calls
/// `empty_output_buffer` only once it is full (bit_writer.cc), so growth
/// is a `set_len` plus a doubling `reserve`. `repr(C)` with `iface`
/// first, so `cinfo.dest` casts back to the whole struct.
#[repr(C)]
struct Dest {
    iface: ffi::jpegli_destination_mgr,
    buf: Vec<u8>,
}

impl Dest {
    /// SAFETY: `cinfo.dest` must point at the `iface` of a live `Dest`.
    unsafe fn of(cinfo: &mut ffi::jpegli_compress_struct) -> &mut Dest {
        unsafe { &mut *cinfo.dest.cast::<Dest>() }
    }

    fn point_at_spare(&mut self) {
        let spare = self.buf.spare_capacity_mut();
        self.iface.next_output_byte = spare.as_mut_ptr().cast();
        self.iface.free_in_buffer = spare.len();
    }

    unsafe extern "C-unwind" fn init(cinfo: &mut ffi::jpegli_compress_struct) {
        // SAFETY: installed by `JpegliEncoder::new` on its own `Dest`.
        let d = unsafe { Dest::of(cinfo) };
        d.buf.clear();
        d.point_at_spare();
    }

    unsafe extern "C-unwind" fn empty(cinfo: &mut ffi::jpegli_compress_struct) -> ffi::boolean {
        // SAFETY: as in `init`; jpegli filled the whole spare capacity.
        let d = unsafe { Dest::of(cinfo) };
        unsafe { d.buf.set_len(d.buf.capacity()) };
        d.buf.reserve(d.buf.capacity());
        d.point_at_spare();
        1
    }

    unsafe extern "C-unwind" fn term(cinfo: &mut ffi::jpegli_compress_struct) {
        // SAFETY: as in `init`; the first `capacity - free` spare bytes
        // are written.
        let d = unsafe { Dest::of(cinfo) };
        let len = d.buf.capacity() - d.iface.free_in_buffer;
        unsafe { d.buf.set_len(len) };
        d.iface.free_in_buffer = 0;
    }
}

extern "C-unwind" fn silence_message(_cinfo: &mut ffi::jpegli_common_struct, _level: c_int) {}

/// Only the message code: the binding types `format_message`'s output
/// buffer as `&[u8; 80]`, and writing through a shared reference is not
/// something to build on. Encoder errors are misuse, not input faults.
extern "C-unwind" fn unwind_error_exit(cinfo: &mut ffi::jpegli_common_struct) {
    // SAFETY: `err` was installed by `JpegliEncoder::new` and is live.
    let code = unsafe { (*cinfo.err).msg_code };
    // resume_unwind skips the panic hook, as the crates' managers do.
    std::panic::resume_unwind(Box::new(format!("jpegli fatal error: code {code}")));
}

/// A started RGB encoder: markers and scanlines, then [`Self::finish`].
pub(super) struct JpegliEncoder {
    cinfo: Box<ffi::jpegli_compress_struct>,
    // Referenced by `cinfo` through raw pointers; boxed so moves of the
    // encoder leave them in place.
    _err: Box<ffi::jpegli_error_mgr>,
    dest: Box<Dest>,
}

impl JpegliEncoder {
    /// Start a `w` x `h` RGB encode at jpegli `quality`, progressive with
    /// [`SCAN_SCRIPT`] or baseline.
    pub(super) fn new(w: usize, h: usize, quality: f32, progressive: bool) -> JpegliEncoder {
        // SAFETY: zeroed structs are the libjpeg idiom before
        // jpegli_std_error / jpegli_CreateCompress initialize them; every
        // pointer handed to jpegli targets a Box owned by the returned
        // encoder, and SCAN_SCRIPT is static.
        unsafe {
            let mut err: Box<ffi::jpegli_error_mgr> = Box::new(mem::zeroed());
            ffi::jpegli_std_error(&mut err);
            err.error_exit = Some(unwind_error_exit);
            err.emit_message = Some(silence_message);
            let mut enc = JpegliEncoder {
                cinfo: Box::new(mem::zeroed()),
                _err: err,
                dest: Box::new(Dest {
                    iface: ffi::jpegli_destination_mgr {
                        next_output_byte: ptr::null_mut(),
                        free_in_buffer: 0,
                        init_destination: Some(Dest::init),
                        empty_output_buffer: Some(Dest::empty),
                        term_destination: Some(Dest::term),
                    },
                    buf: Vec::with_capacity(64 * 1024),
                }),
            };
            enc.cinfo.common.err = &mut *enc._err;
            let size = mem::size_of::<ffi::jpegli_compress_struct>();
            ffi::jpegli_CreateCompress(&mut enc.cinfo, ffi::JPEG_LIB_VERSION, size);
            enc.cinfo.in_color_space = ffi::J_COLOR_SPACE::JCS_RGB;
            enc.cinfo.input_components = 3;
            ffi::jpegli_set_defaults(&mut enc.cinfo);
            enc.cinfo.image_width = w as ffi::JDIMENSION;
            enc.cinfo.image_height = h as ffi::JDIMENSION;
            ffi::jpegli_set_quality(&mut enc.cinfo, quality as c_int, 0);
            if progressive {
                debug_assert_eq!(enc.cinfo.num_components, 3);
                enc.cinfo.scan_info = SCAN_SCRIPT.as_ptr();
                enc.cinfo.num_scans = SCAN_SCRIPT.len() as c_int;
            } else {
                // jpegli_set_defaults leaves sequential output on the
                // standard Huffman tables (progressive always optimizes).
                // Those cost 30-100% in bytes on small and graphic
                // outputs (issue #61); optimized tables cost a second
                // pass over the buffered coefficients, which is still
                // cheaper than the scan script.
                enc.cinfo.optimize_coding = 1;
            }
            enc.cinfo.dest = &mut enc.dest.iface;
            ffi::jpegli_start_compress(&mut enc.cinfo, 1);
            enc
        }
    }

    /// Write one marker (at most 65533 bytes of payload).
    pub(super) fn write_marker(&mut self, marker: c_int, data: &[u8]) {
        // SAFETY: started encoder; jpegli copies `data` before returning.
        unsafe {
            ffi::jpegli_write_marker(&mut self.cinfo, marker, data.as_ptr(), data.len() as _);
        }
    }

    /// Write whole RGB rows (`rows.len()` a multiple of `3 * w`), up to
    /// [`ROW_BATCH`] per call into jpegli.
    pub(super) fn write_scanlines(&mut self, rows: &[u8]) -> std::io::Result<()> {
        let stride = self.cinfo.image_width as usize * 3;
        debug_assert_eq!(rows.len() % stride, 0);
        let mut ptrs = [ptr::null(); ROW_BATCH];
        let mut pending = rows.chunks_exact(stride);
        loop {
            let mut n = 0;
            for (p, row) in ptrs.iter_mut().zip(&mut pending) {
                *p = row.as_ptr();
                n += 1;
            }
            if n == 0 {
                return Ok(());
            }
            let mut done = 0;
            while done < n {
                // SAFETY: started encoder; `ptrs[done..n]` point at rows of
                // `stride` bytes that jpegli only reads.
                let wrote = unsafe {
                    ffi::jpegli_write_scanlines(
                        &mut self.cinfo,
                        ptrs[done..].as_ptr(),
                        (n - done) as ffi::JDIMENSION,
                    )
                } as usize;
                // Zero only once the image already holds every row.
                if wrote == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                done += wrote;
            }
        }
    }

    /// Finish the image and return the JPEG bytes.
    pub(super) fn finish(mut self) -> Vec<u8> {
        // SAFETY: started encoder with every scanline written (jpegli
        // errors, and so unwinds, otherwise).
        unsafe { ffi::jpegli_finish_compress(&mut self.cinfo) };
        mem::take(&mut self.dest.buf)
    }
}

impl Drop for JpegliEncoder {
    fn drop(&mut self) {
        // SAFETY: created in `new`; destroy is valid in any state.
        unsafe {
            self.cinfo.dest = ptr::null_mut();
            ffi::jpegli_destroy_compress(&mut self.cinfo);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(px: &[u8], w: usize, h: usize, progressive: bool) -> Vec<u8> {
        let mut enc = JpegliEncoder::new(w, h, 85.0, progressive);
        enc.write_marker(JPEG_APP2, b"not an icc profile");
        enc.write_scanlines(px).unwrap();
        enc.finish()
    }

    fn decode(jpeg: &[u8]) -> Vec<u8> {
        let mut started = mozjpeg::Decompress::new_mem(jpeg).unwrap().rgb().unwrap();
        let mut out = vec![0u8; started.width() * started.height() * 3];
        started.read_scanlines_into(&mut out).unwrap();
        started.finish().unwrap();
        out
    }

    /// Marker segments in order as (code, payload), walking segment
    /// lengths and skipping entropy-coded data (stuffed 0xFF00 and RSTn
    /// are not markers).
    fn segments(jpeg: &[u8]) -> Vec<(u8, &[u8])> {
        let mut out = Vec::new();
        let mut i = 2;
        while i + 1 < jpeg.len() {
            let m = jpeg[i + 1];
            if m == 0xD9 {
                out.push((m, &jpeg[..0]));
                break;
            }
            let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
            out.push((m, &jpeg[i + 4..i + 2 + len]));
            i += 2 + len;
            if m == 0xDA {
                while !(jpeg[i] == 0xFF
                    && jpeg[i + 1] != 0
                    && !(0xD0..=0xD7).contains(&jpeg[i + 1]))
                {
                    i += 1;
                }
            }
        }
        out
    }

    /// Odd, non-MCU-aligned size, and big enough that the output
    /// outgrows the 64 KiB initial buffer.
    fn frame() -> (Vec<u8>, usize, usize) {
        let (w, h) = (613, 471);
        let mut seed = 0x9E3779B9u32;
        let px = (0..w * h * 3)
            .map(|i| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                ((i % (w * 3)) * 255 / (w * 3)) as u8 ^ (seed >> 26) as u8
            })
            .collect();
        (px, w, h)
    }

    #[test]
    fn scan_script_decodes_to_the_baseline_pixels() {
        let (px, w, h) = frame();
        let prog = encode(&px, w, h, true);
        let base = encode(&px, w, h, false);
        assert!(prog.len() > 64 * 1024, "{} bytes", prog.len());
        assert_ne!(prog, base);
        assert!(decode(&prog) == decode(&base), "scan script changed pixels");

        let seg = segments(&prog);
        let codes: Vec<u8> = seg.iter().map(|s| s.0).collect();
        assert!(codes.contains(&0xE2), "APP2 missing: {codes:x?}");
        // SOF2 lists (component id, sampling, table) triples after
        // precision, height, width and the component count.
        let sof = seg.iter().find(|s| s.0 == 0xC2).expect("not progressive").1;
        let ids: Vec<u8> = sof[6..].chunks(3).map(|c| c[0]).collect();
        // Each SOS as (component indices, Ss, Se, Ah, Al): SCAN_SCRIPT,
        // spelled out so a change to it has to change this too.
        let scans: Vec<(Vec<usize>, u8, u8, u8, u8)> = seg
            .iter()
            .filter(|s| s.0 == 0xDA)
            .map(|(_, p)| {
                let ns = usize::from(p[0]);
                let comps = (0..ns)
                    .map(|k| ids.iter().position(|&id| id == p[1 + 2 * k]).unwrap())
                    .collect();
                let t = &p[1 + 2 * ns..];
                (comps, t[0], t[1], t[2] >> 4, t[2] & 15)
            })
            .collect();
        let want: Vec<(Vec<usize>, u8, u8, u8, u8)> = vec![
            (vec![0, 1, 2], 0, 0, 0, 0),
            (vec![0], 1, 2, 0, 0),
            (vec![0], 3, 10, 0, 0),
            (vec![0], 11, 63, 0, 0),
            (vec![1], 1, 2, 0, 0),
            (vec![1], 3, 63, 0, 0),
            (vec![2], 1, 2, 0, 0),
            (vec![2], 3, 63, 0, 0),
        ];
        assert_eq!(scans, want);

        let seg = segments(&base);
        // jpegli's sequential output is SOF1 (extended), not SOF0.
        assert!(seg.iter().any(|s| s.0 == 0xC1), "not sequential");
        assert_eq!(seg.iter().filter(|s| s.0 == 0xDA).count(), 1);
    }

    /// Sequential output carries tables built for the image, not the
    /// standard ones jpegli_set_defaults leaves in place.
    #[test]
    fn sequential_output_optimizes_huffman_tables() {
        // JPEG Annex K.3: the standard luminance DC table's code counts.
        const STD_DC_LUMA: [u8; 17] = [0x00, 0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
        let (px, w, h) = frame();
        for (w, h) in [(w, h), (40, 30)] {
            let base = encode(&px[..w * h * 3], w, h, false);
            let dht: Vec<&[u8]> = segments(&base)
                .into_iter()
                .filter(|s| s.0 == 0xC4)
                .map(|s| s.1)
                .collect();
            assert!(!dht.is_empty(), "{w}x{h}: no DHT");
            assert!(
                dht.iter().all(|t| !t.starts_with(&STD_DC_LUMA)),
                "{w}x{h}: standard luminance DC table"
            );
        }
    }

    /// Batching must not depend on how the caller slices the rows: the
    /// whole frame at once (serial path) and one row at a time (fused
    /// path) produce the same file.
    #[test]
    fn write_granularity_does_not_change_bytes() {
        let (px, w, h) = frame();
        for progressive in [true, false] {
            let whole = encode(&px, w, h, progressive);
            let mut enc = JpegliEncoder::new(w, h, 85.0, progressive);
            enc.write_marker(JPEG_APP2, b"not an icc profile");
            for row in px.chunks_exact(w * 3) {
                enc.write_scanlines(row).unwrap();
            }
            assert!(enc.finish() == whole, "progressive={progressive}");
        }
    }

    #[test]
    fn fatal_errors_unwind() {
        let (px, w, h) = frame();
        let mut enc = JpegliEncoder::new(w, h, 85.0, true);
        enc.write_scanlines(&px[..w * 3 * 10]).unwrap();
        // Finishing with rows missing is a libjpeg fatal error.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| enc.finish()));
        let msg = *r.unwrap_err().downcast::<String>().unwrap();
        assert!(msg.starts_with("jpegli fatal error"), "{msg}");
    }
}
