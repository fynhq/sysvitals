//! Build script for app-settings: embeds the application icon/version
//! resources on Windows. The build stamp is emitted by `sysvitals-support`.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        sysvitals_build::embed_resources("../app-win/assets/icon.ico");
    }
}
