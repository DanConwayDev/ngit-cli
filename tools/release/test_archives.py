"""Exercise archive boundaries and cross-provider provenance without compilers."""
import io
import json
from pathlib import Path
import struct
import tarfile
import tempfile
import unittest

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


if __name__ == "__main__":
    unittest.main()
