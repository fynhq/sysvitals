//! Build script for app-win: embeds the application icon and version info
//! into the PE resource section, and emits the shared `SV_BUILD_ID` stamp.
//! The actual routines live in `sysvitals-build` so they cannot drift from
//! the settings/history/mac scripts.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // `embed-resource` is host-agnostic; the shared helper is a no-op when
    // cross-compiling to a non-Windows target.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        sysvitals_build::embed_resources("assets/icon.ico");
    }
    sysvitals_build::emit_build_id();
}
