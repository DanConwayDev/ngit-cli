#!/usr/bin/env python3
"""Mirror verified ngit archives to GitHub without rebuilding or replacing assets."""
import json
import os
from pathlib import Path
import re
import urllib.error
import urllib.parse
import urllib.request

from release_plan import plan

REPOSITORY = "DanConwayDev/ngit-cli"


class API:
    def __init__(self, token, read_only=False):
        if not token and not read_only:
            raise ValueError("GitHub writes require a token")
        self.token = token
        self.read_only = read_only

    def call(self, method, path, data=None, upload=False):
        if self.read_only and method != "GET":
            raise ValueError("GitHub mutation is forbidden during rehearsal")
        host = "uploads.github.com" if upload else "api.github.com"
        headers = {"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2026-03-10"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        if data is not None:
            if upload:
                headers["Content-Type"] = "application/octet-stream"
            else:
                data = json.dumps(data).encode()
                headers["Content-Type"] = "application/json"
        request = urllib.request.Request(f"https://{host}/repos/{REPOSITORY}/{path}", data=data, headers=headers, method=method)
        # API endpoints used here must not redirect authenticated requests.
        class NoRedirect(urllib.request.HTTPRedirectHandler):
            def redirect_request(self, req, fp, code, msg, headers, newurl):
                return None
        with urllib.request.build_opener(NoRedirect).open(request, timeout=180) as response:
            return json.load(response)


def promote_latest(api, release):
    if release["prerelease"]:
        return False
    def stable_version(tag):
        match = re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:\+[0-9A-Za-z.-]+)?", tag)
        if not match:
            raise ValueError(f"Cannot compare GitHub latest tag: {tag}")
        return tuple(map(int, match.groups()))
    candidate = stable_version(release["tag_name"])
    try:
        latest = api.call("GET", "releases/latest")
    except urllib.error.HTTPError as error:
        error.close()
        if error.code != 404:
            raise
        return True
    return candidate > stable_version(latest["tag_name"])


def mirror(api, release, dry_run=False):
    tag = urllib.parse.quote(release["tag_name"], safe="")
    ref = api.call("GET", f"git/ref/tags/{tag}")["object"]
    for _ in range(8):
        if ref["type"] == "commit":
            break
        if ref["type"] != "tag":
            raise ValueError("Mirror tag does not reference a commit")
        ref = api.call("GET", f'git/tags/{ref["sha"]}')["object"]
    if ref["type"] != "commit" or ref["sha"] != release["target_commitish"]:
        raise ValueError("GitHub tag does not match the verified source commit; sync Git refs first")
    existing = None
    for page in range(1, 21):
        rows = api.call("GET", f"releases?per_page=100&page={page}")
        existing = next((r for r in rows if r["tag_name"] == release["tag_name"]), None)
        if existing or len(rows) < 100:
            break
    else:
        raise ValueError("Release inventory exceeds scan bound")
    if existing is None and dry_run:
        return {"create_release": True, "upload": [a["name"] for a in release["assets"]],
                "make_latest": promote_latest(api, release)}
    if existing is None:
        metadata = {k: v for k, v in release.items() if k != "assets"}
        existing = api.call("POST", "releases", {**metadata, "draft": True, "make_latest": "false"})
    release_id = int(existing["id"])
    assets = api.call("GET", f"releases/{release_id}/assets?per_page=100")
    if len(assets) == 100:
        raise ValueError("Unexpectedly large release asset inventory")
    # Fail on conflicts before uploading any missing asset; never clobber bytes.
    by_name = {asset["name"]: asset for asset in assets}
    for asset in release["assets"]:
        old = by_name.get(asset["name"])
        if old and (old.get("state") != "uploaded" or old.get("digest") != f'sha256:{asset["sha256"]}'):
            raise ValueError(f'Existing GitHub asset differs or lacks a verified digest: {asset["name"]}')
    if dry_run:
        return {"create_release": False, "publish_draft": existing["draft"],
                "upload": [a["name"] for a in release["assets"] if a["name"] not in by_name],
                "make_latest": promote_latest(api, release)}
    for asset in release["assets"]:
        if asset["name"] in by_name:
            continue
        name = urllib.parse.quote(asset["name"], safe="")
        uploaded = api.call("POST", f"releases/{release_id}/assets?name={name}", asset["path"].read_bytes(), upload=True)
        if uploaded.get("state") != "uploaded" or uploaded.get("digest") != f'sha256:{asset["sha256"]}':
            raise ValueError(f'GitHub upload verification failed: {asset["name"]}')
    # Check immediately before publication while the workflow concurrency group
    # serializes our mirrors. Older retries and prereleases never move latest.
    latest = promote_latest(api, release)
    if existing["draft"]:
        api.call("PATCH", f"releases/{release_id}", {"draft": False, "make_latest": "true" if latest else "false"})
    elif latest:
        api.call("PATCH", f"releases/{release_id}", {"make_latest": "true"})
    print(f'https://github.com/{REPOSITORY}/releases/tag/{release["tag_name"]}')


def main():
    if os.environ.get("NGIT_CI_REF_TYPE") != "tag" or os.environ.get("NGIT_CI_TRIGGER") not in ("push", "manual"):
        raise ValueError("GitHub mirroring requires an authorized tag context")
    directory = Path(os.environ["NGIT_CI_INPUTS"]) / "prep.ready/artifacts/release-set"
    release = plan(directory, os.environ["NGIT_CI_REF_NAME"], os.environ["NGIT_CI_COMMIT_SHA"])
    token = (Path(os.environ["NGIT_CI_SECRETS_DIR"]) / "GH_RELEASE_TOKEN").read_text().strip()
    if not token:
        raise ValueError("GH_RELEASE_TOKEN is empty")
    mirror(API(token), release)


if __name__ == "__main__":
    main()
