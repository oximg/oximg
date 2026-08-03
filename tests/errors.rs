//! The library's typed-error contract, exercised through the public
//! API the way an embedder consumes it: `kind()` is the stable
//! classification, `{e}` the client-safe message, `{e:#}` the chain.
//! (The HTTP status mapping built on the same kinds is pinned by the
//! server suite; the classification table itself is unit-tested in
//! `src/pipeline/error.rs`.)

use oximg::pipeline::{self, ErrorKind, Params};

#[test]
fn missing_local_source_is_source_not_found() {
    let e = pipeline::process_path("/nonexistent/x.jpg".as_ref(), &Params::default())
        .expect_err("must fail");
    assert_eq!(e.kind(), ErrorKind::SourceNotFound);
    // The chain keeps the operator detail behind the boundary context.
    assert!(format!("{e:#}").starts_with("open source: "), "{e:#}");
}

#[test]
fn garbage_bytes_are_undecodable() {
    let e = pipeline::process(b"this is not an image at all", &Params::default())
        .expect_err("must fail");
    assert_eq!(e.kind(), ErrorKind::Undecodable);
    // Top-level Display is the safe-to-echo message, not the chain.
    assert!(!format!("{e}").contains(':'), "top-level only: {e}");
}

#[test]
fn probe_classifies_like_process() {
    let e = pipeline::probe(&[0u8; 4]).expect_err("must fail");
    assert_eq!(e.kind(), ErrorKind::Undecodable);
    let e = pipeline::probe(b"definitely not an image, but long enough").expect_err("must fail");
    assert_eq!(e.kind(), ErrorKind::Undecodable);
}

/// Errors convert into anyhow (std::error::Error + Send + Sync), so
/// embedders' `?` keeps working — and without duplicating the head of
/// the chain when anyhow re-renders it.
#[test]
fn converts_into_anyhow_without_duplicating_the_chain() {
    let run = || -> anyhow::Result<()> {
        pipeline::process_path("/nonexistent/x.jpg".as_ref(), &Params::default())?;
        Ok(())
    };
    let chain = format!("{:#}", run().unwrap_err());
    assert_eq!(chain.matches("open source").count(), 1, "{chain}");
}

/// An AVIF target without the avif feature is the client/caller's
/// request for a missing capability — Undecodable (HTTP 422), not an
/// internal fault.
#[cfg(not(feature = "avif"))]
#[test]
fn avif_without_feature_is_undecodable() {
    let fixture = format!("{}/tests/fixtures/photo.jpg", env!("CARGO_MANIFEST_DIR"));
    let p = Params {
        output: Some(pipeline::ImageFormat::Avif),
        ..Params::default()
    };
    let e = pipeline::process_path(fixture.as_ref(), &p).expect_err("must fail");
    assert_eq!(e.kind(), ErrorKind::Undecodable);
}

/// Regression, found by fuzzing `probe`: mozjpeg reports fatal libjpeg
/// errors by unwinding out of its C error handler, so a malformed
/// header arrived as a panic rather than an Err — a 500 (or, under
/// panic=abort, a dead process) for what is undecodable client input.
/// Both entry points must classify it as Undecodable and keep the
/// libjpeg reason in the chain.
#[test]
fn bogus_jpeg_header_is_undecodable_not_a_panic() {
    let fixture = format!(
        "{}/tests/fixtures/bogus_huffman.jpg",
        env!("CARGO_MANIFEST_DIR")
    );
    let bytes = std::fs::read(&fixture).expect("read fixture");

    let e = pipeline::probe(&bytes).expect_err("must fail");
    assert_eq!(e.kind(), ErrorKind::Undecodable);
    let chain = format!("{e:#}");
    assert!(chain.contains("Bogus Huffman"), "{chain}");

    // The resize path takes the same guard.
    let e = pipeline::process(&bytes, &Params::default()).expect_err("must fail");
    assert_eq!(e.kind(), ErrorKind::Undecodable);
}
