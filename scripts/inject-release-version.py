#!/usr/bin/env python3
"""Validate release metadata or inject a version into the local package records."""

from __future__ import annotations

import argparse
import re
from pathlib import Path


SCRIPT_ROOT = Path(__file__).resolve().parents[1]
BASE_VERSION = re.compile(rb"[1-9][0-9]{7}\.[1-9][0-9]*\.0")
SEMVER = re.compile(
    rb"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
    rb"(?:-(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
    rb"(?:\.(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*)?\Z"
)
SECTION = re.compile(rb"(?m)^[ \t]*\[([^\]\r\n]+)\][ \t]*(?:#.*)?(?:\r?$)")
PACKAGE_BLOCK = re.compile(rb"(?m)^[ \t]*\[\[package\]\][ \t]*(?:#.*)?(?:\r?$)")
STRING_KEY = re.compile(
    rb"(?m)^[ \t]*([A-Za-z0-9_-]+)[ \t]*=[ \t]*([\"'])([^\"']*)\2[ \t]*(?:#.*)?(?:\r?$)"
)


def _section(data: bytes, name: bytes) -> tuple[int, int]:
    headers = list(SECTION.finditer(data))
    matches = [header for header in headers if header.group(1).strip() == name]
    if len(matches) != 1:
        raise ValueError(f"Cargo.toml must contain exactly one [{name.decode()}] section")
    header = matches[0]
    next_header = next(
        (candidate.start() for candidate in headers if candidate.start() > header.start()),
        len(data),
    )
    return header.end(), next_header


def _key(section: bytes, key: bytes, filename: str) -> re.Match[bytes]:
    matches = [match for match in STRING_KEY.finditer(section) if match.group(1) == key]
    if len(matches) != 1:
        raise ValueError(f"{filename} must contain exactly one string key {key.decode()!r}")
    return matches[0]


def _manifest_version(data: bytes) -> tuple[bytes, tuple[int, int]]:
    start, end = _section(data, b"package")
    package = data[start:end]
    name = _key(package, b"name", "Cargo.toml")
    if name.group(3) != b"codex-switch":
        raise ValueError("Cargo.toml [package] name must be 'codex-switch'")
    version = _key(package, b"version", "Cargo.toml")
    value_start, value_end = (start + offset for offset in version.span(3))
    return version.group(3), (value_start, value_end)


def _lock_version(data: bytes) -> tuple[bytes, tuple[int, int]]:
    blocks = list(PACKAGE_BLOCK.finditer(data))
    candidates: list[tuple[int, int, re.Match[bytes]]] = []
    for index, block in enumerate(blocks):
        block_end = blocks[index + 1].start() if index + 1 < len(blocks) else len(data)
        content = data[block.end() : block_end]
        name = _key(content, b"name", "Cargo.lock")
        if name.group(3) == b"codex-switch":
            candidates.append((block.end(), block_end, name))
    local = [candidate for candidate in candidates if not any(
        match.group(1) == b"source"
        for match in STRING_KEY.finditer(data[candidate[0] : candidate[1]])
    )]
    if len(local) != 1:
        raise ValueError("Cargo.lock must contain exactly one local codex-switch package")
    block_start, block_end, _ = local[0]
    content = data[block_start:block_end]
    version = _key(content, b"version", "Cargo.lock")
    value_start, value_end = (block_start + offset for offset in version.span(3))
    return version.group(3), (value_start, value_end)


def _replace(data: bytes, span: tuple[int, int], value: bytes) -> bytes:
    start, end = span
    return data[:start] + value + data[end:]


def check_base(root: Path) -> bytes:
    base = (root / "VERSION").read_bytes().strip()
    if BASE_VERSION.fullmatch(base) is None:
        raise ValueError(f"VERSION must use YYYYMMDD.N.0 with N starting at 1, got {base!r}")

    for filename, read_version in (("Cargo.toml", _manifest_version), ("Cargo.lock", _lock_version)):
        actual, _ = read_version((root / filename).read_bytes())
        if actual != base:
            raise ValueError(f"{filename} package version {actual!r} must match VERSION {base!r}")
    return base


def resolve_ref(ref: str, base: bytes) -> tuple[bytes, bool]:
    if not ref.startswith("refs/tags/"):
        raise ValueError(f"release requires a tag ref, got {ref!r}")
    tag = ref.removeprefix("refs/tags/")
    if tag == "dev":
        return base + b"-dev", True

    if tag.startswith("v") and tag.isascii():
        version = tag[1:].encode("ascii")
        if SEMVER.fullmatch(version) and version.split(b"-", 1)[0] == base:
            return version, False

    raise ValueError(
        f"release tag {tag!r} must match v{base.decode('ascii')} with an optional SemVer prerelease"
    )


def inject(root: Path, version: str) -> None:
    try:
        target = version.encode("ascii")
    except UnicodeEncodeError as error:
        raise ValueError("version must be ASCII SemVer") from error
    if SEMVER.fullmatch(target) is None:
        raise ValueError(f"invalid SemVer release version: {version!r}")

    manifest_path = root / "Cargo.toml"
    lock_path = root / "Cargo.lock"
    try:
        manifest = manifest_path.read_bytes()
        lockfile = lock_path.read_bytes()
    except OSError as error:
        raise ValueError(f"unable to read release inputs: {error}") from error

    manifest_version, manifest_span = _manifest_version(manifest)
    lock_version, lock_span = _lock_version(lockfile)
    if manifest_version != lock_version:
        raise ValueError(
            "Cargo.toml [package] version and Cargo.lock local codex-switch version must match"
        )

    new_manifest = _replace(manifest, manifest_span, target)
    new_lockfile = _replace(lockfile, lock_span, target)
    try:
        if new_manifest != manifest:
            manifest_path.write_bytes(new_manifest)
        if new_lockfile != lockfile:
            lock_path.write_bytes(new_lockfile)
    except OSError as error:
        raise ValueError(f"unable to write release inputs: {error}") from error


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--version", help="release SemVer to inject")
    mode.add_argument("--ref", help="Git tag ref to resolve to GitHub Actions outputs")
    mode.add_argument("--check-base", action="store_true", help="check repository version records")
    parser.add_argument("--root", type=Path, default=SCRIPT_ROOT, help="repository root")
    args = parser.parse_args()
    try:
        if args.version is not None:
            inject(args.root, args.version)
            print(f"Injected release version {args.version} into Cargo.toml and Cargo.lock")
            return

        base = check_base(args.root)
        if args.ref is not None:
            version, is_dev = resolve_ref(args.ref, base)
            print(f"version={version.decode('ascii')}")
            print(f"prerelease={str(version != base).lower()}")
            print(f"is_dev={str(is_dev).lower()}")
    except (OSError, ValueError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
