//! Bench-only access to the production animation loop. Refusals are errors,
//! never a fast still output mislabeled as a successful animation measurement.
//! Reference capture/decoding runs separately from timing and RSS collection.

use super::gif::{AnimationSink, GifReport, try_animated};
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Lossy,
    Lossless,
    Mixed,
    Reference,
}

pub struct Output {
    pub bytes: Vec<u8>,
    pub decision: String,
}

pub fn encode(src: &[u8], params: &Params, mode: Mode) -> Result<Output> {
    let header = src.get(..12).context("source too short")?.try_into()?;
    anyhow::ensure!(
        ImageFormat::sniff(header) == Some(ImageFormat::Gif),
        "expected GIF"
    );
    // The production memory model prices lossy coding. Treating that estimate
    // as a bound for VP8L, mixed search or retained raw frames would be unsafe.
    anyhow::ensure!(
        crate::config::config().max_decoded_bytes.is_none(),
        "unset OXIMG_MAX_DECODED_BYTES for this offline experiment; alternative mode memory is not modeled"
    );
    let p = clamp_to_format(Resolved::new(params), ImageFormat::Webp);
    let _active = ActiveGuard::enter();
    SCRATCH.with(|s| {
        let s = &mut *s.borrow_mut();
        s.held_source_bytes = src.len();
        s.reclaim_handed_frame();
        let mut report = GifReport::new();
        let out = if mode == Mode::Reference {
            try_animated(s, src, &p, &mut report, |w, h, loops, _| {
                Capture::new(w, h, loops)
            })?
        } else {
            try_animated(s, src, &p, &mut report, |w, h, loops, p| {
                AnimEncoder::configured(w, h, loops, p, |opts, config| {
                    config.lossless = i32::from(mode == Mode::Lossless);
                    opts.allow_mixed = i32::from(mode == Mode::Mixed);
                    // Pin lossless fidelity even behind transparent pixels;
                    // the quality comparison also reports visible RGBA error.
                    config.exact = i32::from(mode != Mode::Lossy);
                })
            })?
        };
        let decision = report.to_string();
        Ok(Output {
            bytes: out.with_context(|| format!("animation declined: {decision}"))?,
            decision,
        })
    })
}

// Little-endian bench interchange: magic, width, height, total plays, frame
// count, total ms, followed by (start ms, full-canvas RGBA8) records. This
// preserves millisecond timestamps (ImageMagick's %T rounds to centiseconds).
const MAGIC: &[u8; 8] = b"OXGIFR01";
const MAX_CAPTURE: usize = 256 << 20;

struct Capture {
    data: Vec<u8>,
    pixels: usize,
    frames: u32,
}

impl Capture {
    fn new(w: usize, h: usize, loops: u32) -> Result<Self> {
        let pixels = w.checked_mul(h).and_then(|n| n.checked_mul(4));
        let pixels = pixels
            .filter(|n| *n <= MAX_CAPTURE)
            .context("reference canvas too large")?;
        let mut data = MAGIC.to_vec();
        for n in [w as u32, h as u32, loops, 0, 0] {
            data.extend(n.to_le_bytes());
        }
        Ok(Self {
            data,
            pixels,
            frames: 0,
        })
    }
}

impl AnimationSink for Capture {
    fn add(&mut self, rgba: &[u8], ts_ms: i32) -> Result<()> {
        anyhow::ensure!(
            rgba.len() == self.pixels && ts_ms >= 0,
            "invalid reference frame"
        );
        anyhow::ensure!(
            self.data.len() + 4 + rgba.len() <= MAX_CAPTURE,
            "reference exceeds 256 MiB; use a smaller box or shorter corpus item"
        );
        self.data.extend((ts_ms as u32).to_le_bytes());
        self.data.extend(rgba);
        self.frames += 1;
        Ok(())
    }

    fn finish(mut self, total_ms: i32) -> Result<Vec<u8>> {
        self.data[20..24].copy_from_slice(&self.frames.to_le_bytes());
        self.data[24..28].copy_from_slice(&(total_ms as u32).to_le_bytes());
        Ok(self.data)
    }
}

/// Decode the actual WebP container, including a single frame if libwebp
/// merged the animation. Its timeline must still match the reference.
pub fn decode_webp(bytes: &[u8]) -> Result<Vec<u8>> {
    use libwebp_sys as wp;
    struct Decoder(*mut wp::WebPAnimDecoder);
    impl Drop for Decoder {
        fn drop(&mut self) {
            // SAFETY: owns the non-null decoder returned below, once.
            unsafe { wp::WebPAnimDecoderDelete(self.0) };
        }
    }
    // SAFETY: bytes outlives the decoder. Buffers returned by GetNext are
    // copied before the next call; their length comes from GetInfo. The
    // RAII guard frees the decoder on success and on every error path.
    unsafe {
        let data = wp::WebPData {
            bytes: bytes.as_ptr(),
            size: bytes.len(),
        };
        let mut opts = std::mem::zeroed::<wp::WebPAnimDecoderOptions>();
        anyhow::ensure!(
            wp::WebPAnimDecoderOptionsInit(&mut opts) != 0,
            "WebP decoder ABI"
        );
        opts.color_mode = wp::WEBP_CSP_MODE::MODE_RGBA;
        let dec = wp::WebPAnimDecoderNew(&data, &opts);
        anyhow::ensure!(!dec.is_null(), "invalid WebP animation");
        let dec = Decoder(dec);
        let mut info = wp::WebPAnimInfo::default();
        anyhow::ensure!(
            wp::WebPAnimDecoderGetInfo(dec.0, &mut info) != 0,
            "WebP info"
        );
        let mut capture = Capture::new(
            info.canvas_width as usize,
            info.canvas_height as usize,
            info.loop_count,
        )?;
        let mut start = 0;
        while wp::WebPAnimDecoderHasMoreFrames(dec.0) != 0 {
            let (mut buf, mut end) = (std::ptr::null_mut(), 0);
            anyhow::ensure!(
                wp::WebPAnimDecoderGetNext(dec.0, &mut buf, &mut end) != 0,
                "WebP frame decode"
            );
            anyhow::ensure!(
                end > start && !buf.is_null(),
                "non-increasing WebP timestamp"
            );
            capture.add(std::slice::from_raw_parts(buf, capture.pixels), start)?;
            start = end;
        }
        capture.finish(start)
    }
}
