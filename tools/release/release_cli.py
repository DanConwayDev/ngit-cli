"""Run publication with the verified static Linux CLI from the release set."""
from contextlib import contextmanager
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile


@contextmanager
def environment(release):
    # plan() has already validated all archives and their source/checksums.
    asset = next(a for a in release["assets"] if a["path"].name == "ngit-x86_64-unknown-linux-musl.tar.gz")
    with tempfile.TemporaryDirectory(prefix="ngit-release-cli-") as temporary:
        directory = Path(temporary)
        with tarfile.open(asset["path"]) as archive:
            for name in ("ngit", "git-remote-nostr"):
                member = archive.getmember(name)
                if not member.isfile():
                    raise ValueError("Publication CLI must be a regular archive member")
                with archive.extractfile(member) as source, (directory / name).open("wb") as output:
                    shutil.copyfileobj(source, output)
                (directory / name).chmod(0o755)
        env = {**os.environ, "PATH": f'{directory}{os.pathsep}{os.environ.get("PATH", "")}'}
        subprocess.run([str(directory / "ngit"), "--version"], env=env, check=True, timeout=30)
        subprocess.run([str(directory / "ngit"), "release", "publish", "--help"],
                       env=env, stdout=subprocess.DEVNULL, check=True, timeout=30)
        yield env
