"""Approve and preserve the exact archive set in a manual pre-tag rehearsal."""
import os
from pathlib import Path
import shutil
import tomllib

from release_plan import plan
from release_cli import environment


def rehearse(env):
    # Automatic dependency admission must never create release approval. A tag
    # without a compatible retained rehearsal fails closed at this job.
    if env.get("NGIT_CI_TRIGGER") != "manual" or env.get("NGIT_CI_REF_TYPE") != "branch":
        raise ValueError("Run the manual release rehearsal on the exact branch commit before tagging")
    source = Path(env["NGIT_CI_INPUTS"]) / "archives.collect/artifacts/release-set"
    version = tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]
    release = plan(source, f"v{version}", env["NGIT_CI_COMMIT_SHA"])
    with environment(release):
        destination = Path(env["NGIT_CI_OUTPUT_DIR"]) / "release"
        # Copy the checked archive bytes; never rebuild or repack here.
        shutil.copytree(source, destination)
    print(f"Release rehearsal approved {env['NGIT_CI_COMMIT_SHA']}")


if __name__ == "__main__":
    rehearse(os.environ)
