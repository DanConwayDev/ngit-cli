#!/usr/bin/env python3
"""Package portable binaries and verify the exact cross-provider release set."""
import argparse
import hashlib
import gzip
import importlib.util
import io
import json
import os
import re
from pathlib import Path
import shutil
import struct
import subprocess
import tarfile
import tomllib
import zipfile

spec = importlib.util.spec_from_file_location("openssl", Path(__file__).with_name("verify-openssl.py"))
openssl = importlib.util.module_from_spec(spec)
spec.loader.exec_module(openssl)
TARGETS = {
    "linux-x86_64": "x86_64-unknown-linux-gnu.2.17",
    "linux-aarch64": "aarch64-unknown-linux-gnu.2.17",
    "linux-musl": "x86_64-unknown-linux-musl",
    "macos": "universal-apple-darwin",
    "windows": "x86_64-pc-windows-msvc",
}


def filename(target):
    return f"ngit-{target}." + ("zip" if "windows" in target else "tar.gz")


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def inspect_binary(data, target):
    if "linux" in target:
        machine = 183 if target.startswith("aarch64") else 62
        if data[:6] != b"\x7fELF\x02\x01" or struct.unpack_from("<H", data, 18)[0] != machine:
            raise ValueError(f"Wrong ELF architecture for {target}")
    elif "windows" in target:
        if data[:2] != b"MZ":
            raise ValueError("Missing Windows executable header")
        offset = struct.unpack_from("<I", data, 60)[0]
        if data[offset:offset + 6] != b"PE\0\0\x64\x86":
            raise ValueError("Wrong Windows executable architecture")
    elif data[:4] not in (b"\xca\xfe\xba\xbe", b"\xca\xfe\xba\xbf") or len(openssl.macho_slices(data)) != 2:
        raise ValueError("Expected universal macOS binary")


def verify_archive(path, target):
    names = {"ngit.exe", "git-remote-nostr.exe"} if "windows" in target else {"ngit", "git-remote-nostr"}
    if "windows" in target:
        with zipfile.ZipFile(path) as archive:
            if set(archive.namelist()) != names or len(archive.infolist()) != 2:
                raise ValueError("Unexpected archive members")
            binaries = {name: archive.read(name) for name in names}
    else:
        with tarfile.open(path) as archive:
            members = archive.getmembers()
            if {m.name for m in members} != names or len(members) != 2 or any(not m.isfile() or not m.mode & 0o111 for m in members):
                raise ValueError("Unexpected archive members or executable permissions")
            binaries = {m.name: archive.extractfile(m).read() for m in members}
    for data in binaries.values():
        inspect_binary(data, target)
    openssl.verify(path)


def inspect_linkage(path, target):
    if "linux" in target:
        dynamic = subprocess.check_output(["readelf", "-d", str(path)], text=True)
        headers = subprocess.check_output(["readelf", "-l", str(path)], text=True)
        if "/nix/store/" in dynamic or "/nix/store/" in headers:
            raise ValueError("Portable archive depends on the Nix store")
        needed = re.findall(r"\(NEEDED\).*?\[(.*?)\]", dynamic)
        allowed = {"libc.so.6", "libm.so.6", "libpthread.so.0", "libdl.so.2", "librt.so.1", "libgcc_s.so.1"}
        if set(needed) - allowed:
            raise ValueError(f"Unexpected dynamic dependencies: {needed}")
        if "musl" in target and (needed or "INTERP" in headers):
            raise ValueError("musl archive must be static")
        if target.endswith("gnu.2.17"):
            versions = subprocess.check_output(["readelf", "--version-info", str(path)], text=True)
            if any(tuple(map(int, v.split("."))) > (2, 17) for v in re.findall(r"GLIBC_(\d+\.\d+)", versions)):
                raise ValueError("Archive requires glibc newer than 2.17")
    elif "darwin" in target:
        linked = subprocess.check_output(["otool", "-L", str(path)], text=True)
        for line in linked.splitlines():
            if line.startswith("\t") and not line.strip().startswith(("/usr/lib/", "/System/Library/")):
                raise ValueError(f"Unexpected macOS dynamic dependency: {line.strip()}")


def pack(target, binaries, output):
    output.mkdir(parents=True, exist_ok=True)
    path = output / filename(target)
    suffix = ".exe" if "windows" in target else ""
    names = [f"{name}{suffix}" for name in ("ngit", "git-remote-nostr")]
    for name in names:
        inspect_binary((binaries / name).read_bytes(), target)
        inspect_linkage(binaries / name, target)
    if suffix:
        with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for name in names:
                member = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
                member.compress_type = zipfile.ZIP_DEFLATED
                member.external_attr = 0o100755 << 16
                archive.writestr(member, (binaries / name).read_bytes())
    else:
        with path.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed, tarfile.open(fileobj=compressed, mode="w") as archive:
            for name in names:
                data = (binaries / name).read_bytes()
                member = tarfile.TarInfo(name)
                member.size, member.mode = len(data), 0o755
                archive.addfile(member, io.BytesIO(data))
    verify_archive(path, target)
    if target == "universal-apple-darwin" or target.startswith("x86_64-unknown-linux"):
        runner = [os.environ["NGIT_ARCHIVE_LOADER"]] if target.endswith("gnu.2.17") and "NGIT_ARCHIVE_LOADER" in os.environ else []
        subprocess.run([*runner, str(binaries / "ngit"), "--version"], check=True, timeout=30)
    report = {
        "schema": 1, "commit": os.environ["NGIT_CI_COMMIT_SHA"],
        "version": tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"],
        "target": target, "archive": path.name, "sha256": digest(path),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
    }
    (output / "build.json").write_text(json.dumps(report, indent=2) + "\n")


def collect(inputs, output, commit):
    # Validate everything before exporting anything for publication.
    selected = []
    for job, target in TARGETS.items():
        root = inputs / job / "artifacts"
        report = json.loads((root / "report" / "build.json").read_text())
        archive = root / "archive" / filename(target)
        if (report.get("schema") != 1 or report.get("commit") != commit
                or report.get("target") != target or report.get("archive") != archive.name
                or report.get("sha256") != digest(archive)):
            raise ValueError(f"Artifact provenance/checksum mismatch: {job}")
        verify_archive(archive, target)
        selected.append((archive, report))
    versions = {report["version"] for _, report in selected}
    expected = tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]
    if versions != {expected}:
        raise ValueError("Artifact versions do not match the source checkout")
    output.mkdir(parents=True, exist_ok=True)
    for archive, _ in selected:
        shutil.copyfile(archive, output / archive.name)
    (output / "SHA256SUMS").write_text("".join(f'{report["sha256"]}  {archive.name}\n' for archive, report in selected))
    (output / "builds.json").write_text(json.dumps([r for _, r in selected], indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    build = sub.add_parser("pack")
    build.add_argument("target", choices=TARGETS.values())
    build.add_argument("binaries", type=Path)
    build.add_argument("output", type=Path)
    gather = sub.add_parser("collect")
    gather.add_argument("inputs", type=Path)
    gather.add_argument("output", type=Path)
    args = parser.parse_args()
    if args.command == "pack":
        pack(args.target, args.binaries.resolve(), args.output)
    else:
        collect(args.inputs, args.output, os.environ["NGIT_CI_COMMIT_SHA"])
