//! Shared build-script helpers for the SysVitals binaries.
//!
//! The Windows binaries embed the same icons/version resources through this
//! crate, and `sysvitals-support` emits the single `SV_BUILD_ID` stamp every
//! process reports. This crate keeps those routines in one place so a change
//! to the resource layout or the build-id rules cannot drift between
//! `build.rs` files.
//!
//! The functions run inside the *calling* package's build-script process, so
//! `CARGO_MANIFEST_DIR`, `CARGO_PKG_VERSION*` and the `cargo:` output lines
//! refer to that package. The workspace root is two levels above every app
//! crate.

use std::path::Path;
use std::process::Command;

/// Embed `app.rc` (icon + VERSIONINFO) into the PE resource section. `icon`
/// is the icon path relative to the calling crate; the `app.rc` file name is
/// fixed. No-op when cross-compiling to a non-Windows target.
pub fn embed_resources(icon: &str) {
    for path in ["app.rc", icon] {
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
pub fn version_macros() -> [String; 4] {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| "0".into());
    [
        format!("APP_MAJOR={}", var("CARGO_PKG_VERSION_MAJOR")),
        format!("APP_MINOR={}", var("CARGO_PKG_VERSION_MINOR")),
        format!("APP_PATCH={}", var("CARGO_PKG_VERSION_PATCH")),
        format!("APP_VERSION=\"{}\"", var("CARGO_PKG_VERSION")),
    ]
}

/// Emit the human-readable build identifier consumed as `SV_BUILD_ID`:
/// `v{semver}` when HEAD sits on a `v*` tag (a release build),
/// `v{semver}_{short-hash} (dev)` on any other commit — including `build-*`
/// acceptance tags — and just `v{semver}` when git or the repository metadata
/// is unavailable (tarballs, vendored copies, cache warm-up).
///
/// `SV_BUILD_ID_OVERRIDE` wins unconditionally so CI and manual builds can
/// pin a reproducible stamp.
pub fn emit_build_id() {
    // Register this before the override early-return: otherwise Cargo would
    // not know to rerun the script when the override is later removed.
    println!("cargo:rerun-if-env-changed=SV_BUILD_ID_OVERRIDE");
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
