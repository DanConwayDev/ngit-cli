"""Repair-code execution against an explicitly pinned, already published release."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.parse
import urllib.request

import archives
from release_cli import environment
import setup_ngit
import update_docs

spec = importlib.util.spec_from_file_location("github_mirror", Path(__file__).with_name("github-mirror.py"))
github = importlib.util.module_from_spec(spec)
spec.loader.exec_module(github)
AUTHOR = "a008def15796fba9a0d6fab04e8fd57089285d9fd505da5a83fe8aad57a3564d"
APP = f"32267:{AUTHOR}:ngit"
OPERATIONS = {"mirror", "update-gh-setup", "update-docs"}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def request(path=Path(".ngit/post-release.json")):
    value = json.loads(path.read_text())
    require(set(value) == {"schema", "version", "commit", "release_event", "operations"}, "Unexpected recovery request fields")
    require(value.get("schema") == 1, "Unsupported post-release request schema")
    require(isinstance(value.get("version"), str) and re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?", value["version"]), "Pin a release version in .ngit/post-release.json")
    for key, size in [("commit", 40), ("release_event", 64)]:
        require(isinstance(value.get(key), str) and re.fullmatch(f"[0-9a-f]{{{size}}}", value[key]), f"Pin the original release {key}")
    ops = value.get("operations")
    require(isinstance(ops, list) and ops and all(isinstance(op, str) for op in ops)
            and set(ops) <= OPERATIONS and len(set(ops)) == len(ops), "Choose unique post-release operations")
    require("-" not in value["version"] or ops == ["mirror"], "Prerelease recovery can only mirror GitHub assets")
    return value


def release_plan(document, target, directory):
    require(document.get("format_version") == 2 and document.get("command_status") == "ok"
            and document.get("command") == "release.view", "Invalid verified release response")
    result = document["result"]
    app, release, assets = result["application"], result["release"], result["assets"]
    require(app["coordinate"] == APP and app["trusted"] and app["fully_linked_to_current_repository"]
            and not app["validation"], "Release application is not trusted and linked")
    require(release["author"] == AUTHOR and release["application_coordinate"] == APP
            and release["event_id"] == target["release_event"] and release["version"] == target["version"]
            and release["commit"] == target["commit"] and not release["validation"], "Release identity differs from the pinned request")
    channel = target["version"].split("-", 1)[1].split(".", 1)[0] if "-" in target["version"] else "main"
    require(release["channel"] == channel, "Release channel differs from the requested version")
    require(len(assets) == 5 and len(release["asset_ids"]) == 5 and len(set(release["asset_ids"])) == 5
            and {a["event_id"] for a in assets} == set(release["asset_ids"])
            and not result["unresolved_asset_ids"], "Incomplete release inventory")
    expected = {archives.filename(t).replace("ngit-", f'ngit-v{target["version"]}-', 1): archives.filename(t)
                for t in archives.TARGETS.values()}
    require({a["filename"] for a in assets} == set(expected), "Unexpected release filenames")
    planned = []
    for asset in assets:
        require(asset["author"] == AUTHOR and asset["application_coordinate"] == APP
                and asset["version"] == target["version"] and asset["resolution"] == "resolved"
                and not asset["validation"], "Invalid signed release asset")
        sha = asset["sha256"]
        require(isinstance(sha, str) and re.fullmatch("[0-9a-f]{64}", sha), "Invalid asset checksum")
        verified = asset.get("verification", {})
        size = int(asset["size"])
        require(0 < size <= 512 * 1024 * 1024 and verified.get("ok") is True
                and verified.get("observed_sha256") == sha and int(verified.get("observed_size", -1)) == size,
                "Asset body has not been verified")
        url = urllib.parse.urlsplit(asset["url"])
        require(url.scheme == "https" and url.netloc and not url.username and not url.password
                and not url.query and not url.fragment
                and url.path.rsplit("/", 1)[-1] in (sha, sha + ".gz", sha + ".zip"), "Asset URL is not content-addressed HTTPS")
        planned.append({"name": asset["filename"], "path": directory / expected[asset["filename"]],
                        "sha256": sha, "size": size, "url": asset["url"]})
    return {"tag_name": f'v{target["version"]}', "target_commitish": target["commit"],
            "name": f'v{target["version"]}', "body": release.get("notes") or "",
            "prerelease": "-" in target["version"], "assets": planned}


def fetch_asset(asset):
    # Reject downgrade redirects and verify this download too, not only the
    # preceding ngit verification request. Never trust transport alone.
    with urllib.request.urlopen(asset["url"], timeout=120) as response:
        require(response.url.startswith("https://"), "Asset redirect left HTTPS")
        data = response.read(asset["size"] + 1)
    require(len(data) == asset["size"] and hashlib.sha256(data).hexdigest() == asset["sha256"],
            "Downloaded asset differs from the signed release")
    asset["path"].write_bytes(data)


def verify(target, output):
    output.mkdir(parents=True)
    with tempfile.TemporaryDirectory(prefix="recover-tag-", dir=".") as temporary:
        subprocess.run(["git", "init", temporary], check=True, stdout=subprocess.DEVNULL)
        git = setup_ngit.Git(Path(temporary), os.environ)
        tag = f'refs/tags/v{target["version"]}'
        git.run("fetch", "--quiet", "--no-tags", setup_ngit.PRODUCT, tag)
        require(git.run("rev-parse", "FETCH_HEAD^{commit}") == target["commit"], "Canonical product tag moved")
        require(git.run("cat-file", "-t", "FETCH_HEAD") == "tag", "Recovery requires an annotated product tag")
        response = subprocess.run(["ngit", "--repo", setup_ngit.PRODUCT, "release", "view",
                                   target["release_event"], "--verify", "--json"],
                                  stdout=subprocess.PIPE, text=True, timeout=900)
        if response.returncode:
            # JSON diagnostics are on stdout; preserve them in failed job logs.
            print(response.stdout, file=sys.stderr)
            response.check_returncode()
        document = json.loads(response.stdout)
        release = release_plan(document, target, output)
        for asset in release["assets"]:
            fetch_asset(asset)
        git.run("fetch", "--quiet", "--no-tags", setup_ngit.PRODUCT, tag)
        require(git.run("rev-parse", "FETCH_HEAD^{commit}") == target["commit"], "Canonical tag changed during verification")
    (output / "verified-release.json").write_text(json.dumps(document, indent=2) + "\n")
    (output / "request.json").write_text(json.dumps(target, indent=2) + "\n")


def validate_consumer(repo, operation, target, release):
    expected = {asset["name"]: asset["sha256"] for asset in release["assets"]}
    if operation == "update-gh-setup":
        rows = [line.split("|") for line in (repo / "manifest.txt").read_text().splitlines()
                if line.startswith(f'asset|{target["version"]}|')]
        actual = {row[3]: row[4] for row in rows}
        require(len(rows) == len(expected), "Setup manifest changed the original asset inventory")
    else:
        record = json.loads((repo / "release/installer-promotion.json").read_text())
        require(record["release"]["event_id"] == target["release_event"], "Docs selected a different signed release event")
        actual = {asset["filename"]: asset["sha256"] for asset in record["assets"].values()}
    require(actual == expected, "Consumer selected different bytes from the pinned release")


def operate(target, release, operation, apply, output):
    output.mkdir(parents=True, exist_ok=True)
    summary = {"repair_commit": os.environ["NGIT_CI_COMMIT_SHA"], "release": target, "operation": operation,
               "mode": "apply" if apply else "rehearsal"}
    if operation not in target["operations"]:
        (output / "result.json").write_text(json.dumps({**summary, "skipped": True}, indent=2))
        return
    if operation == "mirror":
        secret = Path(os.environ["NGIT_CI_SECRETS_DIR"]) / "GH_RELEASE_TOKEN"
        token = secret.read_text().strip()
        require(bool(token), "GH_RELEASE_TOKEN is empty")
        # Do not pass recovery-only URL/size fields to the GitHub create API.
        proposal = github.mirror(github.API(token, read_only=not apply), release, dry_run=not apply)
        summary["github_plan"] = proposal
    else:
        with environment(release) as cli_env, tempfile.TemporaryDirectory(prefix="post-release-", dir=".") as temporary:
            root = Path(temporary).resolve()
            home, tmp = root / "home", root / "tmp"
            home.mkdir(mode=0o700)
            tmp.mkdir(mode=0o700)
            env = {**cli_env, **setup_ngit.BOT, "HOME": str(home), "TMPDIR": str(tmp),
                   "XDG_CONFIG_HOME": str(home / "config"), "XDG_DATA_HOME": str(home / "data"),
                   "XDG_CACHE_HOME": str(home / "cache"), "GIT_CONFIG_GLOBAL": str(home / "gitconfig"),
                   "GIT_CONFIG_NOSYSTEM": "1", "GIT_TERMINAL_PROMPT": "0",
                   "NGIT_RELEASE_PREVIEW": "0" if apply else "1", "LC_ALL": "C",
                   # Consumer verification compares against the original source,
                   # not against the newer repair commit in the allocation.
                   "NGIT_CI_COMMIT_SHA": target["commit"], "NGIT_DOCS_EXPORT_COMMIT": target["commit"],
                   "NGIT_DOCS_EXPORT_BIN": shutil.which("ngit", path=cli_env["PATH"])}
            if apply and operation == "update-gh-setup":
                askpass = root / "askpass"
                setup_ngit.write_askpass(askpass)
                env.update(GIT_ASKPASS=str(askpass), RELEASE_TOKEN_FILE=str(Path(os.environ["NGIT_CI_SECRETS_DIR"]).resolve() / "GH_RELEASE_TOKEN"))
            updater = setup_ngit.update if operation == "update-gh-setup" else update_docs.update
            updater(target["version"], root, env, preview=None if apply else output,
                    validate=lambda repo: validate_consumer(repo, operation, target, release))
    (output / "result.json").write_text(json.dumps(summary, indent=2) + "\n")


def main():
    require(os.environ.get("NGIT_CI_TRIGGER") == "manual", "Post-release repair requires a manual trigger")
    target = request()
    operation = os.environ["NGIT_CI_JOB_ID"]
    output = Path(os.environ["NGIT_CI_OUTPUT_DIR"]) / "post-release"
    if operation == "verify":
        verify(target, output)
        return
    require(operation in OPERATIONS, "Unknown post-release operation")
    apply = sys.argv[1:] == ["--apply"]
    require(sys.argv[1:] in ([], ["--apply"]), "Unexpected recovery arguments")
    require(not apply or os.environ.get("NGIT_CI_WORKFLOW_PATH") == ".ngit/nix/workflows/post-release-apply.yaml",
            "Apply is only permitted in the explicit recovery workflow")
    if apply:
        require(os.environ.get("NGIT_CI_REF") == "refs/heads/main", "Apply requires the reviewed main ref")
        rows = subprocess.check_output(["git", "ls-remote", setup_ngit.PRODUCT, "refs/heads/main"],
                                       text=True, timeout=120).splitlines()
        require(len(rows) == 1 and rows[0].split() == [os.environ["NGIT_CI_COMMIT_SHA"], "refs/heads/main"],
                "Apply requires the current canonical main commit")
    source = Path(os.environ["NGIT_CI_INPUTS"]) / "verify/artifacts/release"
    require(json.loads((source / "request.json").read_text()) == target, "Recovery input request mismatch")
    release = release_plan(json.loads((source / "verified-release.json").read_text()), target, source)
    for asset in release["assets"]:
        require(asset["path"].stat().st_size == asset["size"] and archives.digest(asset["path"]) == asset["sha256"],
                "Recovery artifact differs from the original signed bytes")
    operate(target, release, operation, apply, output)


if __name__ == "__main__":
    main()
