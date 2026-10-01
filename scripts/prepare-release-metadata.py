#!/usr/bin/env python3
"""Build the minimal GitHub API responses used by the pre-publish upgrade gate.

Writes the release JSON and, when requested, the `git/ref/tags/<tag>` reference
that updaters with provenance verification resolve to the attested commit.
"""

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
    artifacts_dir: Path,
    base_url: str,
    tag: str,
    name: str,
    extra_assets: tuple[str, ...] = (),
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
    for asset_name in extra_assets:
        if not (artifacts_dir / asset_name).is_file():
            raise ValueError(f"missing extra release asset: {asset_name}")
        assets.append(
            {
                "name": asset_name,
                "browser_download_url": f"{base_url.rstrip('/')}/{quote(asset_name)}",
            }
        )
    return {"tag_name": tag, "name": name, "assets": assets}


def build_tag_reference(tag: str, commit_sha: str) -> dict[str, object]:
    if not re.fullmatch(r"[0-9a-fA-F]{40}", commit_sha):
        raise ValueError(f"invalid commit SHA: {commit_sha}")
    return {
        "ref": f"refs/tags/{tag}",
        "object": {"type": "commit", "sha": commit_sha.lower()},
    }


def write_json(path: Path, value: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--artifacts-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--name", required=True)
    parser.add_argument(
        "--extra-asset",
        action="append",
        default=[],
        help="additional file in --artifacts-dir to list as a release asset",
    )
    parser.add_argument("--tag-ref-output", type=Path)
    parser.add_argument("--commit-sha")
    args = parser.parse_args()
    if (args.tag_ref_output is None) != (args.commit_sha is None):
        parser.error("--tag-ref-output and --commit-sha must be given together")

    metadata = build_metadata(
        args.artifacts_dir, args.base_url, args.tag, args.name, tuple(args.extra_asset)
    )
    write_json(args.output, metadata)
    if args.tag_ref_output is not None:
        write_json(args.tag_ref_output, build_tag_reference(args.tag, args.commit_sha))


if __name__ == "__main__":
    main()
