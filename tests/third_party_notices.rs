//! The license notices that ship with every artifact (#59).
//!
//! cargo-about builds THIRD-PARTY-LICENSES.md from each crate's declared
//! license. It cannot see C code that a `-sys` crate vendors under
//! another license, and it cannot see libraries from outside Cargo. The
//! build scripts can: each native library they link is printed as
//! `cargo:rustc-link-lib=...` in `target/<profile>/build/<crate>-<hash>/output`.
//! `every_linked_library_has_a_notice` reads those lines and checks each
//! library against `LINKED`.
//!
//! When a new library shows up, read the crate's sources and find the
//! license file that covers it. Add a row here. For vendored code, also
//! pin the file in `about.toml` and regenerate THIRD-PARTY-LICENSES.md.
//!
//! What this test does not see:
//! - Code linked without a build script line: the Rust standard library
//!   and the C library. The release jobs and the Docker image ship their
//!   notices; docker.yml and the release workflow check for them.
//! - Libraries linked through `#[link]` in Rust code, through
//!   `cargo:rustc-link-arg`, or loaded at run time with `dlopen`.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// Where the license notice of a linked library comes from.
enum Notice {
    /// Statically linked from the crate's vendored sources. Each path is
    /// relative to the crate root and must be pinned in the crate's
    /// `about.toml` clarify, so its text lands in THIRD-PARTY-LICENSES.md,
    /// which every artifact ships.
    Vendored(&'static [&'static str]),
    /// oximg's own C code, covered by oximg's LICENSE.
    Own,
    /// A shared library the Docker image ships next to the binary. Its
    /// license files go to this directory in the image; docker.yml checks
    /// that they are there.
    Shipped(&'static str),
    /// A shared library the operating system provides. Its package
    /// carries its own notice.
    System,
    /// A runtime whose license drops the notice conditions for compiled
    /// code, whether it is linked statically or not.
    Exempt,
}

use Notice::*;

/// (crate, library name prefix, notice). A prefix, so a version inside
/// the name (`ring_core_0_17_14_`) does not need an update per release.
/// The rows cover every platform the release jobs build on. A row that
/// one platform does not link is not an error.
const LINKED: &[(&str, &str, Notice)] = &[
    (
        "mozjpeg-sys",
        "mozjpeg",
        Vendored(&[
            "LICENSE",
            "vendor/turbojpeg.h",
            "vendor/simd/nasm/jsimdext.inc",
        ]),
    ),
    ("jpegli-sys", "jpegli-static", Vendored(&["LICENSE"])),
    (
        "jpegli-sys",
        "hwy",
        Vendored(&["libjxl/third_party/highway/LICENSE"]),
    ),
    ("libwebp-sys", "webpsys", Vendored(&["vendor/COPYING"])),
    (
        "ring",
        "ring_core_",
        Vendored(&["LICENSE-BoringSSL", "LICENSE-other-bits"]),
    ),
    ("oximg", "oximg_linear_shrink", Own),
    ("oximg", "SvtAv1Enc", Shipped("/usr/share/doc/svt-av1")),
    ("dav1d-sys", "dav1d", System),
    // The C++ runtime jpegli needs. macOS links the system's libc++.
    ("jpegli-sys", "c++", System),
    // libstdc++ is linked statically into the release binaries
    // (-static-libstdc++, and the musl builds). The GCC Runtime Library
    // Exception lets compiled code that uses it ship under any terms.
    ("jpegli-sys", "stdc++", Exempt),
];

/// Every (crate, library) pair that a build script output under
/// target/<profile>/build names, and the crates whose output was found.
/// Cargo keeps old outputs there (other features, older versions), so
/// this can be more than the last build linked. That can only make the
/// test fail, never pass; `cargo clean` clears a stale entry.
fn linked_libraries() -> (BTreeSet<(String, String)>, BTreeSet<String>) {
    // The test binary is target/<profile>/deps/<name>; the build script
    // outputs are in target/<profile>/build.
    let exe = std::env::current_exe().expect("test binary path");
    let build = exe
        .parent()
        .and_then(|deps| deps.parent())
        .map(|profile| profile.join("build"))
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(|| panic!("no build directory next to {}", exe.display()));
    let mut linked = BTreeSet::new();
    let mut crates = BTreeSet::new();
    for entry in std::fs::read_dir(&build).expect("read build directory") {
        let dir = entry.expect("build directory entry").path();
        let Ok(output) = std::fs::read_to_string(dir.join("output")) else {
            continue;
        };
        let name = dir.file_name().unwrap_or_default().to_string_lossy();
        let krate = name.rsplit_once('-').map_or(&*name, |(krate, _hash)| krate);
        crates.insert(krate.to_owned());
        for line in output.lines() {
            let Some(lib) = line
                .strip_prefix("cargo::rustc-link-lib=")
                .or_else(|| line.strip_prefix("cargo:rustc-link-lib="))
            else {
                continue;
            };
            // Drop the link kind (`static=`, `dylib=`); keep the name.
            let lib = lib.rsplit_once('=').map_or(lib, |(_kind, name)| name);
            linked.insert((krate.to_owned(), lib.to_owned()));
        }
    }
    (linked, crates)
}

fn notice_for(krate: &str, lib: &str) -> Option<&'static Notice> {
    LINKED
        .iter()
        .find(|(k, prefix, _)| *k == krate && lib.starts_with(prefix))
        .map(|(_, _, notice)| notice)
}

#[test]
fn every_linked_library_has_a_notice() {
    let (linked, crates) = linked_libraries();
    // This package has a build script, so its output must be there. If it
    // is not, the build directory layout has changed and the test would
    // pass on an empty set.
    let own = env!("CARGO_PKG_NAME");
    assert!(
        crates.contains(own),
        "found no build script output for {own}; the layout under target/ may have changed"
    );
    let missing: Vec<String> = linked
        .into_iter()
        .filter(|(krate, lib)| notice_for(krate, lib).is_none())
        .map(|(krate, lib)| format!("{krate} links {lib}"))
        .collect();
    assert!(
        missing.is_empty(),
        "\n\nThese native libraries have no license source: {missing:?}\n\
         A dependency links C code that THIRD-PARTY-LICENSES.md may not cover.\n\
         To fix it:\n\
         1. Find the crate's sources: `cargo metadata --format-version 1`\n\
         \x20  gives its manifest_path. Look for the C code it builds and\n\
         \x20  the LICENSE or COPYING file next to that code.\n\
         2. Add a row to LINKED in tests/third_party_notices.rs. Most\n\
         \x20  libraries a -sys crate builds are `Vendored`, with the path of\n\
         \x20  that file. The comments on `Notice` explain the other kinds.\n\
         3. For `Vendored`, pin each file in about.toml, as the other\n\
         \x20  [[<crate>.clarify.files]] tables do. The checksum is\n\
         \x20  `shasum -a 256 <file>`.\n\
         4. Regenerate the bundle:\n\
         \x20  cargo about generate --features avif about.hbs -o THIRD-PARTY-LICENSES.md\n"
    );
}

/// The `path` of each `[[<crate>.clarify.files]]` table in about.toml.
fn pinned_files(about: &str, krate: &str) -> BTreeSet<String> {
    let header = format!("[[{krate}.clarify.files]]");
    let mut pinned = BTreeSet::new();
    let mut in_table = false;
    for line in about.lines().map(str::trim) {
        if line.starts_with('[') {
            in_table = line == header;
        } else if in_table && let Some(path) = line.strip_prefix("path = ") {
            pinned.insert(path.trim_matches('"').to_owned());
        }
    }
    pinned
}

#[test]
fn vendored_notices_are_pinned_in_about_toml() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let about = std::fs::read_to_string(root.join("about.toml")).expect("read about.toml");
    let dockerfile = std::fs::read_to_string(root.join("Dockerfile")).expect("read Dockerfile");
    let mut problems = Vec::new();
    for (krate, lib, notice) in LINKED {
        match notice {
            Vendored(files) => {
                let pinned = pinned_files(&about, krate);
                for file in *files {
                    if !pinned.contains(*file) {
                        problems.push(format!(
                            "{krate} ({lib}): {file} is not pinned in about.toml. \
                             Add it to [[{krate}.clarify.files]] with its \
                             `shasum -a 256` checksum, then regenerate \
                             THIRD-PARTY-LICENSES.md (see CONTRIBUTING.md)."
                        ));
                    }
                }
            }
            Shipped(dir) => {
                if !dockerfile.contains(dir) {
                    problems.push(format!(
                        "{krate} ({lib}): the Dockerfile does not install \
                         {dir}. Copy the library's license files there in \
                         the runtime stage, and add them to the notice check \
                         in .github/workflows/docker.yml."
                    ));
                }
            }
            Own | System | Exempt => {}
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

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
