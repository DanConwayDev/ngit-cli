"""Packaging regressions; no network or native toolchain required."""

import importlib.util
from pathlib import Path
import tempfile
import tarfile
import io
import struct
import unittest
from unittest.mock import patch
import zipfile

spec = importlib.util.spec_from_file_location("verify", Path(__file__).with_name("verify-openssl.py"))
verify = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verify)


class SourceTests(unittest.TestCase):
    def test_bad_source_checksum_stops_before_build(self):
        spec = importlib.util.spec_from_file_location("build", Path(__file__).with_name("build-openssl.py"))
        build = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(build)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / f"openssl-{build.VERSION}.tar.gz").write_bytes(b"corrupted download")
            env = root / "env"
            with patch("sys.argv", ["build-openssl.py", "x86_64-unknown-linux-musl",
                                    "--output", str(root), "--env-file", str(env)]), patch.object(build, "build") as native:
                with self.assertRaisesRegex(RuntimeError, "checksum mismatch"):
                    build.main()
                native.assert_not_called()
            self.assertFalse(env.exists())


class ArchiveTests(unittest.TestCase):
    def check_archive(self, ngit, helper=b"launcher", include_helper=True):
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / "release.tar.gz"
            with tarfile.open(archive, "w:gz") as output:
                entries = {"ngit": ngit}
                if include_helper:
                    entries["git-remote-nostr"] = helper
                for name, data in entries.items():
                    member = tarfile.TarInfo(name)
                    member.size = len(data)
                    output.addfile(member, io.BytesIO(data))
            verify.verify(archive)

    def test_windows_uses_native_crypto(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / "release.zip"
            with zipfile.ZipFile(archive, "w") as output:
                output.writestr("ngit.exe", b"WinHTTP and WinCNG")
                output.writestr("git-remote-nostr.exe", b"launcher")
            verify.verify(archive)

    def universal(self, versions):
        slices = [b"\xcf\xfa\xed\xfe" + b"OpenSSL Default Provider\0" + version + b"\0"
                  for version in versions]
        offset = 48
        header = b"\xca\xfe\xba\xbe" + struct.pack(">I", 2)
        header += struct.pack(">iiIII", 0x01000007, 3, offset, len(slices[0]), 0)
        header += struct.pack(">iiIII", 0x0100000c, 0, offset + len(slices[0]), len(slices[1]), 0)
        return header + b"".join(slices)

    def test_crypto_only_universal_pair(self):
        self.check_archive(self.universal([b"3.5.9", b"3.5.9"]))

    def test_one_fixed_slice_cannot_hide_stale_slice(self):
        with self.assertRaises(ValueError):
            self.check_archive(self.universal([b"3.5.9", b"3.6.3"]))

    def test_fixed_pair(self):
        self.check_archive(b"binary\x00OpenSSL 3.5.9 29 Sep 2026\x00")

    def test_vulnerable_ngit_rejected(self):
        with self.assertRaises(ValueError):
            self.check_archive(b"OpenSSL 3.6.3 9 Jun 2026")

    def test_mixed_versions_rejected(self):
        with self.assertRaises(ValueError):
            self.check_archive(b"OpenSSL 3.5.9\x00OpenSSL 3.6.3\x00")

    def test_dynamic_or_missing_ssl_rejected(self):
        with self.assertRaises(ValueError):
            self.check_archive(b"no static OpenSSL")

    def test_stale_helper_rejected(self):
        with self.assertRaises(ValueError):
            self.check_archive(b"OpenSSL 3.5.9", b"OpenSSL 3.6.3")

    def test_missing_helper_rejected(self):
        with self.assertRaises(ValueError):
            self.check_archive(b"OpenSSL 3.5.9", include_helper=False)


if __name__ == "__main__":
    unittest.main()
