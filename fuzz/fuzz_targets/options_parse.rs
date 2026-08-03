//! The Cloudflare-style option-list grammar: client-controlled input
//! parsed before anything else on the options route. Included by path —
//! the module lives in the server binary, which fuzz targets cannot
//! link against.
#![no_main]
use libfuzzer_sys::fuzz_target;

// dead_code: the fuzz target only calls parse(); unexpected_cfgs: the
// included module tests the crate's avif feature, which this crate
// doesn't declare (it evaluates false — the grammar is still covered).
#[path = "../../src/options.rs"]
#[allow(dead_code, unexpected_cfgs)]
mod options;

fuzz_target!(|data: &str| {
    let _ = options::parse(data);
});
