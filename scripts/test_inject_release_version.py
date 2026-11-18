#!/usr/bin/env python3
"""Contract tests for the release-version injector."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "inject-release-version.py"
OLD_VERSION = "20261003.1.0"

MANIFEST = f'''[package]
name = "codex-switch"
version = "{OLD_VERSION}"
edition = "2024"

[dependencies]
serde = {{ version = "1", features = ["derive"] }}
local-helper = {{ version = "0.4.0" }}

[package.metadata]
version = "metadata-untouched"
'''.encode()

LOCKFILE = f'''version = 4

[[package]]
name = "codex-switch"
version = "{OLD_VERSION}"
dependencies = [
 "local-helper",
 "serde",
]

[[package]]
name = "local-helper"
version = "0.4.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
'''.encode()


def write_fixture(root: Path, *, lock_version: str = OLD_VERSION) -> None:
    root.mkdir(parents=True, exist_ok=True)
    (root / "VERSION").write_text(f"{OLD_VERSION}\n", encoding="ascii")
    (root / "Cargo.toml").write_bytes(MANIFEST)
    (root / "Cargo.lock").write_bytes(LOCKFILE.replace(OLD_VERSION.encode(), lock_version.encode(), 1))


def run_inject(root: Path, version: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SCRIPT), "--version", version, "--root", str(root)],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )


class ReleaseRefTests(unittest.TestCase):
    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        write_fixture(self.root)

    def resolve(self, ref: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--ref", ref, "--root", str(self.root)],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_resolves_release_channels_without_modifying_package_files(self) -> None:
        for tag, version, prerelease, is_dev in (
            ("dev", f"{OLD_VERSION}-dev", "true", "true"),
            (f"v{OLD_VERSION}-dev", f"{OLD_VERSION}-dev", "true", "false"),
            (f"v{OLD_VERSION}-rc.1", f"{OLD_VERSION}-rc.1", "true", "false"),
            (f"v{OLD_VERSION}", OLD_VERSION, "false", "false"),
        ):
            with self.subTest(tag=tag):
                result = self.resolve(f"refs/tags/{tag}")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(dict(line.split("=", 1) for line in result.stdout.splitlines()), {
                    "version": version, "prerelease": prerelease, "is_dev": is_dev,
                })
        self.assertEqual((self.root / "Cargo.toml").read_bytes(), MANIFEST)
        self.assertEqual((self.root / "Cargo.lock").read_bytes(), LOCKFILE)

    def test_rejects_refs_that_cannot_be_published(self) -> None:
        for ref in (
            "refs/heads/dev",
            f"refs/tags/{OLD_VERSION}",
            "refs/tags/v20261003.2.0",
            f"refs/tags/v{OLD_VERSION}-rc.01",
            f"refs/tags/v{OLD_VERSION}-rc;1",
        ):
            with self.subTest(ref=ref):
                result = self.resolve(ref)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertIn("tag", result.stderr)

    def test_rejects_prerelease_suffix_in_repository_version(self) -> None:
        (self.root / "VERSION").write_text(f"{OLD_VERSION}-dev\n", encoding="ascii")

        result = self.resolve("refs/tags/dev")

        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("YYYYMMDD.N.0", result.stderr)

    def test_rejects_repository_versions_that_disagree(self) -> None:
        for filename, contents in (("Cargo.toml", MANIFEST), ("Cargo.lock", LOCKFILE)):
            with self.subTest(filename=filename):
                path = self.root / filename
                path.write_bytes(contents.replace(OLD_VERSION.encode(), b"20261003.2.0", 1))
                result = self.resolve("refs/tags/dev")
                path.write_bytes(contents)

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertIn(filename, result.stderr)


class InjectReleaseVersionTests(unittest.TestCase):
    def test_injects_stable_and_dev_versions(self) -> None:
        for version in (OLD_VERSION, f"{OLD_VERSION}-dev"):
            with self.subTest(version=version), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                write_fixture(root)

                result = run_inject(root, version)

                self.assertEqual(result.returncode, 0, result.stderr)
                expected_version = version.encode()
                self.assertEqual(
                    (root / "Cargo.toml").read_bytes(),
                    MANIFEST.replace(OLD_VERSION.encode(), expected_version, 1),
                )
                self.assertEqual(
                    (root / "Cargo.lock").read_bytes(),
                    LOCKFILE.replace(OLD_VERSION.encode(), expected_version, 1),
                )

    def test_dependency_records_and_other_fields_are_byte_for_byte_unchanged(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_fixture(root)
            before_manifest = (root / "Cargo.toml").read_bytes()
            before_lockfile = (root / "Cargo.lock").read_bytes()

            result = run_inject(root, f"{OLD_VERSION}-rc.1")

            self.assertEqual(result.returncode, 0, result.stderr)
            expected_manifest = before_manifest.replace(
                f'version = "{OLD_VERSION}"'.encode(),
                b'version = "20261003.1.0-rc.1"',
                1,
            )
            expected_lockfile = before_lockfile.replace(
                f'version = "{OLD_VERSION}"'.encode(),
                b'version = "20261003.1.0-rc.1"',
                1,
            )
            self.assertEqual((root / "Cargo.toml").read_bytes(), expected_manifest)
            self.assertEqual((root / "Cargo.lock").read_bytes(), expected_lockfile)

    def test_accepts_alphanumeric_prerelease_identifiers_starting_with_zero(self) -> None:
        for suffix in ("0rc1", "01beta", "rc.0alpha"):
            with self.subTest(suffix=suffix), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                write_fixture(root)
                version = f"{OLD_VERSION}-{suffix}"

                result = run_inject(root, version)

                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((root / "Cargo.toml").read_bytes(), MANIFEST.replace(
                    OLD_VERSION.encode(), version.encode(), 1))
                self.assertEqual((root / "Cargo.lock").read_bytes(), LOCKFILE.replace(
                    OLD_VERSION.encode(), version.encode(), 1))

    def test_rejects_numeric_prerelease_identifiers_with_leading_zero(self) -> None:
        for suffix in ("01", "rc.01"):
            with self.subTest(suffix=suffix):
                self._assert_rejected_without_changes(f"{OLD_VERSION}-{suffix}")

    def test_invalid_version_rejects_without_writing(self) -> None:
        self._assert_rejected_without_changes("20261003.1")

    def test_missing_manifest_rejects_without_writing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            root.mkdir(parents=True, exist_ok=True)
            (root / "Cargo.lock").write_bytes(LOCKFILE)
            before = (root / "Cargo.lock").read_bytes()

            result = run_inject(root, f"{OLD_VERSION}-dev")

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual((root / "Cargo.lock").read_bytes(), before)

    def test_missing_lockfile_rejects_without_writing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            root.mkdir(parents=True, exist_ok=True)
            (root / "Cargo.toml").write_bytes(MANIFEST)
            before = (root / "Cargo.toml").read_bytes()

            result = run_inject(root, f"{OLD_VERSION}-dev")

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual((root / "Cargo.toml").read_bytes(), before)

    def test_inconsistent_root_versions_reject_without_writing(self) -> None:
        self._assert_rejected_without_changes(f"{OLD_VERSION}-dev", lock_version="20261003.2.0")

    def _assert_rejected_without_changes(self, version: str, *, lock_version: str = OLD_VERSION) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_fixture(root, lock_version=lock_version)
            before_manifest = (root / "Cargo.toml").read_bytes()
            before_lockfile = (root / "Cargo.lock").read_bytes()

            result = run_inject(root, version)

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual((root / "Cargo.toml").read_bytes(), before_manifest)
            self.assertEqual((root / "Cargo.lock").read_bytes(), before_lockfile)


if __name__ == "__main__":
    unittest.main()
