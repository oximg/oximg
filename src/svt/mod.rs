//! Minimal SVT-AV1 encoder FFI, pregenerated with bindgen against the
//! SVT-AV1 v4.1.0 public headers (layout tests included, so a mismatched
//! library version fails `cargo test --features avif` instead of
//! corrupting memory). Only compiled with the `avif` feature; the library
//! to link comes from `SVT_AV1_LIB_DIR` or the system default paths — use
//! a Release build of SVT-AV1 (some distro packages ship debug builds).
//!
//! ABI compatibility: verified against both v4.1.0 and master at
//! d3c4cb3947a8bfed0aa5a2be996b37bb117fa1bd (the revision pinned in the
//! Dockerfile) — EbSvtAv1EncConfiguration has identical size (664) and
//! identical offsets for every field this crate writes; master added
//! fields only inside existing reserved padding.
#![allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    unsafe_op_in_unsafe_fn,
    clippy::all
)]

pub mod bindings;

#[cfg(test)]
mod tests {
    use super::bindings as b;

    #[test]
    fn encoder_handle_lifecycle() {
        unsafe {
            let mut handle: *mut b::EbComponentType = std::ptr::null_mut();
            let mut config: b::EbSvtAv1EncConfiguration = std::mem::zeroed();
            let err = b::svt_av1_enc_init_handle(&mut handle, &mut config);
            assert_eq!(err, b::EbErrorType::EB_ErrorNone, "init_handle");
            assert!(!handle.is_null());
            config.source_width = 64;
            config.source_height = 64;
            config.enc_mode = 8;
            config.encoder_bit_depth = 10;
            // Mirror the production still-image setup: tune=IQ requires
            // the all-intra prediction structure (enforced since SVT-AV1
            // master post-4.1), which force_key_frames + avif select.
            config.force_key_frames = true;
            config.avif = true;
            let tune = std::ffi::CString::new("tune").unwrap();
            let three = std::ffi::CString::new("3").unwrap();
            assert_eq!(
                b::svt_av1_enc_parse_parameter(&mut config, tune.as_ptr(), three.as_ptr()),
                b::EbErrorType::EB_ErrorNone,
                "tune=3"
            );
            let err = b::svt_av1_enc_set_parameter(handle, &mut config);
            assert_eq!(err, b::EbErrorType::EB_ErrorNone, "set_parameter");
            let err = b::svt_av1_enc_init(handle);
            assert_eq!(err, b::EbErrorType::EB_ErrorNone, "enc_init");
            let err = b::svt_av1_enc_deinit(handle);
            assert_eq!(err, b::EbErrorType::EB_ErrorNone, "deinit");
            let err = b::svt_av1_enc_deinit_handle(handle);
            assert_eq!(err, b::EbErrorType::EB_ErrorNone, "deinit_handle");
        }
    }
}
