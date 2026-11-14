"""Sync a stable ngit release into the canonical documentation repository."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import urllib.request

from release_cli import environment
from release_plan import plan
from setup_ngit import BOT, Git, stable_version

REPOSITORY = "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/relay.ngit.dev/ngit-docs"

def allowed(path):
    return (path == "sources.lock.json" or path == "release/installer-promotion.json"
            or path in ("release/installer-templates/install.sh.in", "release/installer-templates/install.ps1.in",
                        "docs/public/install.sh", "docs/public/install.ps1")
            or path.startswith("content-imports/ngit/stable/")
            or (path.startswith("docs/") and path.endswith(".md")))


def verify_promotion(repo, version, commit, before):
    lock = json.loads((repo / "sources.lock.json").read_text())
    entry = lock["sources"]["ngit"]["stable"]
    if (entry["version"], entry["commit"]) != (version, commit):
        raise ValueError("Docs import differs from the verified release")
    # Other products and preview/upcoming imports are outside this job's scope.
    lock["sources"]["ngit"]["stable"] = before["sources"]["ngit"].get("stable")
    if lock != before:
        raise ValueError("Docs sync changed another source or channel")
    record = json.loads((repo / "release/installer-promotion.json").read_text())
    if record["source"]["commit"] != commit or record["release"]["version"] != version:
        raise ValueError("Installer promotion differs from the verified release")


def publish(git, observed, commit):
    current = git.ref("origin", "refs/heads/main")
    if current == commit:
        return
    if current != observed:
        raise ValueError("Documentation main changed during validation; retry from its new tip")
    git.run("push", "origin", f"{commit}:refs/heads/main")
    if git.ref("origin", "refs/heads/main") != commit:
        raise ValueError("Documentation push is incomplete")


def deployed(document, commit):
    runs = document.get("ci", {}).get("runs", [])
    return any(run.get("commit") == commit and run.get("workflow") == ".ngit/nix/workflows/docs.yaml"
               and run.get("conclusion") == "success" and run.get("classification") == "maintainer-directed"
               for run in runs)


def wait_for_site(repo, commit, env, timeout=600):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        cancel = env.get("NGIT_CI_CANCEL_FILE")
        if cancel and Path(cancel).exists():
            raise RuntimeError("Cancelled while waiting for docs publication")
        result = subprocess.run(["ngit", "--repo", REPOSITORY, "ci", "status", commit, "--json"],
                                cwd=repo, env=env, capture_output=True, text=True, timeout=90)
        if result.returncode == 0 and deployed(json.loads(result.stdout), commit):
            for name in ("install.sh", "install.ps1"):
                expected = hashlib.sha256((repo / "docs/public" / name).read_bytes()).digest()
                with urllib.request.urlopen(f"https://ngit.dev/{name}", timeout=30) as response:
                    actual = hashlib.sha256(response.read(2 * 1024 * 1024)).digest()
                if actual != expected:
                    break  # The gateway may still serve the preceding site.
            else:
                print(f"Documentation publication and live installers verified at {commit}")
                return
        time.sleep(min(15, max(0, deadline - time.monotonic())))
    raise TimeoutError(f"Docs commit {commit} was pushed; publication/live installer verification is pending. Retry update-docs.")


def update(version, root, env, preview=None, validate=None):
    env = {**env, **BOT}
    repo = root / "ngit-docs"
    subprocess.run(["git", "clone", "--branch", "main", "--single-branch", REPOSITORY, str(repo)],
                   env=env, check=True, timeout=600)
    git = Git(repo, env)
    observed = git.run("rev-parse", "HEAD")
    before = json.loads((repo / "sources.lock.json").read_text())
    def run(*args):
        subprocess.run(args, cwd=repo, env=env, check=True, timeout=1200)
    run("npm", "ci", "--ignore-scripts")
    run("npm", "run", "docs:sync:ngit", "--", "--ref", f"v{version}", "--stability", "stable")
    verify_promotion(repo, version, env["NGIT_CI_COMMIT_SHA"], before)
    if validate is not None:
        validate(repo)
    run("npm", "run", "nsite:build")
    git.run("diff", "--check")
    paths = set(git.run("diff", "--name-only", "-z").split("\0"))
    paths.update(git.run("ls-files", "--others", "--exclude-standard", "-z").split("\0"))
    paths.discard("")
    if any(not allowed(path) for path in paths):
        raise ValueError("Docs sync changed files outside generated release content")
    if paths:
        git.run("add", "--", *sorted(paths))
    if preview is not None:
        preview.mkdir(parents=True, exist_ok=True)
        patch = git.run("diff", "--cached", "--binary")
        (preview / "proposed.patch").write_text(patch + ("\n" if patch else ""))
        for name in ("install.sh", "install.ps1"):
            shutil.copyfile(repo / "docs/public" / name, preview / name)
        return
    if paths:
        git.run("-c", "core.hooksPath=/dev/null", "commit", "-m", f"docs: sync verified ngit {version}",
                "-m", "Import the signed stable release and its checked installer templates.")
    commit = git.run("rev-parse", "HEAD")
    secret = Path(os.environ["NGIT_CI_SECRETS_DIR"]).resolve() / "NGIT_PUBLISHER_NBUNKSEC"
    run("ngit", "--defaults", "account", "login", "--nbunksec-file", str(secret),
        "--secret-storage", "file", "--profile", "release-ci", "--json")
    git.run("config", "nostr.signer", "release-ci")
    publish(git, observed, commit)
    wait_for_site(repo, commit, env)


def main():
    version = stable_version(os.environ)
    if version is None:
        print("Prerelease: stable documentation and installers unchanged")
        return
    directory = Path(os.environ["NGIT_CI_INPUTS"]) / "prep.ready/artifacts/release-set"
    release = plan(directory, os.environ["NGIT_CI_REF_NAME"], os.environ["NGIT_CI_COMMIT_SHA"])
    with environment(release) as cli_env, tempfile.TemporaryDirectory(prefix="docs-release-", dir=".") as temporary:
        root = Path(temporary).resolve()
        home, tmp = root / "home", root / "tmp"
        home.mkdir(mode=0o700)
        tmp.mkdir(mode=0o700)
        env = {**cli_env, **BOT, "HOME": str(home), "TMPDIR": str(tmp),
               "XDG_CONFIG_HOME": str(home / "config"), "XDG_DATA_HOME": str(home / "data"),
               "XDG_CACHE_HOME": str(home / "cache"), "GIT_CONFIG_GLOBAL": str(home / "gitconfig"),
               "GIT_CONFIG_NOSYSTEM": "1", "GIT_TERMINAL_PROMPT": "0",
               "NGIT_DOCS_EXPORT_BIN": shutil.which("ngit", path=cli_env["PATH"]),
               "NGIT_DOCS_EXPORT_COMMIT": os.environ["NGIT_CI_COMMIT_SHA"]}
        update(version, root, env)


if __name__ == "__main__":
    main()
