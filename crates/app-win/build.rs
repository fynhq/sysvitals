//! Build script for app-win: embeds the application icon into the PE
//! resource section so the exe shows the SysVitals icon in Explorer,
//! taskbar, and window chrome, and emits the same build stamp used by the
//! settings footer and the macOS startup log.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    embed_resources();

    emit_build_id();
}

/// Compile-time gate: `winres` is a Windows-only build-dependency, so the
/// `winres::` path must not exist on other hosts at all. A runtime
/// `CARGO_CFG_TARGET_OS` check is too late — the path is resolved before
/// `main` runs, so `cargo check`/build on a non-Windows host fails with E0433.
#[cfg(windows)]
fn embed_resources() {
    // The runtime check still matters for a Windows host cross-compiling to
    // a non-Windows target: `winres` is linked there, but embedding PE
    // resources would break the foreign binary.
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }

    let mut res = winres::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    res.set("ProductName", "SysVitals");
    res.set("FileDescription", "SysVitals — lightweight system monitor");
    res.set("LegalCopyright", "MIT License");
    if let Err(e) = res.compile() {
        // Non-fatal: the binary still works, just without an icon.
        println!("cargo:warning=Failed to embed resources: {e}");
    }
}

/// Run `git -C <repo> <args>` and return the trimmed stdout, or `None` when
/// git is missing, the repo is unreachable, the command fails, or the output
/// is empty. Kept deliberately permissive so every caller can fall through
/// to the plain `v{semver}` stamp instead of aborting the build.
fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo);
    let out = cmd.args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn emit_build_id() {
    // Register this before the override early-return: otherwise Cargo would
    // not know to rerun the script when the override is later removed.
    println!("cargo:rerun-if-env-changed=SV_BUILD_ID_OVERRIDE");
    // CI and manual builds can pin the exact string shown in the log by
    // exporting `SV_BUILD_ID_OVERRIDE`; when set it wins unconditionally so
    // the stamp is reproducible even without a git checkout.
    if let Some(over) = std::env::var("SV_BUILD_ID_OVERRIDE")
        .ok()
        .filter(|s| !s.is_empty())
    {
        println!("cargo:rustc-env=SV_BUILD_ID={over}");
        return;
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let repo = Path::new(&manifest_dir).join("../..");

    // Ask cargo to rerun this build script when the checked-out commit or a
    // tag moves. Watching only `.git/HEAD` misses commits on the same branch
    // (HEAD is a symref) and newly created tags. Emitting a path only when it
    // exists avoids cargo's "path outside of the crate directory" warning on
    // tarball builds.
    for candidate in [
        repo.join(".git/HEAD"),
        repo.join(".git/logs/HEAD"),
        repo.join(".git/packed-refs"),
        repo.join(".git/refs/tags"),
        repo.join(".git/refs/heads"),
    ] {
        if candidate.exists() {
            let watch = candidate.canonicalize().unwrap_or(candidate);
            println!("cargo:rerun-if-changed={}", watch.display());
        }
    }

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());

    // `git describe --tags --exact-match` succeeds for any tag. Only a `v*`
    // tag is a release signal; acceptance tags (`build-*`) point at the same
    // commit the dev package was built from, so they keep the hash + `(dev)`
    // stamp instead of looking like a release. A failed lookup means "dev".
    let tagged = git(&repo, &["describe", "--tags", "--exact-match"]);
    let release = tagged.as_deref().is_some_and(|tag| tag.starts_with('v'));
    let stamp = if release {
        format!("v{version}")
    } else {
        match git(&repo, &["rev-parse", "--short", "HEAD"]) {
            Some(hash) => format!("v{version}_{hash} (dev)"),
            None => format!("v{version}"),
        }
    };
    println!("cargo:rustc-env=SV_BUILD_ID={stamp}");
}
