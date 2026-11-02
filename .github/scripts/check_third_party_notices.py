#!/usr/bin/env python3
"""Verify that THIRD_PARTY_NOTICES.md matches the shipped dependency graph."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

TARGETS = [
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
]
ROOTS = ["app-win", "app-settings", "app-history", "app-mac"]
OWN = {
    "app-win",
    "app-settings",
    "app-history",
    "app-mac",
    "sysvitals-core",
    "sysvitals-panel",
}
FINGERPRINT_RE = re.compile(r"Dependency-set fingerprint:\s*([0-9a-f]{64})")


def metadata(repo: Path, target: str) -> dict:
    return json.loads(
        subprocess.check_output(
            [
                "cargo",
                "metadata",
                "--locked",
                "--format-version",
                "1",
                "--filter-platform",
                target,
            ],
            cwd=repo,
            text=True,
        )
    )


def normal_closure(meta: dict, root: str) -> set[str]:
    nodes = {node["id"]: node for node in meta["resolve"]["nodes"]}
    seen = {root}
    stack = [root]
    while stack:
        current = stack.pop()
        node = nodes.get(current)
        if not node:
            continue
        for dep in node["deps"]:
            kinds = dep.get("dep_kinds") or []
            if any(kind.get("kind") in (None, "normal") for kind in kinds):
                if dep["pkg"] not in seen:
                    seen.add(dep["pkg"])
                    stack.append(dep["pkg"])
    return seen


def expected_components(repo: Path) -> set[tuple[str, str, str]]:
    components: set[tuple[str, str, str]] = set()
    for target in TARGETS:
        meta = metadata(repo, target)
        by_id = {pkg["id"]: pkg for pkg in meta["packages"]}
        roots = {pkg["name"]: pkg["id"] for pkg in meta["packages"] if pkg["name"] in ROOTS}
        for root_id in roots.values():
            for pkg_id in normal_closure(meta, root_id):
                pkg = by_id.get(pkg_id)
                if pkg and pkg["name"] not in OWN:
                    components.add(
                        (pkg["name"], pkg["version"], pkg.get("license") or "UNKNOWN")
                    )
    return components


def parse_components(path: Path) -> tuple[set[tuple[str, str, str]], str]:
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()
    try:
        start = lines.index("## Components") + 1
    except ValueError:
        raise SystemExit(f"{path}: missing '## Components' section")
    components: set[tuple[str, str, str]] = set()
    for line in lines[start:]:
        if line.startswith("## "):
            break
        if not line.startswith("|"):
            continue
        cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
        if len(cells) < 3 or cells[0] == "Package":
            continue
        if set(cells[0]) <= {"-", ":"}:
            continue
        components.add(
            (
                cells[0].replace("\\|", "|"),
                cells[1],
                cells[2].replace("\\|", "|"),
            )
        )
    return components, text


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--notices", default=None)
    args = parser.parse_args()

    repo = Path(__file__).resolve().parents[2]
    notices = Path(args.notices) if args.notices else repo / "THIRD_PARTY_NOTICES.md"
    if not notices.is_file():
        print(f"{notices}: file not found", file=sys.stderr)
        return 1

    try:
        expected = expected_components(repo)
    except subprocess.CalledProcessError as exc:
        print(f"cargo metadata failed: {exc}", file=sys.stderr)
        return 1

    actual, text = parse_components(notices)
    fingerprint = hashlib.sha256(
        "\n".join(
            sorted(
                f"{name}\t{version}\t{license_id}"
                for name, version, license_id in expected
            )
        ).encode("utf-8")
    ).hexdigest()

    errors = []
    match = FINGERPRINT_RE.search(text)
    if not match:
        errors.append("missing dependency-set fingerprint")
    elif match.group(1) != fingerprint:
        errors.append(
            f"dependency-set fingerprint mismatch: file={match.group(1)} expected={fingerprint}"
        )

    missing = sorted(expected - actual)
    extra = sorted(actual - expected)
    if missing:
        errors.append(f"missing {len(missing)} component(s):")
        errors.extend(
            f"  {name} {version} | {license_id}" for name, version, license_id in missing[:20]
        )
        if len(missing) > 20:
            errors.append(f"  ... and {len(missing) - 20} more")
    if extra:
        errors.append(f"unexpected {len(extra)} component(s):")
        errors.extend(
            f"  {name} {version} | {license_id}" for name, version, license_id in extra[:20]
        )
        if len(extra) > 20:
            errors.append(f"  ... and {len(extra) - 20} more")

    if errors:
        print(f"{notices} is out of date:", file=sys.stderr)
        for error in errors:
            print(error, file=sys.stderr)
        print("Regenerate the file from the current Cargo.lock.", file=sys.stderr)
        return 1

    print(f"{notices} matches the dependency graph ({len(expected)} packages).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
