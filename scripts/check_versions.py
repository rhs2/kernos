#!/usr/bin/env python3
"""Fail unless every published artefact carries the same version and the
changelog has an entry for it.

Checked: the three Rust crates, the Python package, the npm package, and
CHANGELOG.md. With --tag vX.Y.Z the version must also equal the tag, which is
how the release workflow refuses to publish a mislabelled tag.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def cargo_version(path: Path) -> str:
    text = path.read_text(encoding="utf-8")
    in_package = False
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_package = stripped in ("[package]", "[workspace.package]")
            continue
        if in_package:
            m = re.match(r'version\s*=\s*"([^"]+)"', stripped)
            if m:
                return m.group(1)
    # crates that inherit from the workspace
    if "version.workspace = true" in text or "version = { workspace = true }" in text:
        return cargo_version(ROOT / "Cargo.toml")
    raise SystemExit(f"no version in {path}")


def python_version(path: Path) -> str:
    m = re.search(r'^version\s*=\s*"([^"]+)"', path.read_text(encoding="utf-8"), re.M)
    if not m:
        raise SystemExit(f"no version in {path}")
    return m.group(1)


def npm_version(path: Path) -> str:
    return json.loads(path.read_text(encoding="utf-8"))["version"]


def changelog_has(version: str) -> bool:
    text = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    return re.search(rf"^## \[{re.escape(version)}\]", text, re.M) is not None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tag", help="git tag the release is being cut from, e.g. v0.1.0")
    args = ap.parse_args()

    found = {
        "crates/kernos-core": cargo_version(ROOT / "crates/kernos-core/Cargo.toml"),
        "crates/kernos-policy": cargo_version(ROOT / "crates/kernos-policy/Cargo.toml"),
        "crates/kernos": cargo_version(ROOT / "crates/kernos/Cargo.toml"),
        "sdk/python": python_version(ROOT / "sdk/python/pyproject.toml"),
        "sdk/typescript": npm_version(ROOT / "sdk/typescript/package.json"),
    }
    versions = set(found.values())
    for name, v in found.items():
        print(f"{name:24s} {v}")
    if len(versions) != 1:
        print("versions differ", file=sys.stderr)
        return 1
    version = versions.pop()
    if not changelog_has(version):
        print(f"CHANGELOG.md has no entry for {version}", file=sys.stderr)
        return 1
    if args.tag and args.tag.lstrip("v") != version:
        print(f"tag {args.tag} does not match version {version}", file=sys.stderr)
        return 1
    print(f"all artefacts at {version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
