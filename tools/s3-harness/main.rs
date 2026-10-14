//! Repository test tool; uses the same transport as the S3 integration tests.
#[path = "../../tests/support/s3.rs"]
mod s3_harness;
mod scenarios;

use serde_json::json;
use std::path::PathBuf;

fn main() {
    if let Err(e) = run() {
        println!("{}", json!({"ok": false, "error": e.to_string()}));
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("--help" | "-h") => {
            println!(
                "s3-harness serve\ns3-harness check [--ctl PATH]\n\nserve: loopback S3 with test keys; prints endpoint/env JSON, waits for SIGINT/SIGTERM.\ncheck: drives oximg-ctl against every scenario; prints JSON, exits nonzero on failure.\nBuild oximg and oximg-ctl first. See tools/s3-harness/README.md for routes."
            );
        }
        Some("serve") if args.len() == 1 => {
            let server = scenarios::origin()?;
            let env: std::collections::BTreeMap<_, _> =
                s3_harness::s3_env(server.port()).into_iter().collect();
            println!(
                "{}",
                json!({"ok": true, "endpoint": format!("http://127.0.0.1:{}", server.port()), "env": env, "source": "s3://test-bucket"})
            );
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(async {
                    #[cfg(unix)]
                    {
                        let mut terminate = tokio::signal::unix::signal(
                            tokio::signal::unix::SignalKind::terminate(),
                        )?;
                        tokio::select! {
                            r = tokio::signal::ctrl_c() => r?,
                            _ = terminate.recv() => {},
                        }
                    }
                    #[cfg(not(unix))]
                    tokio::signal::ctrl_c().await?;
                    Ok::<_, std::io::Error>(())
                })?;
        }
        Some("check") => {
            let ctl = match args.as_slice() {
                [_] => std::env::current_exe()?
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join(if cfg!(windows) {
                        "oximg-ctl.exe"
                    } else {
                        "oximg-ctl"
                    }),
                [_, flag, path] if flag == "--ctl" => PathBuf::from(path),
                _ => anyhow::bail!("usage: s3-harness check [--ctl PATH]"),
            };
            let report = scenarios::check(&ctl)?;
            println!("{report}");
            if report["ok"] != true {
                std::process::exit(1);
            }
        }
        _ => anyhow::bail!("usage: s3-harness serve | check [--ctl PATH]"),
    }
    Ok(())
}
