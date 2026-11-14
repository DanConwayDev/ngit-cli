#!/usr/bin/env python3
"""Exercise publication input preparation on PRs, without a tag or secrets."""
import os
from pathlib import Path
import tomllib

from release_plan import plan
from release_cli import environment

version = tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]
release = plan(Path(os.environ["NGIT_CI_INPUTS"]) / "collect/artifacts/release-set",
               f"v{version}", os.environ["NGIT_CI_COMMIT_SHA"])
with environment(release):
    print(f'Validated {len(release["assets"])} publication inputs without credentials')
