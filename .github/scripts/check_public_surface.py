#!/usr/bin/env python3
"""Check that the public surface of this repository stays public.

Runs in CI for new commits and in the release jobs for packaged binaries.
It rejects private-document references, local environment narration, local
absolute paths, verification trailers, skip-ci markers, and secret-like strings.

The scanning primitives live in _surface.py so that this gate and the private
workspace guard cannot drift apart. Patterns that would name a private
environment are supplied by that guard, never defined here.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import _surface  # noqa: E402


def build_config() -> _surface.SurfaceConfig:
    return _surface.SurfaceConfig(label="public_surface")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", default=".")
    parser.add_argument("--range")
    parser.add_argument("--tag")
    parser.add_argument("--tree", action="store_true")
    parser.add_argument("--assets", action="append")
    args = parser.parse_args()

    config = build_config()
    repo = Path(args.repo).resolve()
    errors: list[str] = []

    if args.range:
        _surface.scan_range(repo, args.range, errors, config)
    if args.tag:
        _surface.scan_tag(repo, args.tag, errors, config)
    if args.tree:
        _surface.scan_tree(repo, errors, config)
    for asset in args.assets or []:
        _surface.scan_asset_path(Path(asset), errors, config)

    return _surface.report(config, errors)


if __name__ == "__main__":
    sys.exit(main())
