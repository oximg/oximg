//! `strip`: shell out to `oximg strip`, then prove what it wrote: the
//! decoded pixels match the input's (every frame's bitstream, for an
//! animated WebP), and a second strip of the output under the same ICC
//! policy finds nothing left to remove.

use crate::args::{Opts, StripArgs};
use crate::cmd::resize::peak_rss_bytes;
use crate::error::{CtlError, emit};
use crate::report::{probe_value, sha256_hex};
use crate::spawn::resolve_bin;
use oximg::pipeline::{self, ImageFormat, Params, StripParams};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

pub fn run(opts: &Opts, a: &StripArgs) -> Result<(), CtlError> {
    let bin = resolve_bin(opts.bin.as_deref())?;
    let ephemeral = a.out.is_none();
    let out_path: PathBuf = a.out.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!(
            "oximg-ctl-{}-{}-strip.out",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ))
    });
    let argv = strip_argv(&a.input, &out_path, a.icc.as_deref(), a.auto_rotate);
    if opts.dry_run {
        emit(
            &json!({
                "ok": true,
                "dry_run": true,
                "bin": bin.display().to_string(),
                "argv": argv,
            }),
            opts.pretty,
        );
        return Ok(());
    }
    struct RemoveOnDrop(Option<PathBuf>);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            if let Some(p) = self.0.take() {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    let _ephemeral = RemoveOnDrop(ephemeral.then(|| out_path.clone()));
    let read = |p: &Path| {
        std::fs::read(p).map_err(|e| CtlError::fail(format!("read {}: {e}", p.display()), None))
    };
    // Before the child runs: `--out` may be the input itself.
    let src = read(&a.input)?;
    let t0 = Instant::now();
    let mut cmd = Command::new(&bin);
    cmd.args(&argv[1..]);
    for (k, v) in &opts.env {
        cmd.env(k, v);
    }
    let output = cmd.output().map_err(|e| {
        CtlError::fail(
            format!("run {}: {e}", bin.display()),
            Some("pass --bin at the oximg executable"),
        )
    })?;
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let code = output.status.code().unwrap_or(1);
        let msg = format!("oximg strip exited {code}: {stderr}");
        return Err(if code == 2 {
            CtlError::usage(msg)
        } else {
            CtlError::fail(msg, None)
        });
    }
    let bytes = read(&out_path)?;

    // The child resolved --icc against its own environment; mirror that
    // (flag, else the OXIMG_ICC it was given, else ours, trimmed as
    // config::var does) so `removed` describes the run that happened.
    let keep_icc = match a.icc.as_deref() {
        Some(v) => v == "keep",
        None => {
            opts.env
                .iter()
                .rev()
                .find(|(k, _)| k == "OXIMG_ICC")
                .map(|(_, v)| v.clone())
                .or_else(|| std::env::var("OXIMG_ICC").ok())
                .as_deref()
                .map(str::trim)
                != Some("0")
        }
    };
    let strip_with = |b: &[u8], auto_rotate| {
        pipeline::strip(
            b,
            &StripParams {
                icc: Some(keep_icc),
                auto_rotate,
            },
        )
    };
    let residue = strip_with(&bytes, false);
    // Whether the child rotated is read from what it wrote, not from
    // this process's library: the child's OXIMG_MAX_SRC_PIXELS (from
    // --env) bounds its rotation, and our config may differ. A tagged
    // source whose output carries no tag was turned upright.
    let plain = strip_with(&src, false);
    let source_orientation = plain.as_ref().ok().and_then(|s| s.orientation);
    let output_tagged = residue.as_ref().map_or(true, |s| s.orientation.is_some());
    let rotated = source_orientation.filter(|_| a.auto_rotate && !output_tagged);
    let not_rotated = (a.auto_rotate && source_orientation.is_some() && rotated.is_none())
        .then(|| child_not_rotated(&stderr));
    let removed = if rotated.is_some() {
        strip_with(&src, true)
    } else {
        plain
    };
    // Pixels, residue and frames cannot see what must be kept (the
    // orientation EXIF, the ICC profile); the library's own answer for
    // the input pins those down, byte for byte.
    let matches_library = matches!(&removed, Ok(s) if s.bytes == bytes);
    // A rotated output cannot match the stored pixels; `rotation`
    // proves it instead.
    let (pin, pout) = match rotated {
        None => (pixels(&src), pixels(&bytes)),
        Some(_) => (Ok(String::new()), Ok(String::new())),
    };
    let rotation = rotated.map(|o| rotation_proof(&src, &bytes, o, keep_icc));
    let frames_identical = match (webp_frames(&src), webp_frames(&bytes)) {
        (None, None) => None,
        (i, o) => Some(i == o),
    };
    let mut report = json!({
        "ok": true,
        "in_bytes": src.len(),
        "bytes": bytes.len(),
        "sha256": sha256_hex(&bytes),
        "ms": (ms * 10.0).round() / 10.0,
        "probe": probe_value(&bytes),
        "icc": if keep_icc { "keep" } else { "strip" },
        "removed": match &removed {
            Ok(s) => removed_value(&s.removed),
            Err(e) => json!({ "error": e.to_string() }),
        },
        // Read back from the output: what a viewer will honor.
        "orientation": residue.as_ref().ok().and_then(|s| s.orientation),
        "matches_library": matches_library,
        "residue": match &residue {
            Ok(s) => removed_value(&s.removed),
            Err(e) => json!({ "error": e.to_string() }),
        },
        "pixels_identical": match (&pin, &pout) {
            _ if rotated.is_some() => Value::Null,
            (Ok(i), Ok(o)) => json!(i == o),
            (Err(e), _) | (_, Err(e)) => json!({ "error": e }),
        },
        "rotated": rotated,
        "not_rotated": not_rotated,
        "rotation": rotation.as_ref().map(Rotation::value),
        "frames_identical": frames_identical,
        "peak_rss_bytes": peak_rss_bytes(),
        "stderr": stderr,
    });
    if !ephemeral {
        report["out"] = json!(out_path.display().to_string());
    }
    let clean = matches!(&residue, Ok(s) if s.removed.is_empty());
    let orientation_kept = match (&removed, &residue) {
        (Ok(i), Ok(o)) => i.orientation == o.orientation,
        _ => false,
    };
    let failure = if !clean {
        Some("output still carries removable metadata")
    } else if report["probe"].get("error").is_some() {
        Some("output did not probe as an image")
    } else if pin.is_err() || pout.is_err() {
        Some("pixels could not be decoded for comparison")
    } else if pin != pout {
        Some("decoded pixels differ")
    } else if let Some(failure) = rotation.as_ref().and_then(Rotation::failure) {
        Some(failure)
    } else if !orientation_kept {
        Some("output lost the source's orientation")
    } else if !matches_library {
        Some("output differs from the library's strip of the input")
    } else if frames_identical == Some(false) {
        Some("animation frames differ")
    } else {
        None
    };
    if let Some(failure) = failure {
        report["ok"] = json!(false);
        report["error"] = json!(failure);
        return Err(CtlError::from_value(1, report));
    }
    emit(&report, opts.pretty);
    Ok(())
}

fn removed_value(removed: &[pipeline::Removed]) -> Value {
    removed
        .iter()
        .map(|r| json!({ "name": r.name, "count": r.count, "bytes": r.bytes }))
        .collect()
}

/// The largest per-sample difference a correct rotation shows against
/// the input decoded and then turned upright. The coefficients are
/// exact, but the decoder's rounding is not symmetric: a transposed
/// block runs its IDCT passes in the other order, and fancy chroma
/// upsampling rounds left and right edges differently. Measured: at
/// most 3, over 4:4:4, 4:2:2, 4:2:0 and grey, baseline and progressive,
/// all seven orientations. A wrong rotation differs by 150 and more.
const ROTATION_MAX_DIFF: u8 = 8;

/// What `--auto-rotate` is proved by. Lossless: tagging the output
/// with the inverse orientation and rotating again (through the same
/// library) restores the input's stored pixels exactly, which only
/// identical coefficients can. Correct: the output is the input as
/// displayed, to within [`ROTATION_MAX_DIFF`].
struct Rotation {
    orientation: u8,
    round_trip: Result<bool, String>,
    max_diff: Result<u8, String>,
}

impl Rotation {
    fn value(&self) -> Value {
        fn or_error<T: Into<Value> + Copy>(r: &Result<T, String>) -> Value {
            match r {
                Ok(v) => (*v).into(),
                Err(e) => json!({ "error": e }),
            }
        }
        json!({
            "orientation": self.orientation,
            "round_trip_identical": or_error(&self.round_trip),
            "max_diff_displayed": or_error(&self.max_diff),
        })
    }

    fn failure(&self) -> Option<&'static str> {
        match (&self.round_trip, &self.max_diff) {
            (Err(_), _) | (_, Err(_)) => Some("rotation could not be checked"),
            (Ok(false), _) => {
                Some("rotation is not lossless: its inverse does not restore the input")
            }
            (_, Ok(d)) if *d > ROTATION_MAX_DIFF => {
                Some("rotated output is not the input as displayed")
            }
            _ => None,
        }
    }
}

fn rotation_proof(src: &[u8], out: &[u8], o: u8, keep_icc: bool) -> Rotation {
    // 6 and 8 are quarter turns in opposite directions; every other
    // orientation is its own inverse.
    let inverse = match o {
        6 => 8,
        8 => 6,
        o => o,
    };
    let round_trip = pipeline::strip(
        &with_orientation(out, inverse),
        &StripParams {
            icc: Some(keep_icc),
            auto_rotate: true,
        },
    )
    .map_err(|e| e.to_string())
    .and_then(|back| match back.rotated {
        Some(_) => Ok(pixels(&back.bytes)? == pixels(src)?),
        None => Err(format!(
            "the inverse was not applied: {}",
            back.not_rotated.unwrap_or_default()
        )),
    });
    let max_diff = decoded(src, true).and_then(|want| {
        let got = decoded(out, false)?;
        if got.len() != want.len() || got[..10] != want[..10] {
            return Err("rotated output has the wrong dimensions".into());
        }
        Ok(want
            .iter()
            .zip(&got)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0))
    });
    Rotation {
        orientation: o,
        round_trip,
        max_diff,
    }
}

/// The reason in the child's summary, `...; kept orientation 6, not
/// rotated: <why>)`: the child's own words, since its environment
/// decided. The last delimiter: the output path printed before it may
/// contain the same text.
fn child_not_rotated(stderr: &str) -> String {
    stderr
        .rsplit_once(", not rotated: ")
        .map(|(_, why)| {
            // The summary's own closing parenthesis, once: the reason
            // may end in one of its own.
            let why = why.trim_end();
            why.strip_suffix(')').unwrap_or(why).to_string()
        })
        .unwrap_or_default()
}

/// `jpeg` with an EXIF APP1 holding orientation `o` after the SOI.
fn with_orientation(jpeg: &[u8], o: u8) -> Vec<u8> {
    let mut tiff = [0u8; 26];
    tiff[..8].copy_from_slice(b"MM\0\x2a\0\0\0\x08");
    tiff[8..10].copy_from_slice(&1u16.to_be_bytes());
    tiff[10..12].copy_from_slice(&0x0112u16.to_be_bytes());
    tiff[12..14].copy_from_slice(&3u16.to_be_bytes());
    tiff[14..18].copy_from_slice(&1u32.to_be_bytes());
    tiff[18..20].copy_from_slice(&u16::from(o).to_be_bytes());
    let mut v = Vec::with_capacity(jpeg.len() + 40);
    v.extend_from_slice(&jpeg[..2.min(jpeg.len())]);
    v.extend_from_slice(&[0xFF, 0xE1]);
    v.extend_from_slice(&(2 + 6 + tiff.len() as u16).to_be_bytes());
    v.extend_from_slice(b"Exif\0\0");
    v.extend_from_slice(&tiff);
    v.extend_from_slice(jpeg.get(2..).unwrap_or_default());
    v
}

/// The decoded samples behind [`png_native`]'s colour type, depth and
/// size header, stored or `displayed` upright, through the same
/// lossless PNG as [`pixels`].
fn decoded(bytes: &[u8], displayed: bool) -> Result<Vec<u8>, String> {
    let p = Params {
        output: Some(ImageFormat::Png),
        auto_rotate: Some(displayed),
        icc: Some(false),
        png_quantize: Some(false),
        ..Default::default()
    };
    let (png, _) = pipeline::process(bytes, &p).map_err(|e| e.to_string())?;
    png_native(&png)
}

/// sha256 of the decoded pixels in stored orientation (lossless PNG,
/// no profile), so a strip that touched image data cannot match. A
/// PNG is decoded at its native depth: the pipeline strips 16-bit
/// samples to 8, which would hide a change in a low byte.
fn pixels(bytes: &[u8]) -> Result<String, String> {
    let p = Params {
        output: Some(ImageFormat::Png),
        auto_rotate: Some(false),
        icc: Some(false),
        png_quantize: Some(false),
        ..Default::default()
    };
    // The pipeline runs first even for a PNG: it enforces the source
    // limits (OXIMG_MAX_SRC_PIXELS) before the native decode allocates.
    let hash = pipeline::process(bytes, &p)
        .map(|(png, _)| sha256_hex(&png))
        .map_err(|e| e.to_string())?;
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return png_native(bytes).map(|px| sha256_hex(&px));
    }
    Ok(hash)
}

/// Every sample of a PNG's first image at its stored bit depth (palette
/// expanded, so a changed PLTE shows), behind its colour type and depth.
fn png_native(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut d = png::Decoder::new(std::io::Cursor::new(bytes));
    d.set_transformations(png::Transformations::EXPAND);
    let mut r = d.read_info().map_err(|e| e.to_string())?;
    let size = r.output_buffer_size().ok_or("PNG frame too large")?;
    let mut px = vec![0; size];
    let info = r.next_frame(&mut px).map_err(|e| e.to_string())?;
    px.truncate(info.buffer_size());
    let mut out = vec![info.color_type as u8, info.bit_depth as u8];
    out.extend_from_slice(&info.width.to_be_bytes());
    out.extend_from_slice(&info.height.to_be_bytes());
    out.extend_from_slice(&px);
    Ok(out)
}

/// For an animated WebP, the `ANIM` chunk and each frame's 16-byte
/// header and `ALPH`/`VP8 `/`VP8L` subchunks: everything that renders.
/// `pixels` sees only the first frame, and strip must copy these byte
/// for byte, so equality here proves the animation untouched. `None`
/// for anything else (or a container too broken to walk).
fn webp_frames(b: &[u8]) -> Option<Vec<&[u8]>> {
    fn chunks(b: &[u8]) -> Option<Vec<(&[u8], &[u8])>> {
        let mut v = Vec::new();
        let mut i = 0;
        while i < b.len() {
            let size = u32::from_le_bytes(b.get(i + 4..i + 8)?.try_into().ok()?) as usize;
            let data = b.get(i + 8..(i + 8).checked_add(size)?)?;
            v.push((&b[i..i + 4], data));
            i += 8 + size + (size & 1);
        }
        Some(v)
    }
    if b.len() < 12 || &b[..4] != b"RIFF" || &b[8..12] != b"WEBP" {
        return None;
    }
    // Bytes past the declared RIFF size are a trailer, not frames
    // (strip drops them), exactly as the library bounds its walk.
    let riff_end = (u32::from_le_bytes(b[4..8].try_into().ok()?) as usize).checked_add(8)?;
    let mut out = Vec::new();
    for (fourcc, data) in chunks(b.get(12..riff_end)?)? {
        match fourcc {
            b"ANIM" => out.push(data),
            b"ANMF" => {
                out.push(data.get(..16)?);
                for (sub, payload) in chunks(&data[16..])? {
                    if matches!(sub, b"ALPH" | b"VP8 " | b"VP8L") {
                        out.push(payload);
                    }
                }
            }
            _ => {}
        }
    }
    (!out.is_empty()).then_some(out)
}

fn strip_argv(input: &Path, out: &Path, icc: Option<&str>, auto_rotate: bool) -> Vec<String> {
    let mut v = vec![
        "oximg".into(),
        "strip".into(),
        input.display().to_string(),
        out.display().to_string(),
    ];
    if let Some(icc) = icc {
        v.push("--icc".into());
        v.push(icc.into());
    }
    if auto_rotate {
        v.push("--auto-rotate".into());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16-bit grey PNG, one pixel, sample `v`.
    fn gray16(v: u16) -> Vec<u8> {
        let mut out = Vec::new();
        let mut e = png::Encoder::new(&mut out, 1, 1);
        e.set_color(png::ColorType::Grayscale);
        e.set_depth(png::BitDepth::Sixteen);
        e.write_header()
            .unwrap()
            .write_image_data(&v.to_be_bytes())
            .unwrap();
        out
    }

    #[test]
    fn pixels_see_the_low_byte_of_a_16_bit_png() {
        assert_eq!(pixels(&gray16(0x1234)), pixels(&gray16(0x1234)));
        assert_ne!(pixels(&gray16(0x1234)), pixels(&gray16(0x1235)));
    }

    /// PR #167 review: the output path comes first in the summary and
    /// may contain the delimiter; the reason ends in its own ')'.
    #[test]
    fn the_refusal_reason_is_the_last_one_in_the_summary() {
        let stderr = "oximg: wrote /tmp/a, not rotated: b.jpg (7075 bytes, image/jpeg; \
                      kept orientation 5, not rotated: 200x150 is over \
                      OXIMG_MAX_SRC_PIXELS (1000))\n";
        assert_eq!(
            child_not_rotated(stderr),
            "200x150 is over OXIMG_MAX_SRC_PIXELS (1000)"
        );
        assert_eq!(child_not_rotated("oximg: wrote x (1 bytes)"), "");
    }

    #[test]
    fn frames_stop_at_the_riff_size() {
        let anim = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/animated.webp"
        ))
        .unwrap();
        let with_trailer = [&anim[..], b"SECRET"].concat();
        assert!(webp_frames(&anim).is_some());
        assert_eq!(webp_frames(&with_trailer), webp_frames(&anim));
    }

    #[test]
    fn argv_is_what_a_user_types() {
        assert_eq!(
            strip_argv(Path::new("in.jpg"), Path::new("o.jpg"), None, false),
            ["oximg", "strip", "in.jpg", "o.jpg"]
        );
        assert_eq!(
            strip_argv(
                Path::new("in.jpg"),
                Path::new("o.jpg"),
                Some("strip"),
                false
            ),
            ["oximg", "strip", "in.jpg", "o.jpg", "--icc", "strip"]
        );
        assert_eq!(
            strip_argv(Path::new("in.jpg"), Path::new("o.jpg"), None, true),
            ["oximg", "strip", "in.jpg", "o.jpg", "--auto-rotate"]
        );
    }
}
