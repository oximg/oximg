//! Control plane for the `oximg` binary: spawn the real server, drive
//! HTTP/CLI/library surfaces, and print one JSON object per command.
//!
//! This is a lever for agents (and operators), not a second user-facing
//! product. It never reimplements the pipeline — every HTTP cell hits a
//! spawned `oximg` process, `resize` shells out to `oximg resize`, and
//! `probe`/`sign` use the same library/HMAC scheme the server does.
//!
//!   oximg-ctl get /resize/100/100/photo.jpg
//!   oximg-ctl probe tests/fixtures/photo.jpg
//!   oximg-ctl matrix
//!
//! Exit 0 on success, 1 when a command ran and failed its proof, 2 for
//! usage errors. stdout is always one JSON value (except `--help` and
//! `--version`).

use hmac::Mac;
use hmac::digest::KeyInit;
use oximg::pipeline::{self, Animation};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Unix: forward SIGINT/SIGTERM to the spawned child. `signal(2)`
/// replaces the default disposition, so the wrapper stays alive to
/// `wait()` while the server drains. Forced kill is Drop / timeout only.
#[cfg(unix)]
mod unix_child {
    use std::sync::Once;
    use std::sync::atomic::{AtomicI32, Ordering};

    static CHILD: AtomicI32 = AtomicI32::new(0);

    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
        fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
    }

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    extern "C" fn forward(sig: i32) {
        let pid = CHILD.load(Ordering::SeqCst);
        if pid > 0 {
            unsafe {
                kill(pid, sig);
            }
        }
    }

    pub fn install() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| unsafe {
            signal(SIGINT, forward);
            signal(SIGTERM, forward);
        });
    }

    pub fn set_pid(pid: u32) {
        CHILD.store(pid as i32, Ordering::SeqCst);
    }

    pub fn clear() {
        CHILD.store(0, Ordering::SeqCst);
    }
}

const SPAWN_TIMEOUT: Duration = Duration::from_secs(15);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("oximg-ctl {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let pretty = args.iter().any(|a| a == "--pretty");
    match parse_args(&args) {
        Ok(opts) => {
            if let Err(e) = run(opts) {
                e.with_pretty(pretty).exit();
            }
        }
        Err(e) => e.with_pretty(pretty).exit(),
    }
}

fn print_help() {
    print!(
        "\
oximg-ctl {}
JSON control plane for the oximg binary (server, CLI, and pipeline).

Usage:
  oximg-ctl [global] <command> [args]

Commands:
  serve                     Spawn oximg, print {{port,pid,base}}, wait
  get <path>                HTTP GET; auto-spawns unless --base is set
  probe <file>              Header probe (format, stored size, animation)
  resize <in> <w> <h>       Shell out to `oximg resize`; probe the output
  sign <path>               imgproxy-style HMAC over the decoded path
  matrix                    Walk fixture × box × format cells + negatives

Global:
  --bin PATH                oximg binary (else sibling, OXIMG_BIN, or PATH)
  --images-dir DIR          IMAGES_DIR for a spawned server
                            (default: this crate's tests/fixtures)
  --env KEY=VAL             Extra env for a spawned server (repeatable;
                            PORT and IMAGES_DIR are reserved)
  --pretty                  Indent JSON
  --dry-run                 Print the plan; do not spawn or write
  --help, --version

get:
  --base URL                Use an already-running server (no spawn)
  --accept VALUE            Accept request header
  --write PATH              Save the response body
  --expect N                Fail unless the status is N
  --timeout-secs N          Request timeout (default 60)

resize:
  --out PATH                Output file (default: a temp file)
  -f, --format FMT          jpg | png | webp | avif
  -q, --quality N           JPEG quality 1-100
  --preset P                jpegli | fast | small

sign:
  --key HEX                 OXIMG_KEY (else the env)
  --salt HEX                OXIMG_SALT (else the env)

matrix:
  --source FILE             Fixture filename (repeatable; default: a
                            small committed set under tests/fixtures)
  --box WxH                 e.g. 100x100 or 750x0 (repeatable)
  --format TOKEN            source | jpg | png | webp | avif
                            (repeatable; default: source and webp)
  --no-negatives            Skip the 400/404 cells
  --base URL                Drive an existing server

Spawned servers use PORT=0 and, unless already set, OXIMG_WORKERS=1
so a control-plane loop does not size itself to the host. stderr from
oximg is forwarded. stdout is one JSON object (`--help` and `--version`
are plain text). `serve` prints the ready record then waits; a later
child failure is a stderr line and exit 1, not a second JSON object.

Examples:
  oximg-ctl get /resize/100/100/photo.jpg
  oximg-ctl get /resize/100/100/photo.jpg@webp --write /tmp/out.webp
  oximg-ctl --env OXIMG_OPTIONS_PREFIX=/image get /image/width=100/photo.jpg
  oximg-ctl sign /resize/100/100/photo.jpg --key deadbeef... --salt cafebabe...
  oximg-ctl matrix --box 100x100 --source photo.jpg
",
        env!("CARGO_PKG_VERSION")
    );
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

struct CtlError {
    exit: i32,
    json: Value,
    pretty: bool,
}

impl CtlError {
    fn usage(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        Self {
            exit: 2,
            json: json!({
                "ok": false,
                "error": msg,
                "hint": "oximg-ctl --help",
            }),
            pretty: false,
        }
    }

    fn fail(msg: impl Into<String>, hint: Option<&str>) -> Self {
        let mut v = json!({ "ok": false, "error": msg.into() });
        if let Some(h) = hint {
            v["hint"] = json!(h);
        }
        Self {
            exit: 1,
            json: v,
            pretty: false,
        }
    }

    fn from_value(exit: i32, json: Value) -> Self {
        Self {
            exit,
            json,
            pretty: false,
        }
    }

    fn with_pretty(mut self, pretty: bool) -> Self {
        self.pretty = pretty;
        self
    }

    fn exit(self) -> ! {
        emit(&self.json, self.pretty);
        std::process::exit(self.exit);
    }
}

fn emit(v: &Value, pretty: bool) {
    if pretty {
        println!(
            "{}",
            serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
        );
    } else {
        println!("{v}");
    }
}

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

struct Opts {
    bin: Option<PathBuf>,
    images_dir: Option<PathBuf>,
    env: Vec<(String, String)>,
    pretty: bool,
    dry_run: bool,
    cmd: Cmd,
}

enum Cmd {
    Serve {
        port: Option<u16>,
    },
    Get {
        path: String,
        base: Option<String>,
        write: Option<PathBuf>,
        accept: Option<String>,
        expect: Option<u16>,
        timeout_secs: u64,
    },
    Probe {
        file: PathBuf,
    },
    Resize {
        input: PathBuf,
        w: String,
        h: String,
        out: Option<PathBuf>,
        format: Option<String>,
        quality: Option<String>,
        preset: Option<String>,
    },
    Sign {
        path: String,
        key: Option<String>,
        salt: Option<String>,
    },
    Matrix {
        sources: Vec<String>,
        boxes: Vec<(u32, u32)>,
        formats: Vec<String>,
        negatives: bool,
        base: Option<String>,
        timeout_secs: u64,
    },
}

fn parse_args(args: &[String]) -> Result<Opts, CtlError> {
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
        "serve" => parse_serve(tail)?,
        "get" => parse_get(tail)?,
        "probe" => parse_probe(tail)?,
        "resize" => parse_resize(tail)?,
        "sign" => parse_sign(tail)?,
        "matrix" => parse_matrix(tail)?,
        other => {
            return Err(CtlError::usage(format!(
                "unknown command {other:?} (serve|get|probe|resize|sign|matrix)"
            )));
        }
    };
    Ok(Opts {
        bin,
        images_dir,
        env,
        pretty,
        dry_run,
        cmd,
    })
}

fn need_val<'a>(flag: &str, rest: &'a [String]) -> Result<&'a str, CtlError> {
    rest.get(1)
        .map(String::as_str)
        .ok_or_else(|| CtlError::usage(format!("{flag} needs a value")))
}

fn parse_serve(args: &[String]) -> Result<Cmd, CtlError> {
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
    Ok(Cmd::Serve { port })
}

fn parse_get(args: &[String]) -> Result<Cmd, CtlError> {
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
    Ok(Cmd::Get {
        path,
        base,
        write,
        accept,
        expect,
        timeout_secs,
    })
}

fn parse_probe(args: &[String]) -> Result<Cmd, CtlError> {
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
    Ok(Cmd::Probe { file })
}

fn parse_resize(args: &[String]) -> Result<Cmd, CtlError> {
    let mut positional = Vec::new();
    let mut out = None;
    let mut format = None;
    let mut quality = None;
    let mut preset = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => {
                out = Some(PathBuf::from(
                    it.next()
                        .ok_or_else(|| CtlError::usage("--out needs a value"))?,
                ));
            }
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
            "usage: oximg-ctl resize <in> <max_w> <max_h> [--out PATH]",
        ));
    };
    Ok(Cmd::Resize {
        input: PathBuf::from(input),
        w: w.clone(),
        h: h.clone(),
        out,
        format,
        quality,
        preset,
    })
}

fn parse_sign(args: &[String]) -> Result<Cmd, CtlError> {
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
    Ok(Cmd::Sign { path, key, salt })
}

fn parse_matrix(args: &[String]) -> Result<Cmd, CtlError> {
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
                match v.as_str() {
                    "" | "source" | "jpg" | "jpeg" | "png" | "webp" | "avif" => {
                        formats.push(v.clone());
                    }
                    other => {
                        return Err(CtlError::usage(format!(
                            "unknown --format {other:?} (source|jpg|png|webp|avif)"
                        )));
                    }
                }
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
    Ok(Cmd::Matrix {
        sources,
        boxes,
        formats,
        negatives,
        base,
        timeout_secs,
    })
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
    Ok((w, h))
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

fn run(opts: Opts) -> Result<(), CtlError> {
    match &opts.cmd {
        Cmd::Serve { port } => cmd_serve(&opts, *port),
        Cmd::Get {
            path,
            base,
            write,
            accept,
            expect,
            timeout_secs,
        } => cmd_get(
            &opts,
            path,
            base.as_deref(),
            write.as_deref(),
            accept.as_deref(),
            *expect,
            *timeout_secs,
        ),
        Cmd::Probe { file } => cmd_probe(&opts, file),
        Cmd::Resize { .. } => cmd_resize(&opts),
        Cmd::Sign { path, key, salt } => cmd_sign(&opts, path, key.as_deref(), salt.as_deref()),
        Cmd::Matrix {
            sources,
            boxes,
            formats,
            negatives,
            base,
            timeout_secs,
        } => cmd_matrix(
            &opts,
            sources.clone(),
            boxes.clone(),
            formats.clone(),
            *negatives,
            base.clone(),
            *timeout_secs,
        ),
    }
}

// ---------------------------------------------------------------------------
// serve / spawn
// ---------------------------------------------------------------------------

fn default_images_dir() -> PathBuf {
    let baked = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    if baked.is_dir() {
        baked
    } else {
        PathBuf::from("tests/fixtures")
    }
}

fn resolve_bin(explicit: Option<&Path>) -> Result<PathBuf, CtlError> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Ok(p) = std::env::var("OXIMG_BIN")
        && !p.is_empty()
    {
        return Ok(PathBuf::from(p));
    }
    if let Ok(me) = std::env::current_exe()
        && let Some(dir) = me.parent()
    {
        let sib = dir.join(if cfg!(windows) { "oximg.exe" } else { "oximg" });
        if sib.is_file() {
            return Ok(sib);
        }
    }
    Ok(PathBuf::from("oximg"))
}

fn images_dir(opts: &Opts) -> PathBuf {
    opts.images_dir.clone().unwrap_or_else(default_images_dir)
}

fn env_named(opts: &Opts, name: &str) -> bool {
    opts.env.iter().any(|(k, _)| k == name)
}

fn env_value<'a>(opts: &'a Opts, name: &str) -> Option<&'a str> {
    opts.env
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Unix env keys are case-sensitive. Fold the knobs we ourselves set
/// so `--env oximg_bind=::1` actually reaches the child.
fn canonical_env_key(k: &str) -> String {
    for name in ["OXIMG_BIND", "OXIMG_WORKERS"] {
        if k.eq_ignore_ascii_case(name) {
            return name.to_string();
        }
    }
    k.to_string()
}

fn inherited_unset(name: &str) -> bool {
    match std::env::var(name) {
        Err(_) => true,
        Ok(v) => v.trim().is_empty(),
    }
}

fn effective_bind(opts: &Opts, loopback: bool) -> IpAddr {
    if let Some(v) = env_value(opts, "OXIMG_BIND")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && let Ok(ip) = v.parse()
    {
        return ip;
    }
    if loopback {
        IpAddr::from([127, 0, 0, 1])
    } else {
        IpAddr::from([0, 0, 0, 0])
    }
}

fn http_host(bind: IpAddr) -> String {
    match bind {
        IpAddr::V4(v) if v.is_unspecified() => "127.0.0.1".into(),
        IpAddr::V6(v) if v.is_unspecified() => "[::1]".into(),
        IpAddr::V6(v) => format!("[{v}]"),
        IpAddr::V4(v) => v.to_string(),
    }
}

struct Spawned {
    child: Child,
    port: u16,
    host: String,
    bin: PathBuf,
    images_dir: PathBuf,
    kill_on_drop: bool,
}

impl Drop for Spawned {
    fn drop(&mut self) {
        #[cfg(unix)]
        unix_child::clear();
        if self.kill_on_drop {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn spawn_server(
    opts: &Opts,
    port: Option<u16>,
    loopback: bool,
    http: &Http,
) -> Result<Spawned, CtlError> {
    let bin = resolve_bin(opts.bin.as_deref())?;
    let images_dir = images_dir(opts);
    if !images_dir.is_dir() {
        return Err(CtlError::fail(
            format!("images dir {} does not exist", images_dir.display()),
            Some("pass --images-dir or run from the oximg checkout"),
        ));
    }
    let mut cmd = Command::new(&bin);
    cmd.env(
        "PORT",
        port.map(|p| p.to_string()).unwrap_or_else(|| "0".into()),
    )
    .env("IMAGES_DIR", &images_dir)
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    let bind = effective_bind(opts, loopback);
    // A local proof must not publish IMAGES_DIR on every interface.
    if loopback && !env_named(opts, "OXIMG_BIND") {
        cmd.env("OXIMG_BIND", "127.0.0.1");
    }
    // A control-plane loop should not size itself to the host; leave
    // the operator in charge if they already set a count. Empty inherited
    // values are unset — the server treats them that way too.
    if inherited_unset("OXIMG_WORKERS") && !env_named(opts, "OXIMG_WORKERS") {
        cmd.env("OXIMG_WORKERS", "1");
    }
    for (k, v) in &opts.env {
        cmd.env(k, v);
    }
    let host = http_host(bind);
    #[cfg(unix)]
    unix_child::install();
    let deadline = Instant::now() + SPAWN_TIMEOUT;
    let mut child = cmd.spawn().map_err(|e| {
        CtlError::fail(
            format!("spawn {}: {e}", bin.display()),
            Some("build the binary first: cargo build --release"),
        )
    })?;
    #[cfg(unix)]
    unix_child::set_pid(child.id());
    let stderr = child.stderr.take().expect("stderr piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        let mut boot = String::new();
        let mut port = None;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    boot.push_str(&line);
                    if let Some(rest) = line.strip_prefix("oximg listening on :") {
                        port = rest.split_whitespace().next().and_then(|p| p.parse().ok());
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        std::thread::spawn(move || {
            let mut sink = std::io::stderr();
            let _ = std::io::copy(&mut reader, &mut sink);
        });
        let _ = tx.send((port, boot));
    });
    let remaining = deadline.saturating_duration_since(Instant::now());
    let (port_found, boot) = match rx.recv_timeout(remaining) {
        Ok(v) => v,
        Err(_) => {
            #[cfg(unix)]
            unix_child::clear();
            let _ = child.kill();
            let _ = child.wait();
            return Err(CtlError::fail(
                "server did not print a listening line within 15s",
                Some("check --bin points at oximg, and that cmake/nasm are installed"),
            ));
        }
    };
    let Some(port) = port_found else {
        #[cfg(unix)]
        unix_child::clear();
        let status = child.wait().ok();
        let hint = if boot.is_empty() {
            "check --bin points at oximg, and that cmake/nasm are installed"
        } else {
            "a set-but-invalid OXIMG_* is the usual cause"
        };
        let mut err = CtlError::fail(
            format!("server exited before becoming healthy: {status:?}"),
            Some(hint),
        );
        if !boot.is_empty() {
            err.json["stderr"] = json!(boot.trim());
        }
        return Err(err);
    };
    // The copy thread only sees lines after the listening line; keep
    // the promised "stderr is forwarded" contract for boot warnings.
    eprint!("{boot}");
    let mut spawned = Spawned {
        child,
        port,
        host: host.clone(),
        bin,
        images_dir,
        kill_on_drop: true,
    };
    loop {
        if let Ok(res) = http.get(
            &format!("http://{host}:{port}/health"),
            None,
            Duration::from_secs(2),
        ) && res.status == 200
        {
            return Ok(spawned);
        }
        if let Ok(Some(status)) = spawned.child.try_wait() {
            return Err(CtlError::fail(
                format!("server exited before /health: {status}"),
                None,
            ));
        }
        if Instant::now() > deadline {
            return Err(CtlError::fail(
                "server did not become healthy within 15s",
                Some("the listening line was printed but /health did not answer 200"),
            ));
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn cmd_serve(opts: &Opts, port: Option<u16>) -> Result<(), CtlError> {
    if opts.dry_run {
        emit(
            &json!({
                "ok": true,
                "dry_run": true,
                "bin": resolve_bin(opts.bin.as_deref())?.display().to_string(),
                "images_dir": images_dir(opts).display().to_string(),
                "port": port.unwrap_or(0),
                "env": opts
                    .env
                    .iter()
                    .map(|(k, v)| json!({ (k): redact_env_value(k, v) }))
                    .collect::<Vec<_>>(),
            }),
            opts.pretty,
        );
        return Ok(());
    }
    let http = Http::new().map_err(|e| CtlError::fail(e, None))?;
    let mut spawned = spawn_server(opts, port, false, &http)?;
    emit(
        &json!({
            "ok": true,
            "pid": spawned.child.id(),
            "port": spawned.port,
            "base": format!("http://{}:{}", spawned.host, spawned.port),
            "bin": spawned.bin.display().to_string(),
            "images_dir": spawned.images_dir.display().to_string(),
        }),
        opts.pretty,
    );
    let _ = std::io::stdout().flush();
    // Ready record is already on stdout. A later child failure must not
    // emit a second JSON object. Keep kill_on_drop until the child is
    // reaped so SIGINT/SIGTERM cannot leave an orphan listener.
    match wait_served_child(&mut spawned) {
        Ok(status) if status.success() => {
            spawned.kill_on_drop = false;
            Ok(())
        }
        Ok(status) => {
            spawned.kill_on_drop = false;
            eprintln!("oximg-ctl: server exited {status}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("oximg-ctl: wait on server: {e}");
            std::process::exit(1);
        }
    }
}

fn wait_served_child(spawned: &mut Spawned) -> std::io::Result<ExitStatus> {
    // Unix: SIGINT/SIGTERM are forwarded to the child by unix_child::forward
    // so oximg can drain. wait() returns when that finishes. Drop still
    // SIGKILLs if we unwind before then.
    spawned.child.wait()
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

struct HttpResult {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
    ms: f64,
    url: String,
}

struct Http {
    rt: tokio::runtime::Runtime,
    client: reqwest::Client,
}

impl Http {
    fn new() -> Result<Self, String> {
        Ok(Self {
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("tokio runtime: {e}"))?,
            client: reqwest::Client::builder()
                .build()
                .map_err(|e| format!("http client: {e}"))?,
        })
    }

    fn get(
        &self,
        url: &str,
        accept: Option<&str>,
        timeout: Duration,
    ) -> Result<HttpResult, String> {
        let t0 = Instant::now();
        self.rt.block_on(async {
            let mut req = self.client.get(url).timeout(timeout);
            if let Some(a) = accept {
                req = req.header("accept", a);
            }
            let resp = req.send().await.map_err(|e| format!("GET {url}: {e}"))?;
            let status = resp.status().as_u16();
            let mut headers = HashMap::new();
            for (k, v) in resp.headers() {
                if let Ok(val) = v.to_str() {
                    headers.insert(k.as_str().to_ascii_lowercase(), val.to_string());
                }
            }
            let body = resp
                .bytes()
                .await
                .map_err(|e| format!("read body: {e}"))?
                .to_vec();
            Ok(HttpResult {
                status,
                headers,
                body,
                ms: t0.elapsed().as_secs_f64() * 1e3,
                url: url.to_string(),
            })
        })
    }
}

fn normalize_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn get_report(res: &HttpResult, write: Option<&Path>) -> Result<Value, CtlError> {
    if let Some(p) = write {
        std::fs::write(p, &res.body)
            .map_err(|e| CtlError::fail(format!("write {}: {e}", p.display()), None))?;
    }
    let ct = res.headers.get("content-type").cloned().unwrap_or_default();
    let mut v = json!({
        "ok": true,
        "status": res.status,
        "url": res.url,
        "bytes": res.body.len(),
        "sha256": sha256_hex(&res.body),
        "ms": (res.ms * 10.0).round() / 10.0,
        "content_type": ct,
        "headers": {
            "content-type": res.headers.get("content-type"),
            "vary": res.headers.get("vary"),
            "cache-control": res.headers.get("cache-control"),
            "allow": res.headers.get("allow"),
        },
    });
    if let Some(p) = write {
        v["body_path"] = json!(p.display().to_string());
    }
    if res.status == 200 && ct.starts_with("image/") {
        v["probe"] = probe_value(&res.body);
    } else if !res.body.is_empty() && !ct.starts_with("image/") {
        let text = String::from_utf8_lossy(&res.body);
        let clipped: String = text.chars().take(512).collect();
        v["body_text"] = json!(clipped);
    }
    Ok(v)
}

/// A 200 image response is proved only when the body probes. Non-image
/// 200s (e.g. `/health`) and non-200s are observations, not image proofs.
fn image_200_proved(res: &HttpResult, report: &Value) -> bool {
    if res.status != 200 {
        return true;
    }
    let ct = res
        .headers
        .get("content-type")
        .map(String::as_str)
        .unwrap_or("");
    if !ct.starts_with("image/") {
        return true;
    }
    match report.get("probe") {
        Some(p) => p.get("error").is_none() && p.get("width").is_some(),
        None => false,
    }
}

fn content_type_matches_token(ct: &str, token: &str) -> bool {
    let want = match token {
        "webp" => "image/webp",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "avif" => "image/avif",
        _ => return false,
    };
    ct == want || ct.starts_with(&format!("{want};"))
}

/// Matrix 200 cells must be images that probe. Unlike `image_200_proved`,
/// a text 200 (e.g. `/health`) is not a pass.
fn matrix_200_proved(res: &HttpResult, report: &Value) -> bool {
    if res.status != 200 {
        return false;
    }
    let ct = res
        .headers
        .get("content-type")
        .map(String::as_str)
        .unwrap_or("");
    if !ct.starts_with("image/") {
        return false;
    }
    match report.get("probe") {
        Some(p) => p.get("error").is_none() && p.get("width").is_some(),
        None => false,
    }
}

fn redact_env_value<'a>(key: &str, value: &'a str) -> &'a str {
    let u = key.to_ascii_uppercase();
    if ["KEY", "SALT", "SECRET", "TOKEN", "PASSWORD"]
        .iter()
        .any(|needle| u.contains(needle))
    {
        "<redacted>"
    } else {
        value
    }
}

fn cmd_get(
    opts: &Opts,
    path: &str,
    base: Option<&str>,
    write: Option<&Path>,
    accept: Option<&str>,
    expect: Option<u16>,
    timeout_secs: u64,
) -> Result<(), CtlError> {
    let path = normalize_path(path);
    if opts.dry_run {
        emit(
            &json!({
                "ok": true,
                "dry_run": true,
                "path": path,
                "base": base,
                "accept": accept,
                "expect": expect,
            }),
            opts.pretty,
        );
        return Ok(());
    }
    let http = Http::new().map_err(|e| CtlError::fail(e, None))?;
    let spawned = match base {
        Some(_) => None,
        None => Some(spawn_server(opts, None, true, &http)?),
    };
    let base = match (base, spawned.as_ref()) {
        (Some(b), _) => b.trim_end_matches('/').to_string(),
        (None, Some(s)) => format!("http://{}:{}", s.host, s.port),
        (None, None) => unreachable!(),
    };
    let url = format!("{base}{path}");
    let res = http
        .get(&url, accept, Duration::from_secs(timeout_secs))
        .map_err(|e| {
            CtlError::fail(
                e,
                Some("is the server up? pass --base or let get auto-spawn"),
            )
        })?;
    let mut report = get_report(&res, write)?;
    if let Some(want) = expect
        && res.status != want
    {
        report["ok"] = json!(false);
        report["error"] = json!(format!("expected status {want}, got {}", res.status));
        return Err(CtlError::from_value(1, report));
    }
    if !image_200_proved(&res, &report) {
        report["ok"] = json!(false);
        report["error"] = json!("response body did not probe as an image");
        return Err(CtlError::from_value(1, report));
    }
    emit(&report, opts.pretty);
    Ok(())
}

// ---------------------------------------------------------------------------
// probe / resize / sign
// ---------------------------------------------------------------------------

fn probe_value(bytes: &[u8]) -> Value {
    match pipeline::probe(bytes) {
        Ok((fmt, w, h)) => {
            let mut v = json!({
                "content_type": fmt.content_type(),
                "width": w,
                "height": h,
                "stored_pixels": w as u64 * h as u64,
            });
            match pipeline::probe_animation(bytes) {
                Ok(Some(Animation {
                    frames,
                    duration_ms,
                    loop_count,
                })) => {
                    v["animation"] = json!({
                        "frames": frames,
                        "duration_ms": duration_ms,
                        "loop_count": loop_count,
                    });
                }
                Ok(None) => {}
                Err(e) => v["animation_error"] = json!(e.to_string()),
            }
            v
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

fn cmd_probe(opts: &Opts, file: &Path) -> Result<(), CtlError> {
    if opts.dry_run {
        emit(
            &json!({"ok": true, "dry_run": true, "file": file.display().to_string()}),
            opts.pretty,
        );
        return Ok(());
    }
    let bytes = std::fs::read(file).map_err(|e| {
        CtlError::fail(
            format!("read {}: {e}", file.display()),
            Some("path is relative to the current working directory"),
        )
    })?;
    let mut probe = probe_value(&bytes);
    if probe.get("error").is_some() {
        let mut v = json!({
            "ok": false,
            "file": file.display().to_string(),
            "bytes": bytes.len(),
            "sha256": sha256_hex(&bytes),
            "probe": probe,
        });
        v["error"] = probe["error"].take();
        return Err(CtlError::from_value(1, v));
    }
    emit(
        &json!({
            "ok": true,
            "file": file.display().to_string(),
            "bytes": bytes.len(),
            "sha256": sha256_hex(&bytes),
            "probe": probe,
        }),
        opts.pretty,
    );
    Ok(())
}

fn cmd_resize(opts: &Opts) -> Result<(), CtlError> {
    let Cmd::Resize {
        input,
        w,
        h,
        out,
        format,
        quality,
        preset,
    } = &opts.cmd
    else {
        unreachable!("cmd_resize");
    };
    let format = format.as_deref();
    let quality = quality.as_deref();
    let preset = preset.as_deref();
    let bin = resolve_bin(opts.bin.as_deref())?;
    let ephemeral = out.is_none();
    let tmp;
    let out_path: &Path = match out {
        Some(p) => p,
        None => {
            tmp = std::env::temp_dir().join(format!(
                "oximg-ctl-{}-{}x{}.out",
                std::process::id(),
                w,
                h
            ));
            &tmp
        }
    };
    struct RemoveOnDrop(Option<PathBuf>);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            if let Some(p) = self.0.take() {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    let _ephemeral = RemoveOnDrop(ephemeral.then(|| out_path.to_path_buf()));
    if opts.dry_run {
        emit(
            &json!({
                "ok": true,
                "dry_run": true,
                "bin": bin.display().to_string(),
                "argv": resize_argv(input, w, h, out_path, format, quality, preset),
            }),
            opts.pretty,
        );
        return Ok(());
    }
    let argv = resize_argv(input, w, h, out_path, format, quality, preset);
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
        let msg = format!("oximg resize exited {code}: {stderr}");
        return Err(if code == 2 {
            CtlError::usage(msg)
        } else {
            CtlError::fail(msg, None)
        });
    }
    let bytes = std::fs::read(out_path)
        .map_err(|e| CtlError::fail(format!("read {}: {e}", out_path.display()), None))?;
    let mut report = json!({
        "ok": true,
        "bytes": bytes.len(),
        "sha256": sha256_hex(&bytes),
        "ms": (ms * 10.0).round() / 10.0,
        "probe": probe_value(&bytes),
        "stderr": stderr,
    });
    if !ephemeral {
        report["out"] = json!(out_path.display().to_string());
    }
    emit(&report, opts.pretty);
    Ok(())
}

fn resize_argv(
    input: &Path,
    w: &str,
    h: &str,
    out: &Path,
    format: Option<&str>,
    quality: Option<&str>,
    preset: Option<&str>,
) -> Vec<String> {
    let mut v = vec![
        "oximg".into(),
        "resize".into(),
        input.display().to_string(),
        w.into(),
        h.into(),
        out.display().to_string(),
    ];
    if let Some(f) = format {
        v.push("-f".into());
        v.push(f.into());
    }
    if let Some(q) = quality {
        v.push("-q".into());
        v.push(q.into());
    }
    if let Some(p) = preset {
        v.push("--preset".into());
        v.push(p.into());
    }
    v
}

fn cmd_sign(
    opts: &Opts,
    path: &str,
    key: Option<&str>,
    salt: Option<&str>,
) -> Result<(), CtlError> {
    let path = percent_decode_path(&normalize_path(path))?;
    let key_hex = key
        .map(str::to_string)
        .or_else(|| std::env::var("OXIMG_KEY").ok())
        .ok_or_else(|| CtlError::usage("sign needs --key HEX or OXIMG_KEY"))?;
    let salt_hex = salt
        .map(str::to_string)
        .or_else(|| std::env::var("OXIMG_SALT").ok())
        .ok_or_else(|| CtlError::usage("sign needs --salt HEX or OXIMG_SALT"))?;
    if opts.dry_run {
        emit(
            &json!({
                "ok": true,
                "dry_run": true,
                "path": path,
            }),
            opts.pretty,
        );
        return Ok(());
    }
    let key = decode_hex("key", &key_hex)?;
    let salt = decode_hex("salt", &salt_hex)?;
    let signature = sign_path(&key, &salt, &path)?;
    emit(
        &json!({
            "ok": true,
            "path": path,
            "signature": signature,
            "url": format!("/{signature}{}", percent_encode_path(&path)),
        }),
        opts.pretty,
    );
    Ok(())
}

fn decode_hex(name: &str, v: &str) -> Result<Vec<u8>, CtlError> {
    let v = v.trim();
    if v.is_empty() {
        return Err(CtlError::usage(format!(
            "{name} must not be empty (unset OXIMG_KEY/OXIMG_SALT means signing off)"
        )));
    }
    let raw = v.as_bytes();
    if !raw.len().is_multiple_of(2) {
        return Err(CtlError::usage(format!(
            "{name} is not valid hex (odd length)"
        )));
    }
    if !raw.iter().all(u8::is_ascii_hexdigit) {
        return Err(CtlError::usage(format!("{name} is not valid hex")));
    }
    raw.chunks(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("ascii hex"), 16)
                .map_err(|_| CtlError::usage(format!("{name} is not valid hex")))
        })
        .collect()
}

/// Percent-decode a URL path the way the server's `Path` extractor does
/// before HMAC verify. `+` is a literal (this is a path, not a query).
fn percent_decode_path(path: &str) -> Result<String, CtlError> {
    let b = path.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() {
                return Err(CtlError::usage(format!(
                    "malformed percent-escape in {path:?}"
                )));
            }
            let hi = hex_digit(b[i + 1])
                .ok_or_else(|| CtlError::usage(format!("malformed percent-escape in {path:?}")))?;
            let lo = hex_digit(b[i + 2])
                .ok_or_else(|| CtlError::usage(format!("malformed percent-escape in {path:?}")))?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out)
        .map_err(|_| CtlError::usage(format!("percent-decoded path is not valid UTF-8: {path:?}")))
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Encode a decoded path so it can be put back in a URL. HMAC still
/// covers the decoded form; `%` in a filename must round-trip as `%25`.
/// `/` stays a separator. Other bytes follow RFC 3986 `pchar`.
fn percent_encode_path(path: &str) -> String {
    path.split('/')
        .map(percent_encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn percent_encode_segment(seg: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::new();
    for &b in seg.as_bytes() {
        if is_pchar(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xf) as usize] as char);
        }
    }
    out
}

fn is_pchar(b: u8) -> bool {
    matches!(
        b,
        b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
            | b':'
            | b'@'
    )
}

/// The scheme `Signing::verify` in src/main.rs accepts: unpadded
/// base64url(HMAC-SHA256(key, salt || path)) over the percent-decoded
/// path. Vectors live in tests/server.rs and the Rails gem.
fn sign_path(key: &[u8], salt: &[u8], path: &str) -> Result<String, CtlError> {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key)
        .map_err(|_| CtlError::fail("HMAC key was empty", None))?;
    mac.update(salt);
    mac.update(path.as_bytes());
    Ok(base64url_nopad(&mac.finalize().into_bytes()))
}

fn base64url_nopad(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = u32::from_be_bytes([0, data[i], data[i + 1], data[i + 2]]);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(T[((n >> 6) & 63) as usize] as char);
        out.push(T[(n & 63) as usize] as char);
        i += 3;
    }
    match data.len() - i {
        1 => {
            let n = (data[i] as u32) << 16;
            out.push(T[((n >> 18) & 63) as usize] as char);
            out.push(T[((n >> 12) & 63) as usize] as char);
        }
        2 => {
            let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
            out.push(T[((n >> 18) & 63) as usize] as char);
            out.push(T[((n >> 12) & 63) as usize] as char);
            out.push(T[((n >> 6) & 63) as usize] as char);
        }
        _ => {}
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 0xf) as usize] as char);
    }
    s
}

// ---------------------------------------------------------------------------
// matrix
// ---------------------------------------------------------------------------

fn default_sources() -> Vec<String> {
    let mut v = vec![
        "photo.jpg".into(),
        "rgb.png".into(),
        "photo.webp".into(),
        "still.gif".into(),
        "anim.gif".into(),
    ];
    if cfg!(feature = "avif") {
        v.push("photo.avif".into());
    }
    v
}

fn default_formats() -> Vec<String> {
    vec!["".into(), "webp".into()]
}

fn cmd_matrix(
    opts: &Opts,
    sources: Vec<String>,
    boxes: Vec<(u32, u32)>,
    formats: Vec<String>,
    negatives: bool,
    base: Option<String>,
    timeout_secs: u64,
) -> Result<(), CtlError> {
    let sources = if sources.is_empty() {
        default_sources()
    } else {
        sources
    };
    let boxes = if boxes.is_empty() {
        vec![(100, 100)]
    } else {
        boxes
    };
    let formats = if formats.is_empty() {
        default_formats()
    } else {
        formats
    };

    struct Cell {
        path: String,
        expect: u16,
        kind: &'static str,
        format: Option<String>,
    }
    let mut plan: Vec<Cell> = Vec::new();
    for src in &sources {
        let src_enc = percent_encode_path(src);
        for (w, h) in &boxes {
            for fmt in &formats {
                let (path, format) = match fmt.as_str() {
                    "" | "source" => (format!("/resize/{w}/{h}/{src_enc}"), None),
                    token => (
                        format!("/resize/{w}/{h}/{src_enc}@{token}"),
                        Some(token.to_string()),
                    ),
                };
                plan.push(Cell {
                    path,
                    expect: 200,
                    kind: "cell",
                    format,
                });
            }
        }
    }
    if negatives {
        plan.push(Cell {
            path: "/resize/0/0/photo.jpg".into(),
            expect: 400,
            kind: "negative",
            format: None,
        });
        plan.push(Cell {
            path: "/resize/100/100/missing.jpg".into(),
            expect: 404,
            kind: "negative",
            format: None,
        });
        plan.push(Cell {
            path: "/resize/100/100/photo.jpg@gif".into(),
            expect: 400,
            kind: "negative",
            format: None,
        });
    }

    if opts.dry_run {
        emit(
            &json!({
                "ok": true,
                "dry_run": true,
                "cells": plan.iter().map(|c| json!({
                    "path": c.path,
                    "expect": c.expect,
                    "kind": c.kind,
                })).collect::<Vec<_>>(),
            }),
            opts.pretty,
        );
        return Ok(());
    }

    let http = Http::new().map_err(|e| CtlError::fail(e, None))?;
    let spawned = match base.as_deref() {
        Some(_) => None,
        None => Some(spawn_server(opts, None, true, &http)?),
    };
    let base = match (base.as_deref(), spawned.as_ref()) {
        (Some(b), _) => b.trim_end_matches('/').to_string(),
        (None, Some(s)) => format!("http://{}:{}", s.host, s.port),
        (None, None) => unreachable!(),
    };

    let t0 = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let mut cells = Vec::new();
    let mut failed = 0u32;
    for c in &plan {
        let url = format!("{base}{}", c.path);
        match http.get(&url, None, timeout) {
            Ok(res) => {
                let mut row = get_report(&res, None)?;
                let mut pass = res.status == c.expect;
                if pass && c.expect == 200 {
                    pass = matrix_200_proved(&res, &row);
                    if pass && let Some(token) = c.format.as_deref() {
                        let ct = res
                            .headers
                            .get("content-type")
                            .map(String::as_str)
                            .unwrap_or("");
                        pass = content_type_matches_token(ct, token);
                    }
                }
                if !pass {
                    failed += 1;
                }
                row["path"] = json!(c.path);
                row["expect"] = json!(c.expect);
                row["kind"] = json!(c.kind);
                row["pass"] = json!(pass);
                // Nested ok:true on a failing cell is confusing; the
                // top-level ok is the proof.
                row.as_object_mut().unwrap().remove("ok");
                cells.push(row);
            }
            Err(e) => {
                failed += 1;
                cells.push(json!({
                    "path": c.path,
                    "expect": c.expect,
                    "kind": c.kind,
                    "pass": false,
                    "error": e,
                }));
            }
        }
    }
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    let report = json!({
        "ok": failed == 0,
        "failed": failed,
        "total": cells.len(),
        "ms": (ms * 10.0).round() / 10.0,
        "base": base,
        "cells": cells,
    });
    if failed > 0 {
        return Err(CtlError::from_value(1, report));
    }
    emit(&report, opts.pretty);
    Ok(())
}
