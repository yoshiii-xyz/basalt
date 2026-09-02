#!/usr/bin/env python3
"""Verify cargo-dist archives before they are published."""

from __future__ import annotations

import argparse
import hashlib
import re
import tarfile
import zipfile
from pathlib import Path, PurePosixPath


ARCHIVE_SUFFIXES = (".tar.xz", ".zip")
CHECKSUM_PATTERN = re.compile(r"^(?P<digest>[0-9a-fA-F]{64})\s+\*?(?P<name>[^\s]+)$")
REQUIRED_FILES = ("CHANGELOG.md", "LICENSE", "README.md")


def archive_paths(inputs: list[Path]) -> list[Path]:
    paths: list[Path] = []
    for input_path in inputs:
        if input_path.is_dir():
            paths.extend(
                path
                for path in sorted(input_path.iterdir())
                if path.is_file() and path.name.endswith(ARCHIVE_SUFFIXES)
            )
        else:
            paths.append(input_path)
    if not paths:
        raise ValueError("no .tar.xz or .zip release archives were found")
    return paths


def verify_checksum(archive: Path) -> str:
    checksum_path = Path(f"{archive}.sha256")
    try:
        lines = [line.strip() for line in checksum_path.read_text(encoding="utf-8").splitlines()]
    except OSError as error:
        raise ValueError(f"cannot read checksum for {archive.name}: {error}") from error
    lines = [line for line in lines if line]
    if len(lines) != 1:
        raise ValueError(f"checksum must contain exactly one entry: {checksum_path}")
    match = CHECKSUM_PATTERN.fullmatch(lines[0])
    if match is None or match.group("name") != archive.name:
        raise ValueError(f"checksum entry does not name {archive.name}: {lines[0]!r}")

    digest = hashlib.sha256()
    with archive.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    actual = digest.hexdigest()
    expected = match.group("digest").lower()
    if actual != expected:
        raise ValueError(
            f"checksum mismatch for {archive.name}: expected {expected}, got {actual}"
        )
    return actual


def safe_member_name(name: str, archive: Path) -> PurePosixPath:
    if not name or "\\" in name:
        raise ValueError(f"unsafe archive member in {archive.name}: {name!r}")
    path = PurePosixPath(name)
    if path.is_absolute() or any(part in ("", ".", "..") for part in path.parts):
        raise ValueError(f"unsafe archive member in {archive.name}: {name!r}")
    return path


def verify_tar(archive: Path) -> set[str]:
    with tarfile.open(archive, mode="r:xz") as stream:
        names: set[str] = set()
        for member in stream.getmembers():
            path = safe_member_name(member.name, archive)
            if member.issym() or member.islnk() or member.isdev():
                raise ValueError(f"archive contains a link or device: {archive.name}:{member.name}")
            if not member.isdir() and not member.isfile():
                raise ValueError(f"archive contains an unsupported member: {archive.name}:{member.name}")
            if str(path) in names:
                raise ValueError(f"archive contains a duplicate member: {archive.name}:{member.name}")
            names.add(str(path))
        return names


def verify_zip(archive: Path) -> set[str]:
    with zipfile.ZipFile(archive) as stream:
        names: set[str] = set()
        for member in stream.infolist():
            path = safe_member_name(member.filename, archive)
            mode = (member.external_attr >> 16) & 0o170000
            if mode == 0o120000:
                raise ValueError(f"archive contains a symbolic link: {archive.name}:{member.filename}")
            if str(path) in names:
                raise ValueError(f"archive contains a duplicate member: {archive.name}:{member.filename}")
            names.add(str(path))
        return names


def verify_members(archive: Path) -> None:
    if archive.suffixes[-2:] == [".tar", ".xz"]:
        names = verify_tar(archive)
        root = archive.name.removesuffix(".tar.xz")
        prefix = f"{root}/"
        required = {f"{prefix}{name}" for name in REQUIRED_FILES} | {f"{prefix}basalt"}
    elif archive.suffix == ".zip":
        names = verify_zip(archive)
        required = set(REQUIRED_FILES) | {"basalt.exe"}
    else:
        raise ValueError(f"unsupported release archive: {archive}")

    missing = sorted(required - names)
    if missing:
        raise ValueError(f"{archive.name} is missing required members: {', '.join(missing)}")


def verify_archive(archive: Path) -> None:
    if not archive.is_file():
        raise ValueError(f"release archive does not exist: {archive}")
    digest = verify_checksum(archive)
    verify_members(archive)
    print(f"verified {archive.name} ({digest})")


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Verify cargo-dist release archive checksums, members, and extraction safety."
    )
    parser.add_argument("paths", nargs="+", type=Path, help="release archives or directories containing them")
    args = parser.parse_args()
    try:
        for archive in archive_paths(args.paths):
            verify_archive(archive)
    except (OSError, tarfile.TarError, ValueError, zipfile.BadZipFile) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
