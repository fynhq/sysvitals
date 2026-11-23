#!/usr/bin/env python3
"""Print the workspace package version declared in Cargo.toml.

Parses the [workspace.package] table specifically, so an unrelated `version`
key elsewhere in the manifest can never be picked up. Used by every CI job that
needs the release version.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

WORKSPACE_VERSION = re.compile(
    r"^\[workspace\.package\][\s\S]*?^version\s*=\s*\"([^\"]+)\"",
    re.MULTILINE,
)


def read_version(manifest: Path) -> str:
    if not manifest.is_file():
        raise SystemExit(f"read_version: manifest not found: {manifest}")
    match = WORKSPACE_VERSION.search(manifest.read_text(encoding="utf-8"))
    if not match:
        raise SystemExit(f"read_version: no [workspace.package] version in {manifest}")
    return match.group(1)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--cargo-toml",
        default=None,
        help="path to the workspace manifest (default: Cargo.toml next to the repo root)",
    )
    args = parser.parse_args()

    if args.cargo_toml:
        manifest = Path(args.cargo_toml).resolve()
    else:
        manifest = Path(__file__).resolve().parents[2] / "Cargo.toml"
    print(read_version(manifest))
    return 0


if __name__ == "__main__":
    sys.exit(main())
