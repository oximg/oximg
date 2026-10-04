//! The license notices that ship with every artifact (#59).

/// The IJG license asks that the documentation of a binary-only
/// distribution say this sentence. The bundle is the document that
/// ships with every artifact.
#[test]
fn bundle_carries_the_ijg_attribution() {
    let bundle = include_str!("../THIRD-PARTY-LICENSES.md");
    assert!(
        bundle
            .contains("This software is based in part on the work of the Independent JPEG Group."),
        "THIRD-PARTY-LICENSES.md lost the IJG attribution sentence (about.hbs)"
    );
}
