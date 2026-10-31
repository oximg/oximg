//! `strip`: shell out to `oximg strip`, then prove what it wrote: the
//! decoded pixels match the input's, and a second strip of the output
//! finds nothing left to remove.

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
    let argv = strip_argv(&a.input, &out_path, a.icc.as_deref());
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
    let read = |p: &Path| {
        std::fs::read(p).map_err(|e| CtlError::fail(format!("read {}: {e}", p.display()), None))
    };
    let src = read(&a.input)?;
    let bytes = read(&out_path)?;

    // The child resolved --icc against its own environment; mirror that
    // (flag, else the OXIMG_ICC it was given, else ours) so `removed`
    // describes the run that happened.
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
                != Some("0")
        }
    };
    let removed = pipeline::strip(
        &src,
        &StripParams {
            icc: Some(keep_icc),
        },
    );
    let residue = pipeline::strip(&bytes, &StripParams { icc: Some(true) });
    let (pin, pout) = (pixels(&src), pixels(&bytes));
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
        "orientation": removed.as_ref().ok().and_then(|s| s.orientation),
        "residue": match &residue {
            Ok(s) => removed_value(&s.removed),
            Err(e) => json!({ "error": e.to_string() }),
        },
        "pixels_identical": match (&pin, &pout) {
            (Ok(i), Ok(o)) => json!(i == o),
            (Err(e), _) | (_, Err(e)) => json!({ "error": e }),
        },
        "peak_rss_bytes": peak_rss_bytes(),
        "stderr": stderr,
    });
    if !ephemeral {
        report["out"] = json!(out_path.display().to_string());
    }
    let clean = matches!(&residue, Ok(s) if s.removed.is_empty());
    if report["probe"].get("error").is_some() || !clean || pin.is_ok() && pin != pout {
        report["ok"] = json!(false);
        report["error"] = json!(if !clean {
            "output still carries removable metadata"
        } else if report["probe"].get("error").is_some() {
            "output did not probe as an image"
        } else {
            "decoded pixels differ"
        });
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

/// sha256 of the decoded pixels in stored orientation (lossless PNG,
/// no profile), so a strip that touched image data cannot match.
fn pixels(bytes: &[u8]) -> Result<String, String> {
    let p = Params {
        output: Some(ImageFormat::Png),
        auto_rotate: Some(false),
        icc: Some(false),
        ..Default::default()
    };
    pipeline::process(bytes, &p)
        .map(|(png, _)| sha256_hex(&png))
        .map_err(|e| e.to_string())
}

fn strip_argv(input: &Path, out: &Path, icc: Option<&str>) -> Vec<String> {
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
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_is_what_a_user_types() {
        assert_eq!(
            strip_argv(Path::new("in.jpg"), Path::new("o.jpg"), None),
            ["oximg", "strip", "in.jpg", "o.jpg"]
        );
        assert_eq!(
            strip_argv(Path::new("in.jpg"), Path::new("o.jpg"), Some("strip")),
            ["oximg", "strip", "in.jpg", "o.jpg", "--icc", "strip"]
        );
    }
}
