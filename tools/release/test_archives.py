"""Exercise archive boundaries and cross-provider provenance without compilers."""
import io
import json
from pathlib import Path
import struct
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import archives


def elf(machine=62):
    data = bytearray(64)
    data[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", data, 18, machine)
    return bytes(data) + b"OpenSSL 3.5.9\0"


class Archives(unittest.TestCase):
    def test_wrong_architecture(self):
        with self.assertRaisesRegex(ValueError, "architecture"):
            archives.inspect_binary(elf(62), "aarch64-unknown-linux-gnu.2.17")

    def test_archive_rejects_symlink(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "archive.tar.gz"
            with tarfile.open(path, "w:gz") as out:
                for name in ["ngit", "git-remote-nostr"]:
                    item = tarfile.TarInfo(name)
                    item.type, item.linkname, item.mode = tarfile.SYMTYPE, "/etc/passwd", 0o755
                    out.addfile(item)
            with self.assertRaisesRegex(ValueError, "members"):
                archives.verify_archive(path, "x86_64-unknown-linux-gnu.2.17")

    def test_collect_rejects_wrong_commit_or_corrupt_download(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            target = archives.TARGETS["linux-x86_64"]
            inputs = root / "inputs"
            job = inputs / "linux-x86_64" / "artifacts"
            (job / "archive").mkdir(parents=True)
            (job / "report").mkdir()
            path = job / "archive" / archives.filename(target)
            path.write_bytes(b"archive")
            report = dict(schema=1, commit="a" * 40, target=target, archive=path.name, sha256=archives.digest(path))
            report_path = job / "report" / "build.json"
            report_path.write_text(json.dumps(report))
            with self.assertRaisesRegex(ValueError, "provenance"):
                archives.collect(inputs, root / "output", "b" * 40)
            path.write_bytes(b"corrupt")
            with self.assertRaisesRegex(ValueError, "checksum"):
                archives.collect(inputs, root / "output", "a" * 40)
            self.assertFalse((root / "output").exists())

    def test_duplicate_archive_members_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "archive.tar.gz"
            with tarfile.open(path, "w:gz") as out:
                for name in ["ngit", "ngit", "git-remote-nostr"]:
                    item = tarfile.TarInfo(name)
                    item.size, item.mode = len(elf()), 0o755
                    out.addfile(item, io.BytesIO(elf()))
            with self.assertRaisesRegex(ValueError, "members"):
                archives.verify_archive(path, "x86_64-unknown-linux-gnu.2.17")

    def test_linkage_rejects_new_glibc(self):
        with patch.object(archives.subprocess, "check_output", side_effect=["", "", "Name: GLIBC_2.28"]):
            with self.assertRaisesRegex(ValueError, "newer than"):
                archives.inspect_linkage(Path("ngit"), "x86_64-unknown-linux-gnu.2.17")

    def test_linkage_rejects_nix_runtime(self):
        with patch.object(archives.subprocess, "check_output", side_effect=["RUNPATH /nix/store/host/lib", ""]):
            with self.assertRaisesRegex(ValueError, "Nix store"):
                archives.inspect_linkage(Path("ngit"), "x86_64-unknown-linux-gnu.2.17")

    def test_musl_rejects_dynamic_loader(self):
        with patch.object(archives.subprocess, "check_output", side_effect=["", "INTERP"]):
            with self.assertRaisesRegex(ValueError, "static"):
                archives.inspect_linkage(Path("ngit"), "x86_64-unknown-linux-musl")

    def test_collect_preserves_exact_verified_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            version = archives.tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]
            for job, target in archives.TARGETS.items():
                folder = root / "inputs" / job / "artifacts"
                (folder / "archive").mkdir(parents=True)
                (folder / "report").mkdir()
                name = archives.filename(target)
                path = folder / "archive" / name
                path.write_bytes(target.encode())
                report = dict(schema=1, commit="a" * 40, target=target, archive=name, version=version, sha256=archives.digest(path))
                (folder / "report" / "build.json").write_text(json.dumps(report))
            with patch.object(archives, "verify_archive") as verify:
                archives.collect(root / "inputs", root / "output", "a" * 40)
                self.assertEqual(verify.call_count, 5)
            for target in archives.TARGETS.values():
                self.assertEqual((root / "output" / archives.filename(target)).read_bytes(), target.encode())
            self.assertEqual(len((root / "output" / "SHA256SUMS").read_text().splitlines()), 5)

    def test_pack_does_not_encode_build_time(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binaries = root / "bin"
            binaries.mkdir()
            (binaries / "ngit").write_bytes(elf())
            (binaries / "git-remote-nostr").write_bytes(elf()[:64])
            target = "x86_64-unknown-linux-gnu.2.17"
            with patch.object(archives, "inspect_linkage"), patch.object(archives.subprocess, "run"), patch.object(archives.subprocess, "check_output", return_value="rustc test"), patch.dict(archives.os.environ, {"NGIT_CI_COMMIT_SHA": "a" * 40}):
                archives.pack(target, binaries, root / "one")
                archives.os.utime(binaries / "ngit", (100, 100))
                archives.pack(target, binaries, root / "two")
            name = archives.filename(target)
            self.assertEqual((root / "one" / name).read_bytes(), (root / "two" / name).read_bytes())


if __name__ == "__main__":
    unittest.main()
