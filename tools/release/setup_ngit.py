"""Promote verified stable release pins to setup-ngit's canonical and mirror refs."""
import os
from pathlib import Path
import re
import subprocess
import tempfile

from release_cli import environment
from release_plan import plan

PRODUCT = "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/relay.ngit.dev/ngit"

CANONICAL = "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/setup-ngit"
MIRROR = "https://github.com/DanConwayDev/setup-ngit.git"


BOT = {"GIT_AUTHOR_NAME": "ngit Release Bot", "GIT_AUTHOR_EMAIL": "release-bot@ngit.dev",
       "GIT_COMMITTER_NAME": "ngit Release Bot", "GIT_COMMITTER_EMAIL": "release-bot@ngit.dev"}


def stable_version(env):
    if env.get("NGIT_CI_TRIGGER") not in ("push", "manual") or env.get("NGIT_CI_REF_TYPE") != "tag":
        raise ValueError("setup-ngit publication requires an authorized tag context")
    tag = env["NGIT_CI_REF_NAME"]
    if env.get("NGIT_CI_REF") != f"refs/tags/{tag}":
        raise ValueError("Release tag context is inconsistent")
    if not re.fullmatch(r"v(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)", tag):
        if re.fullmatch(r"v\d+\.\d+\.\d+-[0-9A-Za-z.-]+", tag):
            return None
        raise ValueError("Expected a final vX.Y.Z tag")
    return tag[1:]


class Git:
    def __init__(self, directory, env):
        self.directory, self.env = directory, env

    def run(self, *args):
        if "push" in args and self.env.get("NGIT_RELEASE_PREVIEW") == "1":
            raise RuntimeError("Publication is forbidden during rehearsal")
        cancel = self.env.get("NGIT_CI_CANCEL_FILE")
        if "push" in args and cancel and Path(cancel).exists():
            raise RuntimeError("Cancelled before setup-ngit publication")
        return subprocess.check_output(["git", "-c", "credential.helper=", *args],
                                       cwd=self.directory, env=self.env, text=True, timeout=600).strip()

    def ref(self, remote, ref):
        rows = self.run("ls-remote", "--refs", remote, ref).splitlines()
        if not rows:
            return ""
        if len(rows) != 1 or rows[0].split()[1] != ref:
            raise ValueError(f"Ambiguous remote ref: {remote} {ref}")
        return rows[0].split()[0]


def promote(git, commit, major, observed):
    """Main must be visible on both remotes before either floating tag moves."""
    tag = f"refs/tags/v{major}"
    for remote in ("origin", "github"):
        current = git.ref(remote, "refs/heads/main")
        if current == commit:
            continue  # A prior attempt or Nostr replication already did this.
        if current != observed[remote]["main"]:
            raise ValueError(f"{remote} main changed during validation")
        # Ordinary fast-forward semantics also reject divergent mirrors.
        git.run("push", remote, f"{commit}:refs/heads/main")
    for remote in ("origin", "github"):
        if git.ref(remote, "refs/heads/main") != commit:
            raise ValueError(f"{remote} main is not the verified release commit")
    for remote in ("origin", "github"):
        current = git.ref(remote, tag)
        if current == commit:
            continue
        if current != observed[remote]["tag"]:
            raise ValueError(f"{remote} floating tag changed during validation")
        git.run("push", f"--force-with-lease={tag}:{current}", remote, f"{commit}:{tag}")
    for remote in ("origin", "github"):
        if git.ref(remote, "refs/heads/main") != commit or git.ref(remote, tag) != commit:
            raise ValueError(f"{remote} promotion incomplete; retry the release job")


def update(version, root, env, preview=None, validate=None):
    env = {**env, **BOT}
    repo = root / "setup-ngit"
    subprocess.run(["git", "clone", "--branch", "main", "--single-branch", CANONICAL, str(repo)],
                   env=env, check=True, timeout=600)
    git = Git(repo, env)
    git.run("remote", "add", "github", MIRROR)
    major = version.split(".")[0]
    observed = {remote: {"main": git.ref(remote, "refs/heads/main"),
                         "tag": git.ref(remote, f"refs/tags/v{major}")}
                for remote in ("origin", "github")}
    if observed["origin"]["main"] != git.run("rev-parse", "HEAD"):
        raise ValueError("Canonical main changed during clone")
    # Fetch mirror ancestry before any writes so an unrelated mirror cannot
    # leave a half-published canonical update.
    if observed["github"]["main"]:
        git.run("fetch", "--no-tags", "github", "refs/heads/main")
        git.run("merge-base", "--is-ancestor", observed["github"]["main"], "HEAD")
    for remote in ("origin", "github"):
        if observed[remote]["tag"]:
            git.run("fetch", "--no-tags", remote, f"refs/tags/v{major}")
            git.run("merge-base", "--is-ancestor", observed[remote]["tag"], "HEAD")
    def verify_source():
        git.run("fetch", "--no-tags", PRODUCT, f"refs/tags/v{version}")
        if git.run("rev-parse", "FETCH_HEAD^{commit}") != env["NGIT_CI_COMMIT_SHA"]:
            raise ValueError("Canonical release tag differs from the job artifact commit")
    verify_source()
    def run(*args, extra=None):
        subprocess.run(args, cwd=repo, env={**env, **(extra or {})}, check=True, timeout=900)
    run("node", "scripts/sync-release.mjs", version)
    if validate is not None:
        validate(repo)
    run("node", "--test", *[str(p.relative_to(repo)) for p in sorted((repo / "scripts").glob("*.test.mjs"))])
    run("bash", "scripts/install.sh", extra={"NGIT_SETUP_VERSION": version,
        "NGIT_SETUP_TARGET": "linux-x86_64-musl", "RUNNER_TOOL_CACHE": str(root / "tool-cache"),
        "GITHUB_PATH": str(root / "action-path"), "GITHUB_OUTPUT": str(root / "action-output"),
        "GITHUB_ACTION_PATH": str(repo)})
    verify_source()
    git.run("diff", "--check")
    changes = git.run("status", "--porcelain")
    if changes:
        if changes != "M manifest.txt":
            raise ValueError("setup-ngit validation changed files other than manifest.txt")
        git.run("add", "manifest.txt")
    if preview is not None:
        preview.mkdir(parents=True, exist_ok=True)
        patch = git.run("diff", "--cached", "--binary")
        (preview / "proposed.patch").write_text(patch + ("\n" if patch else ""))
        (preview / "manifest.txt").write_bytes((repo / "manifest.txt").read_bytes())
        return
    if changes:
        git.run("-c", "core.hooksPath=/dev/null", "commit",
                "-m", f"chore: pin setup-ngit to verified ngit {version}",
                "-m", "Verify canonical signed release assets before updating both distribution refs.")
    commit = git.run("rev-parse", "HEAD")
    # Keep credentials out of the checkout and command line; the profile is
    # stored only in this job's disposable HOME, never a personal login.
    secret = Path(os.environ["NGIT_CI_SECRETS_DIR"]).resolve() / "NGIT_PUBLISHER_NBUNKSEC"
    run("ngit", "--defaults", "account", "login", "--nbunksec-file", str(secret),
        "--secret-storage", "file", "--profile", "release-ci", "--json")
    git.run("config", "nostr.signer", "release-ci")
    promote(git, commit, major, observed)
    print(f"setup-ngit main and v{major} published on Nostr and GitHub at {commit}")


def write_askpass(askpass):
    askpass.write_text('#!/usr/bin/env python3\nimport os, sys\nfrom pathlib import Path\n'
                       'if not any(host in sys.argv[1] for host in ("https://github.com\'", "https://x-access-token@github.com\'")): sys.exit(1)\n'
                       'print("x-access-token" if "Username" in sys.argv[1] else Path(os.environ["RELEASE_TOKEN_FILE"]).read_text().strip())\n')
    askpass.chmod(0o700)


def main():
    version = stable_version(os.environ)
    if version is None:
        print("Prerelease: setup-ngit stable pins unchanged")
        return
    directory = Path(os.environ["NGIT_CI_INPUTS"]) / "rehearsal.ready/artifacts/release-set"
    release = plan(directory, os.environ["NGIT_CI_REF_NAME"], os.environ["NGIT_CI_COMMIT_SHA"])
    with environment(release) as cli_env, tempfile.TemporaryDirectory(prefix="setup-release-", dir=".") as temporary:
        root = Path(temporary).resolve()
        home = root / "home"
        home.mkdir(mode=0o700)
        token = Path(os.environ["NGIT_CI_SECRETS_DIR"]).resolve() / "GH_RELEASE_TOKEN"
        if not token.read_text().strip():
            raise ValueError("GH_RELEASE_TOKEN is empty")
        askpass = root / "askpass"
        write_askpass(askpass)
        env = {**cli_env, **BOT, "HOME": str(home), "XDG_CONFIG_HOME": str(home / "config"),
               "XDG_DATA_HOME": str(home / "data"), "XDG_CACHE_HOME": str(home / "cache"),
               "GIT_CONFIG_GLOBAL": str(home / "gitconfig"), "GIT_CONFIG_NOSYSTEM": "1",
               "GIT_TERMINAL_PROMPT": "0", "GIT_ASKPASS": str(askpass), "LC_ALL": "C",
               "RELEASE_TOKEN_FILE": str(token)}
        update(version, root, env)


if __name__ == "__main__":
    main()
