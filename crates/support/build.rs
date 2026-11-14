//! Build script for `sysvitals-support`: emits the shared `SV_BUILD_ID`
//! stamp consumed by the indicator startup logs and the settings footer.
//! The routine lives in `sysvitals-build` so every binary reports the same
//! value.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    sysvitals_build::emit_build_id();
}
