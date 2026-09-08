#!/usr/bin/env python3
"""Build the minimal GitHub Release JSON used by the pre-publish upgrade gate."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from urllib.parse import quote


ARCHIVES = (
    "cs-linux-amd64.tar.gz",
    "cs-linux-arm64.tar.gz",
    "cs-darwin-amd64.tar.gz",
    "cs-darwin-arm64.tar.gz",
    "cs-windows-amd64.zip",
    "cs-windows-arm64.zip",
)
HEX_SHA256 = re.compile(r"[0-9a-fA-F]{64}\Z")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def checked_digest(checksum_path: Path, archive_name: str) -> str:
    fields = checksum_path.read_text(encoding="utf-8").split()
    if len(fields) < 2 or not HEX_SHA256.fullmatch(fields[0]):
        raise ValueError(f"invalid SHA-256 file: {checksum_path}")
    if Path(fields[-1].lstrip("*")).name != archive_name:
        raise ValueError(f"checksum file names the wrong archive: {checksum_path}")
    return fields[0].lower()


def build_metadata(
    artifacts_dir: Path, base_url: str, tag: str, name: str
) -> dict[str, object]:
    assets: list[dict[str, str]] = []
    for archive_name in ARCHIVES:
        archive = artifacts_dir / archive_name
        checksum = artifacts_dir / f"{archive_name}.sha256"
        if not archive.is_file() or not checksum.is_file():
            raise ValueError(f"missing release artifact or checksum: {archive_name}")
        expected = checked_digest(checksum, archive_name)
        actual = sha256(archive)
        if actual != expected:
            raise ValueError(f"artifact checksum mismatch: {archive_name}")
        for asset_name in (archive_name, checksum.name):
            assets.append(
                {
                    "name": asset_name,
                    "browser_download_url": f"{base_url.rstrip('/')}/{quote(asset_name)}",
                }
            )
    return {"tag_name": tag, "name": name, "assets": assets}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--artifacts-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--name", required=True)
    args = parser.parse_args()

    metadata = build_metadata(args.artifacts_dir, args.base_url, args.tag, args.name)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(metadata, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


if __name__ == "__main__":
    main()
