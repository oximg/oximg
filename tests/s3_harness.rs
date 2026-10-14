//! Every standalone harness scenario also runs in cargo test, against both bins.
#![cfg(feature = "server")]

#[path = "support/s3.rs"]
mod s3_harness;
#[path = "../tools/s3-harness/scenarios.rs"]
mod scenarios;

#[test]
fn s3_control_plane_contract() {
    let oximg = std::path::Path::new(env!("CARGO_BIN_EXE_oximg"));
    let ctl = oximg.with_file_name(if cfg!(windows) {
        "oximg-ctl.exe"
    } else {
        "oximg-ctl"
    });
    let result = scenarios::check(&ctl).unwrap();
    assert_eq!(
        result["ok"],
        true,
        "{}",
        serde_json::to_string_pretty(&result).unwrap()
    );
    assert_eq!(result["passed"], result["total"]);
}
