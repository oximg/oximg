//! `strip` walks JPEG markers (entropy data included), PNG chunks and
//! RIFF chunks by hand, on untrusted uploads. Beyond not panicking, its
//! output must be a fixed point: stripping it again removes nothing
//! and changes no byte. With `auto_rotate`, the same holds: a rotated
//! output is upright, and a kept one is kept again.
#![no_main]
use libfuzzer_sys::fuzz_target;
use oximg::pipeline::{StripParams, strip};

fuzz_target!(|data: &[u8]| {
    for icc in [true, false] {
        for auto_rotate in [false, true] {
            let p = StripParams {
                icc: Some(icc),
                auto_rotate,
            };
            if let Ok(once) = strip(data, &p) {
                let twice = strip(&once.bytes, &p).expect("stripped output re-strips");
                assert!(twice.removed.is_empty(), "residue {:?}", twice.removed);
                assert_eq!(twice.bytes, once.bytes);
                assert_eq!(twice.rotated, None, "rotated twice");
            }
        }
    }
});
