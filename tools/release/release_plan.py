"""Validate one collected release set before either publisher reads secrets."""
import json
from pathlib import Path
import re
import tomllib

import archives


def plan(directory, tag, commit):
    version = tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]
    if tag != f"v{version}" or not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Release requires the exact package version tag and source commit")
    reports = json.loads((directory / "builds.json").read_text())
    targets = set(archives.TARGETS.values())
    if len(reports) != len(targets) or {r["target"] for r in reports} != targets:
        raise ValueError("Release requires the complete release set")
    assets = []
    for report in reports:
        name = archives.filename(report["target"])
        path = directory / name
        if (report.get("schema") != 1 or report["archive"] != name or report["commit"] != commit or report["version"] != version
                or report["sha256"] != archives.digest(path)):
            raise ValueError("Release input checksum or provenance mismatch")
        archives.verify_archive(path, report["target"])
        assets.append({"path": path, "name": name.replace("ngit-", f"ngit-{tag}-", 1), "sha256": report["sha256"]})
    match = re.search(r"^## \[" + re.escape(version) + r"\][^\n]*\n(.*?)(?=^## |\Z)", Path("CHANGELOG.md").read_text(), re.M | re.S)
    if not match:
        raise ValueError(f"Missing changelog section for {version}")
    return {"tag_name": tag, "target_commitish": commit, "name": tag,
            "body": match.group(1).strip(), "prerelease": "-" in version.split("+", 1)[0],
            "assets": assets}
