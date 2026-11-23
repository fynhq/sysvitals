#!/usr/bin/env python3
"""Verify that THIRD_PARTY_NOTICES.md matches the shipped dependency graph."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from dataclasses import dataclass
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
    "sysvitals-support",
}
FINGERPRINT_RE = re.compile(r"Dependency-set fingerprint:\s*([0-9a-f]{64})")
COMPONENTS_HEADING = "## Components"
LICENSES_HEADING = "## License texts and notices"
USED_BY_PREFIX = "Used by: "


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


@dataclass
class Notices:
    components: set[tuple[str, str, str]]
    sources: dict[tuple[str, str], str]
    licensed: set[tuple[str, str]]
    text: str


def parse_notices(path: Path) -> Notices:
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()
    try:
        start = lines.index(COMPONENTS_HEADING) + 1
    except ValueError:
        raise SystemExit(f"{path}: missing '{COMPONENTS_HEADING}' section")
    end = next(
        (index for index in range(start, len(lines)) if lines[index].startswith("## ")),
        len(lines),
    )
    components: set[tuple[str, str, str]] = set()
    sources: dict[tuple[str, str], str] = {}
    for line in lines[start:end]:
        if not line.startswith("|"):
            continue
        cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
        if len(cells) < 3 or cells[0] == "Package":
            continue
        if set(cells[0]) <= {"-", ":"}:
            continue
        name = cells[0].replace("\\|", "|")
        version = cells[1]
        license_id = cells[2].replace("\\|", "|")
        components.add((name, version, license_id))
        sources[(name, version)] = cells[3].replace("\\|", "|") if len(cells) > 3 else ""

    try:
        license_start = lines.index(LICENSES_HEADING)
    except ValueError:
        license_start = len(lines)
    licensed: set[tuple[str, str]] = set()
    for line in lines[license_start:]:
        if not line.startswith(USED_BY_PREFIX):
            continue
        for item in line[len(USED_BY_PREFIX):].split(","):
            name, _, version = item.strip().rpartition(" ")
            if name and version:
                licensed.add((name, version))
    return Notices(components, sources, licensed, text)


def fingerprint(components: set[tuple[str, str, str]]) -> str:
    return hashlib.sha256(
        "\n".join(
            sorted(f"{name}\t{version}\t{license_id}" for name, version, license_id in components)
        ).encode("utf-8")
    ).hexdigest()


def validate(
    expected: set[tuple[str, str, str]], parsed: Notices
) -> list[str]:
    actual = parsed.components
    recorded = fingerprint(actual)
    computed = fingerprint(expected)

    errors: list[str] = []
    match = FINGERPRINT_RE.search(parsed.text)
    if not match:
        errors.append("missing dependency-set fingerprint")
    else:
        if match.group(1) != recorded:
            errors.append(
                f"recorded fingerprint does not match the component table in this file: "
                f"file={match.group(1)} table={recorded} "
                "(the table was edited or truncated after generation)"
            )
        if match.group(1) != computed:
            errors.append(
                f"dependency-set fingerprint mismatch: file={match.group(1)} expected={computed}"
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
    no_source = sorted(
        (name, version) for (name, version), source in parsed.sources.items() if not source
    )
    bad_source = sorted(
        (name, version, source)
        for (name, version), source in parsed.sources.items()
        if source and not source.startswith(("http://", "https://"))
    )
    if no_source:
        errors.append(f"{len(no_source)} component(s) have no Source column: {no_source[:10]}")
    if bad_source:
        errors.append(f"{len(bad_source)} component(s) have a non-URL Source column:")
        errors.extend(f"  {name} {version} | {source[:80]}" for name, version, source in bad_source[:10])
    unlicensed = sorted(
        (name, version) for name, version, _ in actual if (name, version) not in parsed.licensed
    )
    if not parsed.licensed:
        errors.append(
            f"missing '{LICENSES_HEADING}' section or it holds no '{USED_BY_PREFIX}' entry; "
            "the reproduced license texts were never validated"
        )
    elif unlicensed:
        errors.append(f"{len(unlicensed)} component(s) have no reproduced license text:")
        errors.extend(f"  {name} {version}" for name, version in unlicensed[:20])
        if len(unlicensed) > 20:
            errors.append(f"  ... and {len(unlicensed) - 20} more")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--notices", default=None)
    args = parser.parse_args()

    repo = Path(__file__).resolve().parents[2]
    notices_path = Path(args.notices) if args.notices else repo / "THIRD_PARTY_NOTICES.md"
    if not notices_path.is_file():
        print(f"{notices_path}: file not found", file=sys.stderr)
        return 1

    try:
        expected = expected_components(repo)
    except subprocess.CalledProcessError as exc:
        print(f"cargo metadata failed: {exc}", file=sys.stderr)
        return 1

    parsed = parse_notices(notices_path)
    errors = validate(expected, parsed)

    if errors:
        print(f"{notices_path} is out of date:", file=sys.stderr)
        for error in errors:
            print(error, file=sys.stderr)
        print("Regenerate the file from the current Cargo.lock.", file=sys.stderr)
        return 1

    print(
        f"{notices_path} matches the dependency graph "
        f"({len(expected)} packages, {len(parsed.licensed)} license texts)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
