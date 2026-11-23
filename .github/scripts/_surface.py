#!/usr/bin/env python3
"""Shared primitives for scanning the public surface of this repository.

Both the in-repo CI gate and the private workspace guard are thin entry points
over this module, so the two can no longer drift apart. Entry points supply a
SurfaceConfig; patterns that are specific to a non-public environment are never
defined here.
"""

from __future__ import annotations

import re
import subprocess
import sys
import zipfile
from collections.abc import Iterable
from dataclasses import dataclass, field
from pathlib import Path

PRIVATE_DOC = re.compile(
    r"("
    r"\b(?<!LICENSE-)(?!README\b|THIRD_PARTY_NOTICES\b|LICENSE\b)[A-Z][A-Z0-9_-]{2,}\.md\b"
    r"|\b(?:AGENTS|CHANGELOG|LESSONS|MEMORY|REQUIREMENTS)\b"
    r"|docs/[A-Za-z0-9_./-]+\.md"
    r"|\b[A-Z]{5,}\.[0-9]+(?:\.[0-9]+)*\b"
    r"|§"
    r"|\b[RQ](?:[1-9]|1[0-9]|2[0-3])\b"
    r")"
)
ENV_NARRATION = re.compile(
    r"(WSL|local host|workstation|host PowerShell|no macOS host|"
    r"does not compile on|MSVC not installed|"
    r"rustup\s+(?:is\s+)?(?:not\s+(?:installed|available)|removed))",
    re.IGNORECASE,
)
LOCAL_PATH = re.compile(
    r"([A-Za-z]:\\Users\\(?!runneradmin)|/home/(?!runner)|/Users/(?!runner)|/mnt/)"
)
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
CJK = re.compile(r"[\u4e00-\u9fff]")
SECRET = re.compile(
    r"(\bpassword\b|\bpasswd\b|\bsecret\b|api[_-]?key|private[_-]?key|"
    r"ghp_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|"
    r"AKIA[0-9A-Z]{16}|xox[bp]-[A-Za-z0-9-]{10,}|BEGIN [A-Z ]*PRIVATE KEY)",
    re.IGNORECASE,
)
VERIFICATION = re.compile(r"^\s*Verification:\s*", re.IGNORECASE | re.MULTILINE)
SKIP_CI = re.compile(r"\[skip[ -]?ci\]", re.IGNORECASE)

BASE_TEXT_CHECKS: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("private-doc", PRIVATE_DOC),
    ("env-narration", ENV_NARRATION),
    ("local-path", LOCAL_PATH),
    ("secret", SECRET),
    ("skip-ci", SKIP_CI),
    ("email", EMAIL),
    ("cjk", CJK),
    ("verification-tail", VERIFICATION),
)

PRINTABLE = re.compile(rb"[\x20-\x7e]{6,}")
USER_PATH = re.compile(
    r"(?:C:\\Users\\(?!(?:runner|runneradmin|Public|Default|All Users)\\)[A-Za-z0-9_.-]+"
    r"|/home/(?!runner/)[A-Za-z0-9_.-]+"
    r"|/Users/(?!runner/)[A-Za-z0-9_.-]+"
    r"|/mnt/[a-z]/[A-Za-z0-9_.-]+)"
)
BINARY_PATH = re.compile(
    rb"(?:[A-Za-z]:\\Users\\(?!runner)|/home/(?!runner)|/Users/(?!runner)|/mnt/[a-z]/)"
)

BASE_BINARY_NEEDLES: tuple[tuple[bytes, str], ...] = (
    (b"Verification:", "verification trailer"),
    (b"AGENTS", "private document reference"),
    (b"LESSONS", "private document reference"),
    (b"CHANGELOG", "private document reference"),
    (b"REQUIREMENTS", "private document reference"),
    (b"MEMORY.md", "private document reference"),
)

BASE_TREE_ALLOW: dict[str, set[str]] = {
    "README.md": {"cjk"},
    "THIRD_PARTY_NOTICES.md": {"email", "private-doc"},
    "Cargo.lock": {"env-narration"},
    "crates/core/src/i18n.rs": {"cjk"},
    "crates/core/locales/zh.conf": {"cjk"},
    "crates/core/src/config.rs": {"cjk"},
    "crates/core/src/format.rs": {"cjk"},
    "crates/core/src/sampler/net.rs": {"env-narration"},
    ".github/scripts/check_public_surface.py": {"all"},
    ".github/scripts/_surface.py": {"all"},
    "scripts/public_guard.py": {"all"},
}


@dataclass
class SurfaceConfig:
    label: str = "public_surface"
    text_checks: tuple[tuple[str, re.Pattern[str]], ...] = BASE_TEXT_CHECKS
    binary_needles: tuple[tuple[bytes, str], ...] = BASE_BINARY_NEEDLES
    tree_allow: dict[str, set[str]] = field(default_factory=lambda: dict(BASE_TREE_ALLOW))


def run_git(repo: Path, args: list[str]) -> str:
    return run_git_bytes(repo, args).decode("utf-8", "replace")


def run_git_bytes(repo: Path, args: list[str]) -> bytes:
    result = subprocess.run(
        ["git", "-C", str(repo), *args],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=True,
    )
    return result.stdout


def git_quiet(repo: Path, args: list[str]) -> str | None:
    try:
        return run_git(repo, args)
    except (subprocess.CalledProcessError, OSError):
        return None


def git_quiet_bytes(repo: Path, args: list[str]) -> bytes | None:
    try:
        return run_git_bytes(repo, args)
    except (subprocess.CalledProcessError, OSError):
        return None


def scan_text(
    kind: str,
    path: str,
    text: str,
    allow: set[str],
    errors: list[str],
    config: SurfaceConfig,
) -> None:
    if "all" in allow:
        return
    for number, line in enumerate(text.splitlines(), 1):
        for name, pattern in config.text_checks:
            if name in allow:
                continue
            if pattern.search(line):
                errors.append(f"{kind} {path}:{number}: {name}: {line.strip()[:160]}")


def scan_message(path: str, text: str, errors: list[str], config: SurfaceConfig) -> None:
    scan_text("message", path, text, set(), errors, config)


def scan_range(repo: Path, rev_range: str, errors: list[str], config: SurfaceConfig) -> None:
    text = None
    if rev_range and ".." in rev_range and not rev_range.startswith(".."):
        text = git_quiet(repo, ["log", "--no-merges", "--format=%B%x1e", rev_range])
    if text is None:
        text = git_quiet(repo, ["log", "-1", "--format=%B"]) or ""
        scan_message("HEAD", text, errors, config)
        return
    for index, body in enumerate(text.split("\x1e"), 1):
        if body.strip():
            scan_message(f"{rev_range}#{index}", body, errors, config)


def _iter_blobs(
    repo: Path, names: Iterable[str], from_index: bool
) -> Iterable[tuple[str, bytes]]:
    for name in names:
        if not name:
            continue
        data: bytes | None = None
        if from_index:
            data = git_quiet_bytes(repo, ["show", f":{name}"])
        else:
            path = repo / name
            if path.is_file():
                try:
                    data = path.read_bytes()
                except OSError:
                    data = None
        if data is None or b"\x00" in data[:4096]:
            continue
        yield name, data


def scan_tree(repo: Path, errors: list[str], config: SurfaceConfig) -> None:
    listing = git_quiet(repo, ["ls-files", "-z"])
    if listing is None:
        errors.append(f"{config.label}: cannot list tracked files in {repo}")
        return
    for name, data in _iter_blobs(repo, listing.split("\x00"), False):
        allow = config.tree_allow.get(name, set())
        scan_text("tree", name, data.decode("utf-8", "replace"), allow, errors, config)


def scan_staged(repo: Path, errors: list[str], config: SurfaceConfig) -> None:
    listing = git_quiet(repo, ["diff", "--cached", "--name-only", "-z"])
    if listing is None:
        errors.append(f"{config.label}: cannot list staged files in {repo}")
        return
    for name, data in _iter_blobs(repo, listing.split("\x00"), True):
        allow = config.tree_allow.get(name, set())
        scan_text("staged", name, data.decode("utf-8", "replace"), allow, errors, config)


def tag_contents(repo: Path, tag: str) -> str:
    text = git_quiet(repo, ["for-each-ref", f"refs/tags/{tag}", "--format=%(contents)"])
    return text if text is not None else ""


def scan_tag(repo: Path, tag: str, errors: list[str], config: SurfaceConfig) -> None:
    body = tag_contents(repo, tag)
    if not body.strip():
        errors.append(f"tag {tag}: cannot read the tag annotation; nothing was scanned")
        return
    scan_text("tag", f"tag:{tag}", body, {"cjk"}, errors, config)


def scan_binary(path: Path, entry: str, data: bytes, errors: list[str], config: SurfaceConfig) -> None:
    label = f"{path}!{entry}" if entry else str(path)
    for needle, reason in config.binary_needles:
        if needle in data:
            errors.append(f"asset {label}: {reason}: {needle.decode(errors='replace')}")
    for match in BINARY_PATH.finditer(data):
        context = data[max(0, match.start() - 20):match.end() + 40]
        errors.append(f"asset {label}: local path: {context.decode('utf-8', 'replace')[:120]}")
    for run in PRINTABLE.finditer(data):
        text = run.group().decode("ascii", "ignore")
        found = USER_PATH.search(text)
        if found:
            errors.append(f"asset {label}: local user path: {found.group(0)[:120]}")


def scan_asset_path(root: Path, errors: list[str], config: SurfaceConfig) -> None:
    if not root.exists():
        errors.append(f"{config.label}: asset path does not exist: {root}")
        return
    targets = [root] if root.is_file() else [p for p in root.rglob("*") if p.is_file()]
    if not targets:
        errors.append(f"{config.label}: asset path holds no files to scan: {root}")
        return
    for path in targets:
        if path.suffix.lower() == ".zip":
            try:
                with zipfile.ZipFile(path) as archive:
                    for entry in archive.infolist():
                        scan_binary(path, entry.filename, archive.read(entry), errors, config)
            except (zipfile.BadZipFile, OSError) as exc:
                errors.append(f"{config.label}: cannot read zip {path}: {exc}")
            continue
        try:
            data = path.read_bytes()
        except OSError as exc:
            errors.append(f"{config.label}: cannot read {path}: {exc}")
            continue
        scan_binary(path, "", data, errors, config)


def is_utc(date: str) -> bool:
    return date.endswith("+00:00") or date.endswith("Z")


def scan_tz(repo: Path, rev_range: str, errors: list[str], config: SurfaceConfig) -> None:
    spec = rev_range if rev_range and ".." in rev_range else "HEAD"
    text = git_quiet(repo, ["log", "--no-merges", "--format=%H%x1f%aI%x1f%cI%x1e", spec])
    if text is None:
        errors.append(f"{config.label}: cannot read commit metadata for {spec}")
        return
    seen_tags: set[str] = set()
    for record in text.split("\x1e"):
        fields = [item for item in record.strip().split("\x1f") if item]
        if len(fields) != 3:
            continue
        sha, author_date, committer_date = fields
        for label, date in (("author", author_date), ("committer", committer_date)):
            if not is_utc(date):
                errors.append(
                    f"commit {sha[:7]}: {label} date {date} is not UTC; "
                    "create commits with TZ=UTC"
                )
        for tag in (git_quiet(repo, ["tag", "--points-at", sha]) or "").split():
            if tag in seen_tags:
                continue
            seen_tags.add(tag)
            tagger = (
                git_quiet(
                    repo,
                    ["for-each-ref", f"refs/tags/{tag}", "--format=%(taggerdate:iso-strict)"],
                )
                or ""
            ).strip()
            if tagger and not is_utc(tagger):
                errors.append(
                    f"tag {tag}: tagger date {tagger} is not UTC; "
                    "create annotated tags with TZ=UTC"
                )


def report(config: SurfaceConfig, errors: list[str]) -> int:
    if errors:
        for error in errors:
            print(f"ERROR {error}")
        print(f"{config.label}: {len(errors)} finding(s)")
        return 1
    print(f"{config.label}: OK")
    return 0
