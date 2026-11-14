//! Build script for app-win: embeds the application icon and version info
//! into the PE resource section. The build stamp is emitted by
//! `sysvitals-support` so every binary shares one value.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // `embed-resource` is host-agnostic; the shared helper is a no-op when
    // cross-compiling to a non-Windows target.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        sysvitals_build::embed_resources("assets/icon.ico");
    }
}
