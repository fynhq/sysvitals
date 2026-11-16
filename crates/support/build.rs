//! Build script: emits the shared SV_BUILD_ID.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    sysvitals_build::emit_build_id();
}
