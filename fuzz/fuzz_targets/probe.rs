//! `probe` is the whole header-parsing surface in one call: format
//! sniff, per-format dimension parsing, and the metadata/ICC scanner —
//! all hand-written, all fed by untrusted bytes in production. It never
//! decodes pixels, so executions stay fast.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = oximg::pipeline::probe(data);
});
