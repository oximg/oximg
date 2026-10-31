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
//!
//! Layout: `args` parses argv into `Opts` + `Cmd`; `cmd/` holds one
//! module per command; `spawn` owns the child server, `http` the client
//! and its report, `report` the probe/hash fields every command shares,
//! `urlpath` the percent-encoding the server's `Path` extractor mirrors.

mod args;
mod cmd;
mod error;
mod http;
mod report;
mod spawn;
mod urlpath;

use args::{Cmd, parse_args};
use oximg::pipeline::ImageFormat;

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
    let result = parse_args(&args).and_then(|(opts, cmd)| match &cmd {
        Cmd::Serve(a) => cmd::serve::run(&opts, a),
        Cmd::Get(a) => cmd::get::run(&opts, a),
        Cmd::Probe(a) => cmd::probe::run(&opts, a),
        Cmd::Resize(a) => cmd::resize::run(&opts, a),
        Cmd::Sign(a) => cmd::sign::run(&opts, a),
        Cmd::Matrix(a) => cmd::matrix::run(&opts, a),
        Cmd::Parity(a) => cmd::parity::run(&opts, a),
    });
    if let Err(e) = result {
        e.with_pretty(pretty).exit();
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
  probe <file>              Header probe (format, stored size, animation, JPEG frame)
  resize <in> <w> <h>       Shell out to `oximg resize`; probe the output
  sign <path>               imgproxy-style HMAC over the decoded path
  matrix                    Walk fixture × box × format cells + negatives
  parity --against PATH     Same cells on two builds; bodies must match

Global:
  --bin PATH                oximg binary (else sibling, OXIMG_BIN, or PATH)
  --images-dir DIR          IMAGES_DIR for a spawned server
                            (default: this crate's tests/fixtures)
  --env KEY=VAL             Extra env for a spawned server (repeatable;
                            PORT and IMAGES_DIR are reserved)
  --pretty                  Indent JSON
  --dry-run                 Print the plan; do not spawn or write
  --help, --version

serve:
  --port N                  Listen port (default: OS-assigned 0)

get:
  --base URL                Use an already-running server (no spawn)
  --accept VALUE            Accept request header
  --write PATH              Save the response body
  --expect N                Fail unless the status is N
  --timeout-secs N          Request timeout (default 60)

resize:
  --out PATH                Output file (default: a temp file)
  -f, --format FMT          {}
  -q, --quality N           JPEG quality 1-100
  --preset P                jpegli | fast | small
  JSON adds `peak_rss_bytes` (the child's peak RSS; null off Unix) and,
  when oximg printed an estimate (`--env OXIMG_LOG_DECODED_BYTES_ABOVE=1`),
  `decoded_bytes`: {{format, total, terms: {{staged, ...}}}}.

sign:
  --key HEX                 OXIMG_KEY (else the env)
  --salt HEX                OXIMG_SALT (else the env)

matrix:
  --source FILE             Fixture filename (repeatable; default: a
                            small committed set under tests/fixtures)
  --box WxH                 e.g. 100x100 or 750x0 (repeatable)
  --format TOKEN            source | {}
                            (repeatable; default: source and webp)
  --no-negatives            Skip the 400/404 cells
  --base URL                Drive an existing server

parity:
  --against PATH            The second build's oximg (the first is --bin)
  --source FILE             Fixture filename (repeatable; default: every
                            file in the images dir that probes as an image)
  --box WxH                 Repeatable (default: 100x100, 333x0, 64x64)
  --format TOKEN            As for matrix (default: source and webp)
  A cell compares only when both builds answer 200; the same error on
  both is skipped. Exit 1 if any cell differs or none compared.

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
        env!("CARGO_PKG_VERSION"),
        ImageFormat::output_token_hint().replace('|', " | "),
        ImageFormat::output_token_hint().replace('|', " | "),
    );
}
