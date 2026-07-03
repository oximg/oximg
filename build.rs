fn main() {
    // SVT-AV1 linking, only when the avif feature is enabled. Use a
    // Release build of the library: several distros ship debug builds
    // that encode at half speed.
    if std::env::var_os("CARGO_FEATURE_AVIF").is_some() {
        if let Ok(dir) = std::env::var("SVT_AV1_LIB_DIR") {
            println!("cargo:rustc-link-search=native={dir}");
        } else {
            for dir in ["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"] {
                if std::path::Path::new(dir)
                    .join("libSvtAv1Enc.dylib")
                    .exists()
                    || std::path::Path::new(dir).join("libSvtAv1Enc.so").exists()
                    || std::path::Path::new(dir).join("libSvtAv1Enc.a").exists()
                {
                    println!("cargo:rustc-link-search=native={dir}");
                    break;
                }
            }
        }
        println!("cargo:rustc-link-lib=SvtAv1Enc");
        println!("cargo:rerun-if-env-changed=SVT_AV1_LIB_DIR");
    }
}
