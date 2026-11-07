//! Build script for app-mac: emits the startup build identifier. The routine
//! is shared with the indicator and the settings footer via `sysvitals-build`.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    sysvitals_build::emit_build_id();
}
