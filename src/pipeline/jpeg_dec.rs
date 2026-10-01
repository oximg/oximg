//! libjpeg decompression over `mozjpeg-sys`, owned in-tree.
//!
//! The `mozjpeg` crate's `Decompress` keeps `jpeg_decompress_struct`
//! private and reads one scanline per `jpeg_read_scanlines` call, so
//! the decoder's internals are out of reach: reading several rows per
//! call, or installing an IDCT hook (issue #60). This wrapper drives
//! the same C library through the same entry points, with the same
//! error and end-of-input behaviour as that crate, so decoded pixels
//! are unchanged:
//!
//! - fatal libjpeg errors unwind out of `error_exit` with a
//!   `"libjpeg fatal error: ..."` payload, which `panic_guard`
//!   converts into an error; `Drop` releases the C state on the way
//!   out;
//! - warnings are silenced;
//! - a source that ends early is completed with a fake EOI marker
//!   (libjpeg's documented convention), so truncated files decode
//!   with fill data instead of failing; a read error is fatal.
//!
//! The encode side (`PRESET=fast|small`) still uses the crate.

use mozjpeg_sys as ffi;
use std::io::{self, BufRead};
use std::mem;
use std::os::raw::{c_int, c_long};
use std::ptr;

pub(crate) use mozjpeg::ColorSpace;

/// A header-parsed decoder: dimensions and color space are known,
/// decode parameters can still be set.
pub(crate) struct Decompress<R> {
    // Boxed so the addresses libjpeg holds (cinfo.err, cinfo.src)
    // survive moves of this struct.
    cinfo: Box<ffi::jpeg_decompress_struct>,
    err: Box<ffi::jpeg_error_mgr>,
    src: *mut Source<R>,
}

/// The most rows handed to one `jpeg_read_scanlines` call: one iMCU
/// row of 4:2:0 output at full scale (2 x 8). libjpeg may return fewer
/// per call; the loop asks again.
const ROWS_PER_CALL: usize = 16;

/// A decoder past `jpeg_start_decompress`: output dimensions are
/// final and scanlines can be read.
pub(crate) struct DecompressStarted<R> {
    dec: Decompress<R>,
}

impl<'a> Decompress<&'a [u8]> {
    pub(crate) fn new_mem(mem: &'a [u8]) -> io::Result<Self> {
        Self::new_reader(mem)
    }
}

impl<R: BufRead> Decompress<R> {
    /// Create the decoder and read the header. An empty source is an
    /// `UnexpectedEof` error before any libjpeg state exists; a
    /// malformed header unwinds (see the module docs).
    pub(crate) fn new_reader(reader: R) -> io::Result<Self> {
        let mut src = Box::new(Source {
            iface: ffi::jpeg_source_mgr {
                next_input_byte: ptr::null(),
                bytes_in_buffer: 0,
                init_source: Some(Source::<R>::init_source),
                fill_input_buffer: Some(Source::<R>::fill_input_buffer),
                skip_input_data: Some(Source::<R>::skip_input_data),
                resync_to_restart: Some(ffi::jpeg_resync_to_restart),
                term_source: Some(Source::<R>::term_source),
            },
            to_consume: 0,
            reader,
        });
        src.fill()?;
        // SAFETY: both structs are plain C data for which all-zero is
        // the state libjpeg's own initializers expect to start from.
        let mut err: Box<ffi::jpeg_error_mgr> = Box::new(unsafe { mem::zeroed() });
        unsafe { ffi::jpeg_std_error(&mut err) };
        err.error_exit = Some(error_exit);
        err.emit_message = Some(silence_message);
        let mut dec = Decompress {
            cinfo: Box::new(unsafe { mem::zeroed() }),
            err,
            src: Box::into_raw(src),
        };
        dec.cinfo.common.err = &mut *dec.err;
        // SAFETY: cinfo is zeroed with its error manager set, as
        // jpeg_create_decompress requires; `iface` is the first field
        // of the repr(C) Source, so the cast is the field's address.
        unsafe {
            ffi::jpeg_create_decompress(&mut *dec.cinfo);
        }
        dec.cinfo.src = dec.src.cast();
        // require_image = 0: a tables-only stream is an Err, not an
        // unwind.
        if unsafe { ffi::jpeg_read_header(&mut dec.cinfo, 0) } != 1 {
            return Err(io::Error::other("no image in the JPEG file"));
        }
        Ok(dec)
    }
}

impl<R> Decompress<R> {
    /// Source dimensions.
    pub(crate) fn size(&self) -> (usize, usize) {
        (
            self.cinfo.image_width as usize,
            self.cinfo.image_height as usize,
        )
    }

    pub(crate) fn color_space(&self) -> ColorSpace {
        self.cinfo.jpeg_color_space
    }

    pub(crate) fn num_components(&self) -> usize {
        self.cinfo.num_components.max(0) as usize
    }

    /// DCT-domain scaling by `numerator / 8`.
    #[track_caller]
    pub(crate) fn scale(&mut self, numerator: u8) {
        assert!(
            (1..=16).contains(&numerator),
            "numerator must be between 1 and 16"
        );
        self.cinfo.scale_num = numerator.into();
        self.cinfo.scale_denom = 8;
    }

    pub(crate) fn do_fancy_upsampling(&mut self, value: bool) {
        self.cinfo.do_fancy_upsampling = ffi::boolean::from(value);
    }

    /// Component `i`'s (horizontal, vertical) sampling factors.
    pub(crate) fn sampling(&self, i: usize) -> (i32, i32) {
        let c = component(&self.cinfo, i);
        (c.h_samp_factor, c.v_samp_factor)
    }

    pub(crate) fn rgb(self) -> io::Result<DecompressStarted<R>> {
        self.start(ColorSpace::JCS_RGB)
    }

    /// Start in raw data mode: the component planes as stored, with no
    /// upsampling or color conversion, read with
    /// [`DecompressStarted::read_raw_chunk`].
    pub(crate) fn raw(mut self) -> io::Result<DecompressStarted<R>> {
        self.cinfo.raw_data_out = 1;
        let cs = self.cinfo.jpeg_color_space;
        self.start(cs)
    }

    pub(crate) fn start(mut self, colorspace: ColorSpace) -> io::Result<DecompressStarted<R>> {
        self.cinfo.out_color_space = colorspace;
        if unsafe { ffi::jpeg_start_decompress(&mut self.cinfo) } == 0 {
            // Only a suspending source returns 0, and this one never
            // suspends.
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(DecompressStarted { dec: self })
    }
}

impl<R> DecompressStarted<R> {
    /// Output dimensions, after any DCT scaling.
    pub(crate) fn width(&self) -> usize {
        self.dec.cinfo.output_width as usize
    }

    pub(crate) fn height(&self) -> usize {
        self.dec.cinfo.output_height as usize
    }

    /// Fill `dest` with whole output rows, returning it. `dest` must
    /// hold a whole number of rows; reading past the last row is an
    /// `UnexpectedEof` error.
    ///
    /// Rows are requested up to [`ROWS_PER_CALL`] at a time. Asked for
    /// a single row, libjpeg's merged upsampler decodes its two-row
    /// group into a spare buffer and copies a row out per call; handed
    /// room for the group, it writes into `dest` directly.
    pub(crate) fn read_scanlines_into<'d>(
        &mut self,
        dest: &'d mut [u8],
    ) -> io::Result<&'d mut [u8]> {
        let cinfo = &mut *self.dec.cinfo;
        let line = cinfo.output_width as usize * cinfo.output_components.max(0) as usize;
        if line == 0 || !dest.len().is_multiple_of(line) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "destination of {}B is not a whole number of {line}B rows",
                    dest.len()
                ),
            ));
        }
        let rows = dest.len() / line;
        let base = dest.as_mut_ptr();
        let mut ptrs = [ptr::null_mut::<ffi::JSAMPLE>(); ROWS_PER_CALL];
        let mut done = 0;
        while done < rows {
            if cinfo.output_scanline >= cinfo.output_height {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let want = (rows - done).min(ROWS_PER_CALL);
            for (i, p) in ptrs[..want].iter_mut().enumerate() {
                // SAFETY: done + i < rows, so the row lies inside dest.
                *p = unsafe { base.add((done + i) * line) };
            }
            // SAFETY: `want` pointers to disjoint `line`-byte rows of
            // dest; libjpeg writes at most `want` rows.
            let read = unsafe { ffi::jpeg_read_scanlines(cinfo, ptrs.as_mut_ptr(), want as _) };
            if read == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            done += read as usize;
        }
        Ok(dest)
    }

    /// Raw data mode: component `i`'s vertical sampling factor and its
    /// plane's row stride in bytes (whole DCT blocks, so a row runs
    /// past the image width).
    pub(crate) fn raw_plane(&self, i: usize) -> (usize, usize) {
        let c = component(&self.dec.cinfo, i);
        (
            c.v_samp_factor.max(0) as usize,
            c.width_in_blocks as usize * ffi::DCTSIZE,
        )
    }

    /// Raw data mode: decode the next iMCU row into the three planes, in
    /// rows of [`Self::raw_plane`]'s stride: `v_samp_factor * 8` rows
    /// per component, past the image height on the last iMCU row (their
    /// content is padding). The planes are grown as needed and reused
    /// as they are, since libjpeg writes every row. Reading past the
    /// last row is an `UnexpectedEof` error.
    pub(crate) fn read_raw_chunk(&mut self, planes: &mut [Vec<u8>; 3]) -> io::Result<()> {
        let cinfo = &mut *self.dec.cinfo;
        assert!(
            cinfo.raw_data_out != 0 && cinfo.num_components == 3,
            "raw reads need a 3-component decoder started with raw()"
        );
        if cinfo.output_scanline >= cinfo.output_height {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        // libjpeg caps sampling factors at 4: 32 rows per component.
        const MAX_ROWS: usize = 4 * ffi::DCTSIZE;
        let imcu = cinfo.max_v_samp_factor.max(0) as usize * ffi::DCTSIZE;
        let mut rows = [[ptr::null_mut::<ffi::JSAMPLE>(); MAX_ROWS]; 3];
        let mut comps = [ptr::null_mut::<ffi::JSAMPROW_MUT>(); 3];
        for (i, (plane, (rows, comp))) in planes
            .iter_mut()
            .zip(rows.iter_mut().zip(comps.iter_mut()))
            .enumerate()
        {
            let c = component(cinfo, i);
            let (h, stride) = (
                c.v_samp_factor.max(0) as usize * ffi::DCTSIZE,
                c.width_in_blocks as usize * ffi::DCTSIZE,
            );
            assert!(h <= MAX_ROWS, "sampling factor above 4");
            if plane.len() < h * stride {
                plane.resize(h * stride, 0);
            }
            for (r, p) in rows[..h].iter_mut().enumerate() {
                // SAFETY: r < h, so the row lies inside the plane.
                *p = unsafe { plane.as_mut_ptr().add(r * stride) };
            }
            *comp = rows.as_mut_ptr();
        }
        // SAFETY: each component gets v_samp_factor * 8 pointers to
        // disjoint `stride`-byte rows, the most one iMCU row writes.
        let read = unsafe { ffi::jpeg_read_raw_data(cinfo, comps.as_mut_ptr(), imcu as _) };
        if read as usize != imcu {
            // Only a suspending source reads less, and this one never
            // suspends.
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> io::Result<()> {
        if unsafe { ffi::jpeg_finish_decompress(&mut self.dec.cinfo) } == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(())
    }
}

/// Component `i`'s info, which libjpeg fills in at the header parse.
fn component(cinfo: &ffi::jpeg_decompress_struct, i: usize) -> &ffi::jpeg_component_info {
    assert!(
        i < cinfo.num_components.max(0) as usize,
        "component {i} out of range"
    );
    // SAFETY: comp_info holds num_components entries once the header is
    // read, which every Decompress has done.
    unsafe { &*cinfo.comp_info.add(i) }
}

impl<R> Drop for Decompress<R> {
    fn drop(&mut self) {
        // SAFETY: cinfo was created in new_reader (destroy is also safe
        // on a struct whose create unwound part-way: it frees only what
        // the memory manager recorded). libjpeg holds no reference to
        // the source once destroyed, so it is freed after.
        unsafe {
            ffi::jpeg_destroy_decompress(&mut self.cinfo);
            drop(Box::from_raw(self.src));
        }
    }
}

/// The `jpeg_source_mgr` over a `BufRead`. libjpeg reads straight out
/// of the reader's buffer; `to_consume` is how much of it libjpeg was
/// handed, consumed on the next refill.
#[repr(C)]
struct Source<R> {
    iface: ffi::jpeg_source_mgr,
    to_consume: usize,
    reader: R,
}

/// libjpeg's convention for a source that ends early: hand it an EOI
/// marker so the decode completes with fill data.
static FAKE_EOI: [u8; 4] = [0xFF, 0xD9, 0xFF, 0xD9];

impl<R: BufRead> Source<R> {
    /// Recover the Source from cinfo.src, refusing any other source
    /// manager. The Source is a separate allocation, so the reference
    /// does not borrow cinfo; it lives as long as the Decompress.
    unsafe fn from_cinfo<'s>(cinfo: &mut ffi::jpeg_decompress_struct) -> &'s mut Self {
        let src = cinfo.src.cast::<Self>();
        // The init_source pointer identifies a source manager as ours.
        type Init = unsafe extern "C-unwind" fn(&mut ffi::jpeg_decompress_struct);
        #[allow(unpredictable_function_pointer_comparisons)]
        let ours = !src.is_null()
            && unsafe { (*src).iface.init_source } == Some(Self::init_source as Init);
        if !ours {
            fail(&mut cinfo.common, ffi::JERR_VIRTUAL_BUG);
        }
        unsafe { &mut *src }
    }

    fn fill(&mut self) -> io::Result<()> {
        self.reader.consume(self.to_consume);
        self.to_consume = 0;
        let buf = self.reader.fill_buf()?;
        self.to_consume = buf.len();
        self.iface.next_input_byte = buf.as_ptr();
        self.iface.bytes_in_buffer = buf.len();
        if buf.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(())
    }

    /// Hand back whatever libjpeg was given but did not use.
    fn return_unconsumed(&mut self) {
        let unused = self.to_consume.saturating_sub(self.iface.bytes_in_buffer);
        self.to_consume = 0;
        self.reader.consume(unused);
    }

    unsafe extern "C-unwind" fn init_source(cinfo: &mut ffi::jpeg_decompress_struct) {
        // The first fill happened in new_reader.
        let _ = unsafe { Self::from_cinfo(cinfo) };
    }

    unsafe extern "C-unwind" fn fill_input_buffer(
        cinfo: &mut ffi::jpeg_decompress_struct,
    ) -> ffi::boolean {
        let this = unsafe { Self::from_cinfo(cinfo) };
        match this.fill() {
            Ok(()) => 1,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                this.iface.next_input_byte = FAKE_EOI.as_ptr();
                this.iface.bytes_in_buffer = FAKE_EOI.len();
                warn(&mut cinfo.common, ffi::JWRN_JPEG_EOF);
                1
            }
            Err(_) => fail(&mut cinfo.common, ffi::JERR_FILE_READ),
        }
    }

    unsafe extern "C-unwind" fn skip_input_data(
        cinfo: &mut ffi::jpeg_decompress_struct,
        num_bytes: c_long,
    ) {
        let Ok(mut left) = usize::try_from(num_bytes) else {
            return;
        };
        let this = unsafe { Self::from_cinfo(cinfo) };
        loop {
            let skip = this.iface.bytes_in_buffer.min(left);
            this.iface.bytes_in_buffer -= skip;
            // SAFETY: skip <= bytes_in_buffer, within the current buffer.
            this.iface.next_input_byte = unsafe { this.iface.next_input_byte.add(skip) };
            left -= skip;
            if left == 0 {
                return;
            }
            if this.fill().is_err() {
                fail(&mut cinfo.common, ffi::JERR_FILE_READ);
            }
        }
    }

    unsafe extern "C-unwind" fn term_source(cinfo: &mut ffi::jpeg_decompress_struct) {
        unsafe { Self::from_cinfo(cinfo) }.return_unconsumed();
    }
}

#[cold]
fn fail(cinfo: &mut ffi::jpeg_common_struct, code: c_int) -> ! {
    // SAFETY: cinfo.err is the error manager new_reader installed.
    unsafe {
        let err = &mut *cinfo.err;
        err.msg_code = code;
        if let Some(exit) = err.error_exit {
            exit(cinfo);
        }
    }
    std::process::abort();
}

fn warn(cinfo: &mut ffi::jpeg_common_struct, code: c_int) {
    // SAFETY: as in `fail`.
    unsafe {
        let err = &mut *cinfo.err;
        err.msg_code = code;
        if let Some(emit) = err.emit_message {
            emit(cinfo, -1);
        }
    }
}

#[cold]
unsafe extern "C-unwind" fn silence_message(_cinfo: &mut ffi::jpeg_common_struct, _level: c_int) {}

/// Fatal libjpeg errors unwind with the formatted message as payload.
/// `resume_unwind` skips the panic hook; `panic_guard` catches it.
#[cold]
unsafe extern "C-unwind" fn error_exit(cinfo: &mut ffi::jpeg_common_struct) {
    let mut buf = [0u8; 80];
    // SAFETY: cinfo.err is ours; format_message writes a NUL-terminated
    // message of at most JMSG_LENGTH_MAX (80) bytes into the buffer.
    // mozjpeg-sys declares the buffer parameter as a shared reference,
    // so the pointer type is adjusted to what the C side really does.
    let msg = unsafe {
        match (*cinfo.err).format_message {
            Some(format) => {
                type Writes =
                    unsafe extern "C-unwind" fn(&mut ffi::jpeg_common_struct, &mut [u8; 80]);
                let format: Writes = mem::transmute::<
                    unsafe extern "C-unwind" fn(&mut ffi::jpeg_common_struct, &[u8; 80]),
                    Writes,
                >(format);
                format(cinfo, &mut buf);
                let text = buf.split(|&c| c == 0).next().unwrap_or_default();
                String::from_utf8_lossy(text).into_owned()
            }
            None => format!("code {}", (*cinfo.err).msg_code),
        }
    };
    std::panic::resume_unwind(Box::new(format!("libjpeg fatal error: {msg}")));
}

/// Parity with the `mozjpeg` crate's decoder, which this replaces: the
/// same pixels for every decode setting the pipeline uses, and the same
/// outcome for inputs that fail.
#[cfg(test)]
mod tests {
    use super::*;

    /// How one decode ended: pixels and output size, an `Err`, or an
    /// unwind with its message.
    #[derive(Debug, PartialEq)]
    enum Outcome {
        Pixels(usize, usize, Vec<u8>),
        Err(io::ErrorKind),
        Unwind(String),
    }

    #[derive(Clone, Copy, Debug)]
    struct Setup {
        scale: u8,
        fancy: bool,
        out: ColorSpace,
    }

    fn catch(f: impl FnOnce() -> io::Result<(usize, usize, Vec<u8>)>) -> Outcome {
        match crate::panic_guard::catch_unwind_as_error("test", f) {
            Ok(Ok((w, h, px))) => Outcome::Pixels(w, h, px),
            Ok(Err(e)) => Outcome::Err(e.kind()),
            Err(e) => Outcome::Unwind(e.to_string()),
        }
    }

    fn ours(jpeg: &[u8], s: Setup) -> Outcome {
        catch(|| {
            let mut dec = Decompress::new_mem(jpeg)?;
            dec.scale(s.scale);
            dec.do_fancy_upsampling(s.fancy);
            let mut started = dec.start(s.out)?;
            let (w, h) = (started.width(), started.height());
            let comps = if s.out == ColorSpace::JCS_CMYK { 4 } else { 3 };
            let mut px = vec![0u8; w * h * comps];
            // Odd chunking, like the pipeline's chunked loops.
            for chunk in px.chunks_mut(w * comps * 7) {
                started.read_scanlines_into(chunk)?;
            }
            started.finish()?;
            Ok((w, h, px))
        })
    }

    fn crate_decoder(jpeg: &[u8], s: Setup) -> Outcome {
        catch(|| {
            let mut dec = mozjpeg::Decompress::new_mem(jpeg)?;
            dec.scale(s.scale);
            dec.do_fancy_upsampling(s.fancy);
            let mut started = dec.to_colorspace(s.out)?;
            let (w, h) = (started.width(), started.height());
            let px: Vec<u8> = started.read_scanlines()?;
            started.finish()?;
            Ok((w, h, px))
        })
    }

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// A 4:2:0 baseline and a progressive photo-like JPEG with odd
    /// dimensions, so scaled sizes round and MCU rows end ragged.
    fn generated() -> Vec<(String, Vec<u8>)> {
        let (w, h) = (333, 217);
        let px: Vec<u8> = (0..w * h * 3)
            .map(|i| ((i % (w * 3)) * 255 / (w * 3) + (i / (w * 3)) % 37) as u8)
            .collect();
        [false, true]
            .into_iter()
            .map(|progressive| {
                let mut c = mozjpeg::Compress::new(ColorSpace::JCS_RGB);
                c.set_size(w, h);
                c.set_quality(85.0);
                if progressive {
                    c.set_progressive_mode();
                }
                let mut started = c.start_compress(Vec::new()).unwrap();
                started.write_scanlines(&px).unwrap();
                (
                    format!("generated progressive={progressive}"),
                    started.finish().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn decodes_like_the_mozjpeg_crate() {
        let mut sources = generated();
        for name in ["photo.jpg", "tiny.jpg"] {
            sources.push((name.to_string(), fixture(name)));
        }
        for (name, jpeg) in &sources {
            for scale in [8, 7, 5, 4, 3, 2, 1] {
                for fancy in [true, false] {
                    let s = Setup {
                        scale,
                        fancy,
                        out: ColorSpace::JCS_RGB,
                    };
                    let got = ours(jpeg, s);
                    assert!(matches!(got, Outcome::Pixels(..)), "{name} {s:?}: {got:?}");
                    assert!(got == crate_decoder(jpeg, s), "{name} {s:?}");
                }
            }
        }
    }

    #[test]
    fn decodes_cmyk_and_ycck_like_the_mozjpeg_crate() {
        for name in [
            "cmyk_icc.jpg",
            "cmyk_noadobe.jpg",
            "cmyk_prog.jpg",
            "cmyk_sub.jpg",
            "cmyk_t0.jpg",
            "cmyk_ycck.jpg",
        ] {
            let jpeg = fixture(name);
            for scale in [8, 4] {
                let s = Setup {
                    scale,
                    fancy: true,
                    out: ColorSpace::JCS_CMYK,
                };
                let got = ours(&jpeg, s);
                assert!(matches!(got, Outcome::Pixels(..)), "{name} {s:?}: {got:?}");
                assert!(got == crate_decoder(&jpeg, s), "{name} {s:?}");
            }
        }
    }

    #[test]
    fn fails_like_the_mozjpeg_crate() {
        let rgb = Setup {
            scale: 8,
            fancy: true,
            out: ColorSpace::JCS_RGB,
        };
        let (_, jpeg) = generated().remove(0);
        let mut cases: Vec<(String, Vec<u8>)> = vec![
            ("bogus Huffman table".into(), fixture("bogus_huffman.jpg")),
            ("empty".into(), Vec::new()),
            ("SOI only".into(), vec![0xFF, 0xD8]),
            // SOI, then EOI before any frame: a tables-only stream.
            ("no image".into(), vec![0xFF, 0xD8, 0xFF, 0xD9]),
            ("not a JPEG".into(), b"GIF89a, not a JPEG at all".to_vec()),
        ];
        // Truncated inside the header, and at points inside the scan,
        // where the fake EOI completes the image with fill data.
        for keep in [20, 200, jpeg.len() / 3, jpeg.len() * 3 / 5, jpeg.len() - 2] {
            cases.push((format!("truncated to {keep}B"), jpeg[..keep].to_vec()));
        }
        for (name, bytes) in &cases {
            assert_eq!(ours(bytes, rgb), crate_decoder(bytes, rgb), "{name}");
        }
        assert!(
            matches!(ours(&cases[0].1, rgb), Outcome::Unwind(ref m) if m.contains("libjpeg fatal error: ")),
            "a corrupt scan unwinds with libjpeg's message"
        );
    }

    #[test]
    fn rows_per_call_never_changes_the_pixels() {
        // Requests that split, match and span libjpeg's row groups,
        // under both upsamplers (merged when fancy is off).
        for (name, jpeg) in generated() {
            for scale in [8, 4] {
                for fancy in [true, false] {
                    let decode = |rows: usize| {
                        let mut dec = Decompress::new_mem(&jpeg).unwrap();
                        dec.scale(scale);
                        dec.do_fancy_upsampling(fancy);
                        let mut started = dec.rgb().unwrap();
                        let (w, h) = (started.width(), started.height());
                        let mut px = vec![0u8; w * h * 3];
                        for chunk in px.chunks_mut(w * 3 * rows) {
                            started.read_scanlines_into(chunk).unwrap();
                        }
                        started.finish().unwrap();
                        px
                    };
                    let one = decode(1);
                    for rows in [2, 3, 15, 16, 17, 1000] {
                        assert!(
                            decode(rows) == one,
                            "{name} scale={scale} fancy={fancy} rows={rows}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn rejects_partial_rows_and_reads_past_the_end() {
        let (_, jpeg) = generated().remove(0);
        let mut started = Decompress::new_mem(&jpeg).unwrap().rgb().unwrap();
        let (w, h) = (started.width(), started.height());
        let e = started
            .read_scanlines_into(&mut vec![0u8; w * 3 + 1])
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        let mut all = vec![0u8; w * h * 3];
        started.read_scanlines_into(&mut all).unwrap();
        let e = started
            .read_scanlines_into(&mut vec![0u8; w * 3])
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
        started.finish().unwrap();
    }

    #[test]
    fn term_source_hands_back_what_libjpeg_did_not_read() {
        // Bytes after EOI stay in the reader, as with the crate's
        // source manager.
        let (_, mut jpeg) = generated().remove(0);
        let n = jpeg.len();
        jpeg.extend_from_slice(b"trailer");
        let mut reader = std::io::BufReader::with_capacity(64, &jpeg[..]);
        {
            let mut started = Decompress::new_reader(&mut reader).unwrap().rgb().unwrap();
            let (w, h) = (started.width(), started.height());
            started
                .read_scanlines_into(&mut vec![0u8; w * h * 3])
                .unwrap();
            started.finish().unwrap();
        }
        let mut rest = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut rest).unwrap();
        assert!(rest.ends_with(b"trailer") && rest.len() <= jpeg.len() - n + 2);
    }
}
