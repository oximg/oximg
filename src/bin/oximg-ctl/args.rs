//! argv → global `Opts` plus one `Cmd`. Every malformed flag is a usage
//! error (exit 2) before anything spawns.

use crate::error::CtlError;
use oximg::pipeline::ImageFormat;
use std::path::PathBuf;

/// Global flags, shared by every command.
#[derive(Clone)]
pub struct Opts {
    pub bin: Option<PathBuf>,
    pub images_dir: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub pretty: bool,
    pub dry_run: bool,
}

pub enum Cmd {
    Serve(ServeArgs),
    Get(GetArgs),
    Probe(ProbeArgs),
    Resize(ResizeArgs),
    Strip(StripArgs),
    Sign(SignArgs),
    Matrix(MatrixArgs),
    Parity(ParityArgs),
}

pub struct ServeArgs {
    pub port: Option<u16>,
}

pub struct GetArgs {
    pub path: String,
    pub base: Option<String>,
    pub write: Option<PathBuf>,
    pub accept: Option<String>,
    pub expect: Option<u16>,
    pub timeout_secs: u64,
}

pub struct ProbeArgs {
    pub file: PathBuf,
}

pub struct ResizeArgs {
    pub input: PathBuf,
    pub w: String,
    pub h: String,
    pub out: Option<PathBuf>,
    pub format: Option<String>,
    pub quality: Option<String>,
    pub preset: Option<String>,
    /// `--icc keep|strip`, passed through unvalidated: the CLI's refusal
    /// of a bad value is part of what gets proved.
    pub icc: Option<String>,
    /// `Some(false)` is `--no-auto-rotate`, `Some(true)` `--auto-rotate`.
    pub auto_rotate: Option<bool>,
}

pub struct StripArgs {
    pub input: PathBuf,
    pub out: Option<PathBuf>,
    pub icc: Option<String>,
}

pub struct SignArgs {
    pub path: String,
    pub key: Option<String>,
    pub salt: Option<String>,
}

pub struct MatrixArgs {
    pub sources: Vec<String>,
    pub boxes: Vec<(u32, u32)>,
    pub formats: Vec<String>,
    pub negatives: bool,
    pub base: Option<String>,
    pub timeout_secs: u64,
}

pub struct ParityArgs {
    /// The second build's `oximg`; the first is `--bin` (or the sibling).
    pub against: PathBuf,
    pub sources: Vec<String>,
    pub boxes: Vec<(u32, u32)>,
    pub formats: Vec<String>,
    pub timeout_secs: u64,
}

pub fn parse_args(args: &[String]) -> Result<(Opts, Cmd), CtlError> {
    let mut bin = None;
    let mut images_dir = None;
    let mut env = Vec::new();
    let mut pretty = false;
    let mut dry_run = false;
    let mut rest: &[String] = args;

    while let Some(a) = rest.first() {
        match a.as_str() {
            "--bin" => {
                bin = Some(PathBuf::from(need_val("--bin", rest)?));
                rest = &rest[2..];
            }
            "--images-dir" => {
                images_dir = Some(PathBuf::from(need_val("--images-dir", rest)?));
                rest = &rest[2..];
            }
            "--env" => {
                let v = need_val("--env", rest)?;
                let (k, val) = v
                    .split_once('=')
                    .ok_or_else(|| CtlError::usage(format!("--env needs KEY=VAL, got {v:?}")))?;
                if k.eq_ignore_ascii_case("PORT") {
                    return Err(CtlError::usage(
                        "--env PORT is reserved; use --port (auto-spawn uses PORT=0)",
                    ));
                }
                if k.eq_ignore_ascii_case("IMAGES_DIR") {
                    return Err(CtlError::usage(
                        "--env IMAGES_DIR is reserved; use --images-dir",
                    ));
                }
                env.push((canonical_env_key(k), val.to_string()));
                rest = &rest[2..];
            }
            "--pretty" => {
                pretty = true;
                rest = &rest[1..];
            }
            "--dry-run" => {
                dry_run = true;
                rest = &rest[1..];
            }
            _ => break,
        }
    }

    let Some((cmd, tail)) = rest.split_first() else {
        return Err(CtlError::usage("missing command"));
    };
    let cmd = match cmd.as_str() {
        "serve" => Cmd::Serve(parse_serve(tail)?),
        "get" => Cmd::Get(parse_get(tail)?),
        "probe" => Cmd::Probe(parse_probe(tail)?),
        "resize" => Cmd::Resize(parse_resize(tail)?),
        "strip" => Cmd::Strip(parse_strip(tail)?),
        "sign" => Cmd::Sign(parse_sign(tail)?),
        "matrix" => Cmd::Matrix(parse_matrix(tail)?),
        "parity" => Cmd::Parity(parse_parity(tail)?),
        other => {
            return Err(CtlError::usage(format!(
                "unknown command {other:?} (serve|get|probe|resize|strip|sign|matrix|parity)"
            )));
        }
    };
    let opts = Opts {
        bin,
        images_dir,
        env,
        pretty,
        dry_run,
    };
    Ok((opts, cmd))
}

fn need_val<'a>(flag: &str, rest: &'a [String]) -> Result<&'a str, CtlError> {
    rest.get(1)
        .map(String::as_str)
        .ok_or_else(|| CtlError::usage(format!("{flag} needs a value")))
}

/// Unix env keys are case-sensitive. Fold the knobs we ourselves set
/// so `--env oximg_bind=::1` actually reaches the child.
fn canonical_env_key(k: &str) -> String {
    for name in [
        "OXIMG_BIND",
        "OXIMG_WORKERS",
        "OXIMG_KEY",
        "OXIMG_SALT",
        "OXIMG_SOURCE_BASE_URL",
        "OXIMG_AUTO_ROTATE",
    ] {
        if k.eq_ignore_ascii_case(name) {
            return name.to_string();
        }
    }
    k.to_string()
}

fn parse_serve(args: &[String]) -> Result<ServeArgs, CtlError> {
    let mut port = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => {
                let v = it
                    .next()
                    .ok_or_else(|| CtlError::usage("--port needs a value"))?;
                port = Some(
                    v.parse()
                        .map_err(|_| CtlError::usage(format!("invalid --port {v:?}")))?,
                );
            }
            other => {
                return Err(CtlError::usage(format!("serve: unknown option {other:?}")));
            }
        }
    }
    Ok(ServeArgs { port })
}

fn parse_get(args: &[String]) -> Result<GetArgs, CtlError> {
    let mut path = None;
    let mut base = None;
    let mut write = None;
    let mut accept = None;
    let mut expect = None;
    let mut timeout_secs = 60u64;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--base" => {
                base = Some(
                    it.next()
                        .ok_or_else(|| CtlError::usage("--base needs a value"))?
                        .clone(),
                );
            }
            "--write" => {
                write = Some(PathBuf::from(
                    it.next()
                        .ok_or_else(|| CtlError::usage("--write needs a value"))?,
                ));
            }
            "--accept" => {
                accept = Some(
                    it.next()
                        .ok_or_else(|| CtlError::usage("--accept needs a value"))?
                        .clone(),
                );
            }
            "--expect" => {
                let v = it
                    .next()
                    .ok_or_else(|| CtlError::usage("--expect needs a value"))?;
                expect = Some(
                    v.parse()
                        .map_err(|_| CtlError::usage(format!("invalid --expect {v:?}")))?,
                );
            }
            "--timeout-secs" => {
                let v = it
                    .next()
                    .ok_or_else(|| CtlError::usage("--timeout-secs needs a value"))?;
                timeout_secs = v
                    .parse()
                    .map_err(|_| CtlError::usage(format!("invalid --timeout-secs {v:?}")))?;
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(CtlError::usage(format!("get: unknown option {flag:?}")));
            }
            p => {
                if path.is_some() {
                    return Err(CtlError::usage("get takes one path"));
                }
                path = Some(p.to_string());
            }
        }
    }
    let path = path.ok_or_else(|| CtlError::usage("usage: oximg-ctl get <path>"))?;
    Ok(GetArgs {
        path,
        base,
        write,
        accept,
        expect,
        timeout_secs,
    })
}

fn parse_probe(args: &[String]) -> Result<ProbeArgs, CtlError> {
    let mut file = None;
    for a in args {
        if a.starts_with('-') && a.len() > 1 {
            return Err(CtlError::usage(format!("probe: unknown option {a:?}")));
        }
        if file.is_some() {
            return Err(CtlError::usage("probe takes one file"));
        }
        file = Some(PathBuf::from(a));
    }
    let file = file.ok_or_else(|| CtlError::usage("usage: oximg-ctl probe <file>"))?;
    Ok(ProbeArgs { file })
}

fn parse_strip(args: &[String]) -> Result<StripArgs, CtlError> {
    let mut input = None;
    let mut out = None;
    let mut icc = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => {
                out = Some(PathBuf::from(
                    it.next()
                        .ok_or_else(|| CtlError::usage("--out needs a value"))?,
                ));
            }
            "--icc" => {
                icc = Some(
                    it.next()
                        .ok_or_else(|| CtlError::usage("--icc needs a value"))?
                        .clone(),
                );
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(CtlError::usage(format!("strip: unknown option {flag:?}")));
            }
            p if input.is_none() => input = Some(PathBuf::from(p)),
            _ => return Err(CtlError::usage("strip takes one input")),
        }
    }
    let input = input.ok_or_else(|| {
        CtlError::usage("usage: oximg-ctl strip <in> [--out PATH] [--icc keep|strip]")
    })?;
    Ok(StripArgs { input, out, icc })
}

fn parse_resize(args: &[String]) -> Result<ResizeArgs, CtlError> {
    let mut positional = Vec::new();
    let mut out = None;
    let mut format = None;
    let mut quality = None;
    let mut preset = None;
    let mut icc = None;
    let mut auto_rotate = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => {
                out = Some(PathBuf::from(
                    it.next()
                        .ok_or_else(|| CtlError::usage("--out needs a value"))?,
                ));
            }
            "--icc" => {
                icc = Some(
                    it.next()
                        .ok_or_else(|| CtlError::usage("--icc needs a value"))?
                        .clone(),
                );
            }
            "--auto-rotate" => auto_rotate = Some(true),
            "--no-auto-rotate" => auto_rotate = Some(false),
            "-f" | "--format" => {
                format = Some(
                    it.next()
                        .ok_or_else(|| CtlError::usage("--format needs a value"))?
                        .clone(),
                );
            }
            "-q" | "--quality" => {
                quality = Some(
                    it.next()
                        .ok_or_else(|| CtlError::usage("--quality needs a value"))?
                        .clone(),
                );
            }
            "--preset" => {
                preset = Some(
                    it.next()
                        .ok_or_else(|| CtlError::usage("--preset needs a value"))?
                        .clone(),
                );
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(CtlError::usage(format!("resize: unknown option {flag:?}")));
            }
            p => positional.push(p.to_string()),
        }
    }
    let [input, w, h] = positional.as_slice() else {
        return Err(CtlError::usage(
            "usage: oximg-ctl resize <in> <max_w> <max_h> [--out PATH] [-f fmt] [-q N] \
             [--preset P] [--icc keep|strip] [--[no-]auto-rotate]",
        ));
    };
    Ok(ResizeArgs {
        input: PathBuf::from(input),
        w: w.clone(),
        h: h.clone(),
        out,
        format,
        quality,
        preset,
        icc,
        auto_rotate,
    })
}

fn parse_sign(args: &[String]) -> Result<SignArgs, CtlError> {
    let mut path = None;
    let mut key = None;
    let mut salt = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--key" => {
                key = Some(
                    it.next()
                        .ok_or_else(|| CtlError::usage("--key needs a value"))?
                        .clone(),
                );
            }
            "--salt" => {
                salt = Some(
                    it.next()
                        .ok_or_else(|| CtlError::usage("--salt needs a value"))?
                        .clone(),
                );
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(CtlError::usage(format!("sign: unknown option {flag:?}")));
            }
            p => {
                if path.is_some() {
                    return Err(CtlError::usage("sign takes one path"));
                }
                path = Some(p.to_string());
            }
        }
    }
    let path = path.ok_or_else(|| CtlError::usage("usage: oximg-ctl sign <path>"))?;
    Ok(SignArgs { path, key, salt })
}

fn parse_matrix(args: &[String]) -> Result<MatrixArgs, CtlError> {
    let mut sources = Vec::new();
    let mut boxes = Vec::new();
    let mut formats = Vec::new();
    let mut negatives = true;
    let mut base = None;
    let mut timeout_secs = 60u64;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--source" => {
                sources.push(
                    it.next()
                        .ok_or_else(|| CtlError::usage("--source needs a value"))?
                        .clone(),
                );
            }
            "--box" => {
                let v = it
                    .next()
                    .ok_or_else(|| CtlError::usage("--box needs WxH"))?;
                boxes.push(parse_box(v)?);
            }
            "--format" => {
                let v = it
                    .next()
                    .ok_or_else(|| CtlError::usage("--format needs a value"))?;
                formats.push(parse_format(v)?);
            }
            "--no-negatives" => negatives = false,
            "--base" => {
                base = Some(
                    it.next()
                        .ok_or_else(|| CtlError::usage("--base needs a value"))?
                        .clone(),
                );
            }
            "--timeout-secs" => {
                let v = it
                    .next()
                    .ok_or_else(|| CtlError::usage("--timeout-secs needs a value"))?;
                timeout_secs = v
                    .parse()
                    .map_err(|_| CtlError::usage(format!("invalid --timeout-secs {v:?}")))?;
            }
            other => {
                return Err(CtlError::usage(format!("matrix: unknown option {other:?}")));
            }
        }
    }
    Ok(MatrixArgs {
        sources,
        boxes,
        formats,
        negatives,
        base,
        timeout_secs,
    })
}

fn parse_parity(args: &[String]) -> Result<ParityArgs, CtlError> {
    let mut against = None;
    let mut sources = Vec::new();
    let mut boxes = Vec::new();
    let mut formats = Vec::new();
    let mut timeout_secs = 60u64;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .ok_or_else(|| CtlError::usage(format!("{a} needs a value")))
        };
        match a.as_str() {
            "--against" => against = Some(PathBuf::from(val()?)),
            "--source" => sources.push(val()?.clone()),
            "--box" => boxes.push(parse_box(val()?)?),
            "--format" => formats.push(parse_format(val()?)?),
            "--timeout-secs" => {
                let v = val()?;
                timeout_secs = v
                    .parse()
                    .map_err(|_| CtlError::usage(format!("invalid --timeout-secs {v:?}")))?;
            }
            other => {
                return Err(CtlError::usage(format!("parity: unknown option {other:?}")));
            }
        }
    }
    let against = against.ok_or_else(|| {
        CtlError::usage("usage: oximg-ctl parity --against PATH (the second build's oximg)")
    })?;
    Ok(ParityArgs {
        against,
        sources,
        boxes,
        formats,
        timeout_secs,
    })
}

/// A `--format` cell token: `source` (the bare path) or an `@{fmt}`
/// output the server encodes.
fn parse_format(v: &str) -> Result<String, CtlError> {
    match v {
        "" | "source" => Ok(v.to_string()),
        tok if ImageFormat::from_token(tok).is_some() => Ok(v.to_string()),
        other => Err(CtlError::usage(format!(
            "unknown --format {other:?} (source|{})",
            ImageFormat::output_token_hint()
        ))),
    }
}

fn parse_box(v: &str) -> Result<(u32, u32), CtlError> {
    let (w, h) = v
        .split_once('x')
        .ok_or_else(|| CtlError::usage(format!("--box needs WxH, got {v:?}")))?;
    let w: u32 = w
        .parse()
        .map_err(|_| CtlError::usage(format!("invalid --box {v:?}")))?;
    let h: u32 = h
        .parse()
        .map_err(|_| CtlError::usage(format!("invalid --box {v:?}")))?;
    if w == 0 && h == 0 {
        return Err(CtlError::usage(
            "--box 0x0 is invalid (HTTP 400); leave one axis 0 to unconstrained",
        ));
    }
    if w > 8192 || h > 8192 {
        return Err(CtlError::usage(
            "--box axes are 1–8192 (0 allowed on one axis only)",
        ));
    }
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<(Opts, Cmd), CtlError> {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        parse_args(&argv)
    }

    /// A usage error's message; panics unless it is one (exit 2 carries
    /// the --help hint).
    fn usage(argv: &[&str]) -> String {
        let Err(e) = parse(argv) else {
            panic!("{argv:?} should be a usage error");
        };
        assert_eq!(e.json["hint"], "oximg-ctl --help", "{}", e.json);
        e.json["error"].as_str().unwrap().to_string()
    }

    #[test]
    fn global_flags_come_before_the_command() {
        let (opts, cmd) = parse(&[
            "--pretty",
            "--dry-run",
            "--bin",
            "/x/oximg",
            "--images-dir",
            "/f",
            "get",
            "/a",
        ])
        .unwrap();
        assert!(opts.pretty && opts.dry_run);
        assert_eq!(opts.bin.as_deref(), Some(std::path::Path::new("/x/oximg")));
        assert_eq!(opts.images_dir.as_deref(), Some(std::path::Path::new("/f")));
        assert!(
            matches!(cmd, Cmd::Get(GetArgs { ref path, timeout_secs: 60, .. }) if path == "/a")
        );
    }

    #[test]
    fn env_folds_the_managed_keys_and_keeps_others() {
        let (opts, _) = parse(&[
            "--env",
            "oximg_bind=::1",
            "--env",
            "Custom_Key=a=b",
            "serve",
        ])
        .unwrap();
        assert_eq!(
            opts.env,
            vec![
                ("OXIMG_BIND".to_string(), "::1".to_string()),
                ("Custom_Key".to_string(), "a=b".to_string()),
            ]
        );
    }

    #[test]
    fn env_rejects_reserved_and_malformed_keys() {
        assert!(usage(&["--env", "port=1", "serve"]).contains("PORT is reserved"));
        assert!(usage(&["--env", "Images_Dir=/x", "serve"]).contains("IMAGES_DIR is reserved"));
        assert!(usage(&["--env", "NOEQUALS", "serve"]).contains("KEY=VAL"));
        assert!(usage(&["--env"]).contains("--env needs a value"));
    }

    #[test]
    fn commands_must_exist() {
        assert_eq!(usage(&["--pretty"]), "missing command");
        assert!(usage(&["frobnicate"]).contains("unknown command"));
    }

    #[test]
    fn get_takes_exactly_one_path() {
        assert!(usage(&["get"]).contains("usage: oximg-ctl get"));
        assert!(usage(&["get", "/a", "/b"]).contains("one path"));
        assert!(usage(&["get", "--expect", "ok", "/a"]).contains("invalid --expect"));
        assert!(usage(&["get", "--nope", "/a"]).contains("unknown option"));
    }

    #[test]
    fn resize_takes_three_positionals() {
        assert!(usage(&["resize", "a.jpg", "1"]).contains("usage: oximg-ctl resize"));
        let (_, cmd) = parse(&["resize", "a.jpg", "10", "0", "-f", "webp", "-q", "70"]).unwrap();
        let Cmd::Resize(r) = cmd else {
            panic!("not resize")
        };
        assert_eq!((r.w.as_str(), r.h.as_str()), ("10", "0"));
        assert_eq!(
            (r.format.as_deref(), r.quality.as_deref()),
            (Some("webp"), Some("70"))
        );
    }

    #[test]
    fn strip_takes_one_input() {
        assert!(usage(&["strip"]).contains("usage: oximg-ctl strip"));
        assert!(usage(&["strip", "a.jpg", "b.jpg"]).contains("one input"));
        let (_, cmd) = parse(&["strip", "a.jpg", "--icc", "strip"]).unwrap();
        let Cmd::Strip(s) = cmd else {
            panic!("not strip")
        };
        assert_eq!(s.icc.as_deref(), Some("strip"));
    }

    #[test]
    fn matrix_validates_boxes_and_formats() {
        let (_, cmd) = parse(&[
            "matrix",
            "--box",
            "750x0",
            "--box",
            "0x64",
            "--format",
            "source",
            "--format",
            "webp",
            "--no-negatives",
        ])
        .unwrap();
        let Cmd::Matrix(m) = cmd else {
            panic!("not matrix")
        };
        assert_eq!(m.boxes, vec![(750, 0), (0, 64)]);
        assert_eq!(m.formats, vec!["source", "webp"]);
        assert!(!m.negatives);
        assert!(usage(&["matrix", "--format", "gif"]).contains("unknown --format"));
    }

    #[test]
    fn boxes_follow_the_http_grammar() {
        assert_eq!(parse_box("100x100").unwrap(), (100, 100));
        assert_eq!(parse_box("8192x0").unwrap(), (8192, 0));
        for bad in ["0x0", "8193x1", "1x8193", "100", "ax1", "1x", "-1x5"] {
            assert!(parse_box(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn serve_port_must_be_a_number() {
        let (_, cmd) = parse(&["serve", "--port", "8080"]).unwrap();
        assert!(matches!(cmd, Cmd::Serve(ServeArgs { port: Some(8080) })));
        assert!(usage(&["serve", "--port", "http"]).contains("invalid --port"));
    }

    #[test]
    fn parity_needs_a_second_build() {
        assert!(usage(&["parity"]).contains("--against PATH"));
        assert!(usage(&["parity", "--against"]).contains("--against needs a value"));
        assert!(
            usage(&["parity", "--against", "b", "--format", "gif"]).contains("unknown --format")
        );
        assert!(usage(&["parity", "--against", "b", "--box", "0x0"]).contains("0x0 is invalid"));
        assert!(usage(&["parity", "--against", "b", "--no-negatives"]).contains("unknown option"));
        let (_, cmd) = parse(&[
            "parity",
            "--against",
            "/b/oximg",
            "--source",
            "photo.jpg",
            "--box",
            "64x0",
            "--format",
            "png",
        ])
        .unwrap();
        let Cmd::Parity(p) = cmd else {
            panic!("not parity")
        };
        assert_eq!(p.against, std::path::Path::new("/b/oximg"));
        assert_eq!(
            (p.sources, p.boxes, p.formats),
            (
                vec!["photo.jpg".to_string()],
                vec![(64, 0)],
                vec!["png".to_string()]
            )
        );
        assert_eq!(p.timeout_secs, 60);
    }
}
