#!/usr/bin/env python3
"""Publish the verified ngit-ci archive set to Nostr and Blossom."""
import os
from pathlib import Path
import shutil
import subprocess

from release_plan import plan

REPOSITORY = "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/relay.ngit.dev/ngit"


def prepare():
    if os.environ.get("NGIT_CI_TRIGGER") != "push" or os.environ.get("NGIT_CI_REF_TYPE") != "tag":
        raise ValueError("Release publication requires a tag push")
    tag, commit = os.environ["NGIT_CI_REF_NAME"], os.environ["NGIT_CI_COMMIT_SHA"]
    ref = os.environ["NGIT_CI_REF"]
    if ref != f"refs/tags/{tag}":
        raise ValueError("Release tag context is inconsistent")
    directory = Path(os.environ["NGIT_CI_INPUTS"]) / "archives.collect/artifacts/release-set"
    release = plan(directory, tag, commit)
    # Provider checkouts contain the allocated commit, but omit tag objects.
    # Fetch just the release tag and reject a moved or lightweight tag.
    subprocess.run(["git", "fetch", "--no-tags", "--depth=1", REPOSITORY, f"{ref}:{ref}"], check=True, timeout=90)
    def git(*args):
        return subprocess.check_output(["git", *args], text=True, timeout=30).strip()
    if git("rev-parse", "HEAD") != commit or git("rev-parse", f"{ref}^{{commit}}") != commit:
        raise ValueError("Release tag or checkout does not match the artifact commit")
    if git("cat-file", "-t", ref) != "tag":
        raise ValueError("Release requires an annotated tag")
    released_at = git("for-each-ref", "--format=%(creatordate:unix)", ref)
    if not released_at.isdigit() or int(released_at) <= 0:
        raise ValueError("Missing release tag date")
    version = tag[1:]
    channel = version.split("-", 1)[1].split(".", 1)[0].split("+", 1)[0] if "-" in version else "main"
    return release, version, channel, released_at


def main():
    release, version, channel, released_at = prepare()
    secret = Path(os.environ["NGIT_CI_SECRETS_DIR"]) / "NGIT_PUBLISHER_NBUNKSEC"
    if not secret.read_text().strip():
        raise ValueError("NGIT_PUBLISHER_NBUNKSEC is empty")
    if os.environ.get("NGIT_CI_CANCEL_FILE") and Path(os.environ["NGIT_CI_CANCEL_FILE"]).exists():
        raise RuntimeError("Cancelled before publication")
    output = Path("artifacts")
    output.mkdir(exist_ok=True)
    for asset in release["assets"]:
        shutil.copyfile(asset["path"], output / asset["name"])
    subprocess.run([
        "ngit", "--repo", REPOSITORY, "--nbunksec-file", str(secret), "--defaults", "--json",
        "release", "publish", version, "--manifest", ".ngit/release.yaml",
        "--channel", channel, "--tag", release["tag_name"],
        "--commit", release["target_commitish"], "--released-at", released_at,
    ], check=True)


if __name__ == "__main__":
    main()
