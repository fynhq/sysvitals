//! Build script for app-mac: computes the human-readable build identifier
//! logged at startup: `v{semver}` when HEAD sits on an annotated tag (a
//! release build), `v{semver}_{short-hash} (dev)` on any other commit, and
//! just `v{semver}` when git or the repository metadata is unavailable
//! (tarballs, vendored copies, cache warm-up). The stamp matches the one in
//! the settings footer, so a log file can be tied to an exact build.
//!
//! CI passes `fetch-tags: true` to the checkout, so an annotated release tag
//! is visible here; builds without git metadata fall back to the plain
//! version stamp.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    emit_build_id();
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
    // CI and manual builds can pin the exact string logged at startup by
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

    // `git describe --tags --exact-match` succeeds only when HEAD points at a
    // tagged commit — the release signal. A failed lookup means "dev build".
    let tagged = git(&repo, &["describe", "--tags", "--exact-match"]);
    let stamp = match tagged {
        Some(_) => format!("v{version}"),
        None => match git(&repo, &["rev-parse", "--short", "HEAD"]) {
            Some(hash) => format!("v{version}_{hash} (dev)"),
            None => format!("v{version}"),
        },
    };
    println!("cargo:rustc-env=SV_BUILD_ID={stamp}");
}
