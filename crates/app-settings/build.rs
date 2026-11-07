//! Build script for app-settings: embeds the application icon/version
//! resources on Windows and emits the build identifier shown in the footer.
//! The routines are shared with the other binaries in `sysvitals-build`.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        sysvitals_build::embed_resources("../app-win/assets/icon.ico");
    }
    sysvitals_build::emit_build_id();
}
