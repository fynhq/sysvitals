//! Shared build-script helpers: resources and build stamp.

use std::path::Path;
use std::process::Command;

pub fn embed_resources(icon: &str) {
    for path in ["app.rc", icon] {
        if Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }

    match embed_resource::compile("app.rc", version_macros()) {
        embed_resource::CompilationResult::Ok | embed_resource::CompilationResult::NotWindows => {}
        other => println!("cargo:warning=Failed to embed resources: {other}"),
    }
}

pub fn version_macros() -> [String; 4] {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| "0".into());
    [
        format!("APP_MAJOR={}", var("CARGO_PKG_VERSION_MAJOR")),
        format!("APP_MINOR={}", var("CARGO_PKG_VERSION_MINOR")),
        format!("APP_PATCH={}", var("CARGO_PKG_VERSION_PATCH")),
        format!("APP_VERSION=\"{}\"", var("CARGO_PKG_VERSION")),
    ]
}

// Build stamp: v{semver} on a v* tag, otherwise v{semver}_{short-hash} (dev); SV_BUILD_ID_OVERRIDE wins.
pub fn emit_build_id() {
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
