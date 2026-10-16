fn main() {
    // The variadic progress-callback shim (see src/oc_progress.c).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rerun-if-changed=src/oc_progress.c");
        cc::Build::new().file("src/oc_progress.c").compile("oc_progress");
    }
}
