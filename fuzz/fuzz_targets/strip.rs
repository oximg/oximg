//! `strip` walks JPEG markers (entropy data included), PNG chunks and
//! RIFF chunks by hand, on untrusted uploads. Beyond not panicking, its
//! output must be a fixed point: stripping it again removes nothing
//! and changes no byte. The second pass never rotates: with
//! `auto_rotate` the first one may have turned the image upright, or
//! kept its tag for a reason the strip itself removes (a large
//! trailer counted against the memory cap), so only the metadata is a
//! fixed point.
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
                let again = StripParams {
                    icc: Some(icc),
                    auto_rotate: false,
                };
                let twice = strip(&once.bytes, &again).expect("stripped output re-strips");
                assert!(twice.removed.is_empty(), "residue {:?}", twice.removed);
                assert_eq!(twice.bytes, once.bytes);
                assert_eq!(twice.orientation, once.orientation);
            }
        }
    }
});
