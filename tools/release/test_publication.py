"""Publication boundaries: validate source and artifacts before any signing."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import tomllib
import unittest
from unittest.mock import patch

import archives
import release_plan

spec = importlib.util.spec_from_file_location("publisher", Path(__file__).with_name("publish-release.py"))
publisher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publisher)


class Publication(unittest.TestCase):
    def fixture(self, directory):
        version = tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]
        reports = []
        for target in archives.TARGETS.values():
            name = archives.filename(target)
            path = directory / name
            path.write_bytes(target.encode())
            reports.append(dict(schema=1, commit="a" * 40, version=version, target=target,
                                archive=name, sha256=archives.digest(path)))
        (directory / "builds.json").write_text(json.dumps(reports))
        return version

    def test_plan_rejects_mixed_commit_corrupt_or_incomplete_set(self):
        for mutation in ["commit", "corrupt", "missing"]:
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                version = self.fixture(root)
                reports = json.loads((root / "builds.json").read_text())
                if mutation == "commit":
                    reports[-1]["commit"] = "b" * 40
                elif mutation == "corrupt":
                    (root / reports[-1]["archive"]).write_bytes(b"corrupt")
                else:
                    reports.pop()
                (root / "builds.json").write_text(json.dumps(reports))
                with patch.object(archives, "verify_archive"), self.assertRaises(ValueError):
                    release_plan.plan(root, f"v{version}", "a" * 40)

    def test_invalid_context_never_fetches_or_publishes(self):
        for trigger, ref_type in [("pull_request", "branch"), ("manual", "tag"), ("push", "branch")]:
            with self.subTest(trigger=trigger, ref_type=ref_type), patch.dict(os.environ, dict(NGIT_CI_TRIGGER=trigger, NGIT_CI_REF_TYPE=ref_type), clear=True), patch.object(publisher.subprocess, "run") as run:
                with self.assertRaisesRegex(ValueError, "tag push"):
                    publisher.main()
                run.assert_not_called()

    def test_moved_tag_stops_before_secret_or_publication(self):
        env = dict(NGIT_CI_TRIGGER="push", NGIT_CI_REF_TYPE="tag", NGIT_CI_REF_NAME="v1.2.3",
                   NGIT_CI_REF="refs/tags/v1.2.3", NGIT_CI_COMMIT_SHA="a" * 40, NGIT_CI_INPUTS="inputs")
        with patch.dict(os.environ, env, clear=True), patch.object(publisher, "plan"), patch.object(publisher.subprocess, "run") as run, patch.object(publisher.subprocess, "check_output", side_effect=["a" * 40, "b" * 40]):
            with self.assertRaisesRegex(ValueError, "does not match"):
                publisher.main()
            self.assertEqual(run.call_count, 1)
            self.assertEqual(run.call_args.args[0][:2], ["git", "fetch"])

    def test_tag_date_and_prerelease_channel(self):
        env = dict(NGIT_CI_TRIGGER="push", NGIT_CI_REF_TYPE="tag", NGIT_CI_REF_NAME="v1.2.3-rc.1",
                   NGIT_CI_REF="refs/tags/v1.2.3-rc.1", NGIT_CI_COMMIT_SHA="a" * 40, NGIT_CI_INPUTS="inputs")
        for tag_type in ["tag", "commit"]:
            with self.subTest(tag_type=tag_type), patch.dict(os.environ, env, clear=True), patch.object(publisher, "plan", return_value={}), patch.object(publisher.subprocess, "run"), patch.object(publisher.subprocess, "check_output", side_effect=["a" * 40, "a" * 40, tag_type, "1700000000"]):
                if tag_type == "tag":
                    self.assertEqual(publisher.prepare()[1:], ("1.2.3-rc.1", "rc", "1700000000"))
                else:
                    with self.assertRaisesRegex(ValueError, "annotated"):
                        publisher.prepare()

    def test_publication_preserves_verified_bytes_and_explicit_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            version = self.fixture(root)
            with patch.object(archives, "verify_archive"):
                release = release_plan.plan(root, f"v{version}", "a" * 40)
            secret = root / "NGIT_PUBLISHER_NBUNKSEC"
            secret.write_text("test-credential")
            # Keep process cwd unchanged; map only the output directory.
            real_path = publisher.Path
            def path(value):
                return root / "artifacts" if value == "artifacts" else real_path(value)
            with patch.dict(os.environ, {"NGIT_CI_SECRETS_DIR": str(root)}, clear=True), patch.object(publisher, "Path", side_effect=path), patch.object(publisher, "prepare", return_value=(release, version, "main", "1700000000")), patch.object(publisher.subprocess, "run") as run:
                publisher.main()
            for asset in release["assets"]:
                self.assertEqual((root / "artifacts" / asset["name"]).read_bytes(), asset["path"].read_bytes())
            args = run.call_args.args[0]
            self.assertEqual(args[args.index("--nbunksec-file") + 1], str(secret))
            self.assertEqual(args[args.index("--commit") + 1], "a" * 40)
            self.assertEqual(args[args.index("--tag") + 1], f"v{version}")
            self.assertNotIn("test-credential", args)

    def test_cancelled_job_does_not_publish(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "NGIT_PUBLISHER_NBUNKSEC").write_text("test-credential")
            cancel = root / "cancel"
            cancel.touch()
            env = dict(NGIT_CI_SECRETS_DIR=str(root), NGIT_CI_CANCEL_FILE=str(cancel))
            with patch.dict(os.environ, env, clear=True), patch.object(publisher, "prepare", return_value=({}, "1.2.3", "main", "1700000000")), patch.object(publisher.subprocess, "run") as run:
                with self.assertRaisesRegex(RuntimeError, "Cancelled"):
                    publisher.main()
                run.assert_not_called()
