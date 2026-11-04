//! Build script for app-history: embeds the application icon and version info
//! into the PE resource section so the history window shows the SysVitals icon
//! in its title bar and taskbar entry on Windows, and Explorer shows its file
//! properties.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // `embed-resource` is host-agnostic, so the dependency is unconditional
    // and no compile-time `#[cfg(windows)]` gate is needed. This runtime check
    // covers a Windows host cross-compiling to a non-Windows target, where a
    // PE resource must not be linked into the foreign binary.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resources();
    }
}

fn embed_resources() {
    for path in ["app.rc", "../app-win/assets/icon.ico"] {
        if Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }

    match embed_resource::compile("app.rc", version_macros()) {
        embed_resource::CompilationResult::Ok | embed_resource::CompilationResult::NotWindows => {}
        // Non-fatal: the binary still works, just without an icon.
        other => println!("cargo:warning=Failed to embed resources: {other}"),
    }
}

/// rc.exe cannot read Cargo's package metadata, so the version fields reach
/// the `.rc` file as preprocessor defines; a workspace version bump then
/// updates every resource without touching the resource scripts.
fn version_macros() -> [String; 4] {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| "0".into());
    [
        format!("APP_MAJOR={}", var("CARGO_PKG_VERSION_MAJOR")),
        format!("APP_MINOR={}", var("CARGO_PKG_VERSION_MINOR")),
        format!("APP_PATCH={}", var("CARGO_PKG_VERSION_PATCH")),
        format!("APP_VERSION=\"{}\"", var("CARGO_PKG_VERSION")),
    ]
}
