#!/usr/bin/env python3
"""Check that the public surface of this repository stays public.

Runs in CI for new commits and in the release jobs for packaged binaries.
It rejects private-document references, local environment narration, local
absolute paths, verification trailers, and secret-like strings.

This script deliberately contains only generic patterns; personal identifiers
belong in the private workspace guard, never in the public repository.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import zipfile
from pathlib import Path

PRIVATE_DOC = re.compile(
    r"("
    r"\b(?<!LICENSE-)(?!README\b|THIRD_PARTY_NOTICES\b|LICENSE\b)[A-Z][A-Z0-9_-]{2,}\.md\b"
    r"|docs/[A-Za-z0-9_-]+\.md"
    r"|\b[A-Z]{5,}\.[0-9]+(?:\.[0-9]+)*\b"
    r"|§"
    r"|\b[RQ](?:[1-9]|1[0-9]|2[0-3])\b"
    r")"
)
ENV_NARRATION = re.compile(
    r"(WSL|local host|workstation|host PowerShell|no macOS host|"
    r"does not compile on|MSVC not installed|rustup)",
    re.IGNORECASE,
)
VERIFICATION = re.compile(r"^\s*Verification:\s*", re.IGNORECASE | re.MULTILINE)
LOCAL_PATH = re.compile(r"([A-Za-z]:\\Users\\|/home/|/Users/|/mnt/)")
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
CJK = re.compile(r"[\u4e00-\u9fff]")
SECRET = re.compile(
    r"(\bpassword\b|\bpasswd\b|\bsecret\b|api[_-]?key|private[_-]?key|"
    r"ghp_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|"
    r"AKIA[0-9A-Z]{16}|xox[bp]-[A-Za-z0-9-]{10,}|BEGIN [A-Z ]*PRIVATE KEY)",
    re.IGNORECASE,
)
TREE_ALLOW = {
    "README.md": {"cjk"},
    "THIRD_PARTY_NOTICES.md": {"email"},
    "Cargo.lock": {"env-narration"},
    "crates/core/src/i18n.rs": {"cjk"},
    "crates/core/locales/zh.conf": {"cjk"},
    "crates/core/src/config.rs": {"cjk"},
    "crates/core/src/format.rs": {"cjk"},
    "crates/core/src/sampler/net.rs": {"env-narration"},
    ".github/scripts/check_public_surface.py": {"all"},
}
BINARY_PATTERNS = [
    (b"Verification:", "verification trailer"),
]
PRINTABLE = re.compile(rb"[\x20-\x7e]{6,}")
USER_PATH = re.compile(
    r"(?:C:\\Users\\(?!(?:runner|runneradmin|Public|Default|All Users)\\)[A-Za-z0-9_.-]+"
    r"|/home/(?!(?:runner)/)[A-Za-z0-9_.-]+"
    r"|/Users/(?!(?:runner)/)[A-Za-z0-9_.-]+"
    r"|/mnt/[a-z]/[A-Za-z0-9_.-]+)"
)


def run_git(repo: Path, args: list[str]) -> str:
    result = subprocess.run(
        ["git", "-C", str(repo), *args],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=True,
    )
    return result.stdout.decode("utf-8", "replace")


def scan_text(kind: str, path: str, text: str, allow: set[str], errors: list[str]) -> None:
    if "all" in allow:
        return
    for number, line in enumerate(text.splitlines(), 1):
        checks = [
            ("private-doc", PRIVATE_DOC),
            ("env-narration", ENV_NARRATION),
            ("local-path", LOCAL_PATH),
            ("secret", SECRET),
        ]
        for name, pattern in checks:
            if name not in allow and pattern.search(line):
                errors.append(f"{kind} {path}:{number}: {name}: {line.strip()[:160]}")
        if "email" not in allow and EMAIL.search(line):
            errors.append(f"{kind} {path}:{number}: email: {line.strip()[:160]}")
        if "cjk" not in allow and CJK.search(line):
            errors.append(f"{kind} {path}:{number}: cjk: {line.strip()[:160]}")
        if VERIFICATION.search(line):
            errors.append(f"{kind} {path}:{number}: verification-tail: {line.strip()[:160]}")


def scan_message(path: str, text: str, errors: list[str]) -> None:
    scan_text("message", path, text, set(), errors)


def scan_range(repo: Path, rev_range: str, errors: list[str]) -> None:
    try:
        text = run_git(repo, ["log", "--no-merges", "--format=%B%x1e", rev_range])
    except subprocess.CalledProcessError:
        text = run_git(repo, ["log", "-1", "--format=%B"])
        scan_message("HEAD", text, errors)
        return
    for index, body in enumerate(text.split("\x1e"), 1):
        if body.strip():
            scan_message(f"{rev_range}#{index}", body, errors)


def scan_tree(repo: Path, errors: list[str]) -> None:
    for name in run_git(repo, ["ls-files", "-z"]).split("\x00"):
        if not name:
            continue
        path = repo / name
        if not path.is_file():
            continue
        data = path.read_bytes()
        if b"\x00" in data[:4096]:
            continue
        scan_text("tree", name, data.decode("utf-8", "replace"), TREE_ALLOW.get(name, set()), errors)


def scan_tag(repo: Path, tag: str, errors: list[str]) -> None:
    text = run_git(repo, ["tag", "-l", "-n99", tag])
    scan_text("tag", f"tag:{tag}", text, {"cjk"}, errors)


def scan_assets(root: Path, errors: list[str]) -> None:
    if root.is_file():
        if root.suffix.lower() == ".zip":
            with zipfile.ZipFile(root) as archive:
                for entry in archive.infolist():
                    scan_binary(root, entry.filename, archive.read(entry), errors)
        else:
            scan_binary(root, "", root.read_bytes(), errors)
        return
    for path in root.rglob("*"):
        if not path.is_file():
            continue
        if path.suffix.lower() == ".zip":
            with zipfile.ZipFile(path) as archive:
                for entry in archive.infolist():
                    scan_binary(path, entry.filename, archive.read(entry), errors)
        else:
            scan_binary(path, "", path.read_bytes(), errors)


def scan_binary(path: Path, entry: str, data: bytes, errors: list[str]) -> None:
    label = f"{path}!{entry}" if entry else str(path)
    for needle, reason in BINARY_PATTERNS:
        if needle in data:
            errors.append(f"asset {label}: {reason}: {needle.decode(errors='replace')}")
    for run in PRINTABLE.finditer(data):
        text = run.group().decode("ascii", "ignore")
        if USER_PATH.search(text):
            errors.append(f"asset {label}: local user path: {text[:120]}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", default=".")
    parser.add_argument("--range")
    parser.add_argument("--tag")
    parser.add_argument("--tree", action="store_true")
    parser.add_argument("--assets", action="append")
    args = parser.parse_args()

    repo = Path(args.repo).resolve()
    errors: list[str] = []
    if args.range:
        scan_range(repo, args.range, errors)
    if args.tag:
        scan_tag(repo, args.tag, errors)
    if args.tree:
        scan_tree(repo, errors)
    for asset in args.assets or []:
        scan_assets(Path(asset), errors)

    if errors:
        for error in errors:
            print(f"ERROR {error}")
        print(f"public_surface: {len(errors)} finding(s)")
        return 1
    print("public_surface: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())