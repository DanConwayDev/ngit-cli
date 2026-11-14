"""Exercise the publication CLI's isolation and cleanup boundaries."""
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import release_cli


class ReleaseCLI(unittest.TestCase):
    def fixture(self, root, symlink=False):
        path = root / "ngit-x86_64-unknown-linux-musl.tar.gz"
        with tarfile.open(path, "w:gz") as archive:
            for name in ("ngit", "git-remote-nostr"):
                member = tarfile.TarInfo(name)
                data = f"verified {name}".encode()
                member.size = len(data)
                if symlink and name == "ngit":
                    member.type = tarfile.SYMTYPE
                    member.linkname = "/bin/false"
                    archive.addfile(member)
                else:
                    archive.addfile(member, io.BytesIO(data))
        return {"assets": [{"path": path}]}

    def test_exact_binaries_scoped_path_and_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            release = self.fixture(Path(temporary))
            original_path = os.environ.get("PATH")
            with patch.object(release_cli.subprocess, "run") as run:
                with release_cli.environment(release) as env:
                    directory = Path(env["PATH"].split(os.pathsep)[0])
                    for name in ("ngit", "git-remote-nostr"):
                        self.assertEqual((directory / name).read_bytes(), f"verified {name}".encode())
                        self.assertEqual((directory / name).stat().st_mode & 0o777, 0o755)
                    self.assertEqual(os.environ.get("PATH"), original_path)
                    self.assertEqual(run.call_args_list[0].args[0], [str(directory / "ngit"), "--version"])
                    self.assertEqual(run.call_args_list[1].args[0], [str(directory / "ngit"), "release", "publish", "--help"])
                    self.assertTrue(all(c.kwargs["env"] == env for c in run.call_args_list))
                self.assertFalse(directory.exists())

    def test_failed_cli_check_cleans_up_before_yield(self):
        with tempfile.TemporaryDirectory() as temporary:
            release = self.fixture(Path(temporary))
            seen = []
            def fail(args, **kwargs):
                seen.append(Path(args[0]).parent)
                raise subprocess.CalledProcessError(1, args)
            with patch.object(release_cli.subprocess, "run", side_effect=fail), self.assertRaises(subprocess.CalledProcessError):
                with release_cli.environment(release):
                    self.fail("A broken CLI must not reach publication")
            self.assertEqual(len(seen), 1)
            self.assertFalse(seen[0].exists())

    def test_symlink_member_never_executes(self):
        with tempfile.TemporaryDirectory() as temporary:
            release = self.fixture(Path(temporary), symlink=True)
            with patch.object(release_cli.subprocess, "run") as run, self.assertRaisesRegex(ValueError, "regular"):
                with release_cli.environment(release):
                    self.fail("A symlink must not reach publication")
            run.assert_not_called()
