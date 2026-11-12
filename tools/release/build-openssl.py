#!/usr/bin/env python3
"""Build the pinned static OpenSSL used by release archives (Python 3.12+).

openssl-sys supports OpenSSL 3.x, but its openssl-src 300.x dependency still
ships 3.6.3. Supply a private static prefix through its documented environment
interface until a patched, compatible openssl-src is published. No host library
is used. Run before Cargo with the release target; write the variables
to GITHUB_ENV (or an env file to export locally). Linux cross builds require Zig on PATH.
"""

import argparse
import hashlib
import os
from pathlib import Path
import shlex
import subprocess
import tarfile
import urllib.request

VERSION = "3.5.9"
SHA256 = "603f5602e2eef00d77fbd429d34dcd5822bb301757a1bc9cdb24c670f1eb859a"
URL = f"https://github.com/openssl/openssl/releases/download/openssl-{VERSION}/openssl-{VERSION}.tar.gz"
TARGETS = {
    "x86_64-unknown-linux-gnu": ("linux-x86_64", None),
    "x86_64-unknown-linux-gnu.2.17": ("linux-x86_64", "x86_64-linux-gnu.2.17"),
    "aarch64-unknown-linux-gnu.2.17": ("linux-aarch64", "aarch64-linux-gnu.2.17"),
    "x86_64-unknown-linux-musl": ("linux-x86_64", "x86_64-linux-musl"),
    "x86_64-apple-darwin": ("darwin64-x86_64-cc", None),
    "aarch64-apple-darwin": ("darwin64-arm64-cc", None),
}


def build(target, root, archive, jobs):
    configure_target, zig_target = TARGETS[target]
    directory = root / target
    directory.mkdir(parents=True, exist_ok=True)
    prefix = directory / "install"
    # Do not reuse a partial build or a prefix produced from different sources.
    if (directory / f"openssl-{VERSION}").exists() or prefix.exists():
        raise RuntimeError(f"Use a fresh build directory: {directory}")
    with tarfile.open(archive) as source:
        source.extractall(directory, filter="data")
    source = directory / f"openssl-{VERSION}"
    env = os.environ.copy()
    options = ["no-shared", "no-module", "no-tests", "no-comp", "no-zlib",
               "no-ssl3", "no-weak-ssl-ciphers", "no-async", "--libdir=lib",
               f"--prefix={prefix.as_posix()}"]
    if zig_target:
        for variable, command in [("CC", "cc"), ("AR", "ar"), ("RANLIB", "ranlib")]:
            wrapper = directory / command
            args = ["zig", command]
            if command == "cc":
                args += ["-target", zig_target]
            wrapper.write_text("#!/bin/sh\nexec " + shlex.join(args) + ' "$@"\n')
            wrapper.chmod(0o755)
            env[variable] = str(wrapper)
        options += ["--openssldir=/usr/local/ssl"]
    elif "linux" in target:
        options += ["--openssldir=/usr/local/ssl"]
    elif "darwin" in target:
        env["CC"] = "clang"
        env["MACOSX_DEPLOYMENT_TARGET"] = "10.13" if target.startswith("x86_64") else "11.0"
        options += ["--openssldir=/usr/local/ssl"]
    def run(*args):
        subprocess.run(args, cwd=source, env=env, check=True)
    run("perl", "Configure", configure_target, *options)
    run("make", f"-j{jobs}")
    run("make", "install_sw")
    rust_target = target.removesuffix(".2.17").upper().replace("-", "_")
    return {f"{rust_target}_OPENSSL_DIR": str(prefix),
            f"{rust_target}_OPENSSL_STATIC": "1",
            f"{rust_target}_OPENSSL_NO_VENDOR": "1"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", choices=[*TARGETS, "universal-apple-darwin"])
    parser.add_argument("--output", type=Path, default=Path("target/release-openssl"))
    parser.add_argument("--env-file", type=Path, required=True)
    parser.add_argument("--jobs", type=int, default=min(os.cpu_count() or 1, 8))
    args = parser.parse_args()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=True)
    archive = root / f"openssl-{VERSION}.tar.gz"
    if not archive.exists():
        temporary = archive.with_suffix(f".{os.getpid()}.tmp")
        urllib.request.urlretrieve(URL, temporary)
        temporary.replace(archive)
    with archive.open("rb") as source:
        if hashlib.file_digest(source, "sha256").hexdigest() != SHA256:
            raise RuntimeError("OpenSSL source checksum mismatch")
    targets = (["x86_64-apple-darwin", "aarch64-apple-darwin"]
               if args.target == "universal-apple-darwin" else [args.target])
    variables = {}
    for target in targets:
        variables.update(build(target, root, archive, args.jobs))
    with args.env_file.open("a") as output:
        for key, value in variables.items():
            output.write(f"{key}={value}\n")


if __name__ == "__main__":
    main()
