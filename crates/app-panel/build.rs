//! Build script: embeds the Windows icon/version resources.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        sysvitals_build::embed_resources("../app-win/assets/icon.ico");
    }
}
