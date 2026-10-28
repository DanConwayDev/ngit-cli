#!/usr/bin/env python3
"""Check the embedded OpenSSL version in both binaries of a release archive.

This is a packaging regression guard, not a substitute for platform runtime
TLS tests or inspecting dynamic dependencies. The helper is a thin launcher
and must use the ngit packaged alongside it; it contains no OpenSSL itself.
"""

import argparse
from pathlib import Path
import re
import struct
import tarfile
import zipfile

EXPECTED = b"3.5.9"


def macho_slices(binary):
    """Return each architecture separately so a fixed slice cannot mask a stale one."""
    magic = binary[:4]
    if magic in (b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf"):
        return [binary]
    if magic not in (b"\xca\xfe\xba\xbe", b"\xca\xfe\xba\xbf"):
        return None
    count = struct.unpack_from(">I", binary, 4)[0]
    if count != 2:
        raise ValueError("Universal archive must contain both macOS architectures")
    format = ">iiIII" if magic[-1] == 0xbe else ">iiQQII"
    size = struct.calcsize(format)
    slices = []
    cpus = set()
    for index in range(count):
        cpu, _, offset, length, *_ = struct.unpack_from(format, binary, 8 + index * size)
        if offset < 8 + count * size or offset + length > len(binary):
            raise ValueError("Invalid universal binary slice bounds")
        cpus.add(cpu)
        slices.append(binary[offset:offset + length])
    if cpus != {0x01000007, 0x0100000c}:
        raise ValueError("Universal archive must contain x86_64 and arm64")
    return slices


def verify(archive):
    if zipfile.is_zipfile(archive):
        with zipfile.ZipFile(archive) as source:
            binaries = {Path(name).name: source.read(name) for name in source.namelist()
                        if Path(name).name in ("ngit.exe", "git-remote-nostr.exe")}
        names = ("ngit.exe", "git-remote-nostr.exe")
    else:
        with tarfile.open(archive) as source:
            binaries = {Path(member.name).name: source.extractfile(member).read()
                        for member in source.getmembers() if member.isfile()
                        and Path(member.name).name in ("ngit", "git-remote-nostr")}
        names = ("ngit", "git-remote-nostr")
    if set(binaries) != set(names):
        raise ValueError("Archive must contain both ngit and git-remote-nostr")
    for name in names:
        versions = set(re.findall(rb"OpenSSL (\d+\.\d+\.\d+[a-z]?)\b", binaries[name]))
        slices = macho_slices(binaries[name]) if name == "ngit" else None
        if slices is not None:
            # Secure Transport handles macOS HTTPS. Only libcrypto is retained
            # for SSH, so the unused OpenSSL_version banner may be stripped.
            for binary in slices:
                found = set(re.findall(rb"OpenSSL (\d+\.\d+\.\d+[a-z]?)\b", binary))
                if (found - {EXPECTED} or b"OpenSSL Default Provider\0" not in binary
                        or EXPECTED + b"\0" not in binary):
                    raise ValueError(f"{name}: macOS slice lacks the patched OpenSSL provider")
            print(f"{name}: OpenSSL {EXPECTED.decode()} provider in {len(slices)} macOS slice(s)")
        else:
            # Windows uses WinHTTP/WinCNG, not OpenSSL.
            expected = {EXPECTED} if name == "ngit" else set()
            if versions != expected or (not expected and b"OpenSSL Default Provider" in binaries[name]):
                raise ValueError(f"{name}: expected {expected}, found {versions}")
            print(f"{name}: " + (f"OpenSSL {EXPECTED.decode()}" if expected else "no embedded OpenSSL"))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    verify(parser.parse_args().archive)
