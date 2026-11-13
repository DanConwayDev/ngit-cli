"""Repair checkout identity never replaces the signed release identity or bytes."""
import copy
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import archives
import post_release as recovery


def fixture(directory):
    target = dict(schema=1, version="3.0.5", commit="a" * 40, release_event="b" * 64,
                  operations=["mirror", "update-gh-setup", "update-docs"])
    assets = []
    for index, platform in enumerate(archives.TARGETS.values()):
        data = platform.encode()
        sha = hashlib.sha256(data).hexdigest()
        name = archives.filename(platform)
        (directory / name).write_bytes(data)
        assets.append(dict(event_id=str(index) * 64, filename=name.replace("ngit-", "ngit-v3.0.5-", 1),
                           author=recovery.AUTHOR, application_coordinate=recovery.APP, version="3.0.5",
                           resolution="resolved", validation=[], sha256=sha, size=len(data),
                           url=f"https://relay.ngit.dev/{sha}",
                           verification=dict(ok=True, observed_sha256=sha, observed_size=str(len(data)))))
    doc = dict(format_version=2, command_status="ok", command="release.view", result=dict(
        application=dict(coordinate=recovery.APP, trusted=True, fully_linked_to_current_repository=True, validation=[]),
        release=dict(author=recovery.AUTHOR, application_coordinate=recovery.APP, event_id=target["release_event"],
                     version="3.0.5", commit=target["commit"], channel="main", validation=[],
                     asset_ids=[a["event_id"] for a in assets], notes="Original signed release notes"),
        assets=assets, unresolved_asset_ids=[]))
    return target, doc


class Recovery(unittest.TestCase):
    def test_request_requires_reviewed_identity_and_operations(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target, _ = fixture(root)
            path = root / "request.json"
            path.write_text(json.dumps(target))
            self.assertEqual(recovery.request(path), target)
            for field, value in [("version", None), ("commit", "HEAD"), ("release_event", "latest"),
                                 ("operations", []), ("operations", ["publish-nip82"])]:
                path.write_text(json.dumps({**target, field: value}))
                with self.assertRaises(ValueError):
                    recovery.request(path)

    def test_signed_release_plan_is_independent_of_repair_checkout_version(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target, doc = fixture(root)
            release = recovery.release_plan(doc, target, root)
            self.assertEqual(release["target_commitish"], "a" * 40)
            self.assertEqual(release["tag_name"], "v3.0.5")
            self.assertEqual(release["body"], "Original signed release notes")
            self.assertEqual(len(release["assets"]), 5)
            for mutate in [lambda d: d["result"]["release"].update(commit="c" * 40),
                           lambda d: d["result"]["release"].update(event_id="c" * 64),
                           lambda d: d["result"]["application"].update(trusted=False),
                           lambda d: d["result"]["assets"][0].update(author="c" * 64),
                           lambda d: d["result"]["assets"][0]["verification"].update(ok=False),
                           lambda d: d["result"]["assets"].pop()]:
                changed = copy.deepcopy(doc)
                mutate(changed)
                with self.assertRaises(ValueError):
                    recovery.release_plan(changed, target, root)

    def test_no_side_effects_outside_manual_or_explicit_apply_workflow(self):
        with patch.dict(os.environ, {"NGIT_CI_TRIGGER": "push"}, clear=True), patch.object(recovery, "request") as request:
            with self.assertRaisesRegex(ValueError, "manual"):
                recovery.main()
            request.assert_not_called()
        with patch.dict(os.environ, dict(NGIT_CI_TRIGGER="manual", NGIT_CI_JOB_ID="mirror", NGIT_CI_OUTPUT_DIR="unused"), clear=True), \
             patch.object(recovery, "request", return_value={}), patch.object(recovery.sys, "argv", ["script", "--apply"]):
            with self.assertRaisesRegex(ValueError, "explicit recovery"):
                recovery.main()

    def test_apply_rejects_pr_context_and_stale_main_before_consuming_assets(self):
        env = dict(NGIT_CI_TRIGGER="manual", NGIT_CI_JOB_ID="mirror",
                   NGIT_CI_OUTPUT_DIR="unused", NGIT_CI_COMMIT_SHA="a" * 40,
                   NGIT_CI_WORKFLOW_PATH=".ngit/nix/workflows/post-release-apply.yaml")
        for ref, pattern in [("refs/heads/pr/repair", "reviewed main"),
                             ("refs/heads/main", "current canonical main")]:
            with self.subTest(ref=ref), patch.dict(os.environ, {**env, "NGIT_CI_REF": ref}, clear=True), \
                 patch.object(recovery, "request", return_value={}), \
                 patch.object(recovery.sys, "argv", ["script", "--apply"]), \
                 patch.object(recovery.subprocess, "check_output", return_value="b" * 40 + "\trefs/heads/main\n"):
                with self.assertRaisesRegex(ValueError, pattern):
                    recovery.main()

    def test_consumer_cannot_replace_original_signed_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target, doc = fixture(root)
            release = recovery.release_plan(doc, target, root)
            (root / "manifest.txt").write_text("\n".join(f'asset|3.0.5|target|{a["name"]}|{a["sha256"]}|url' for a in release["assets"]))
            recovery.validate_consumer(root, "update-gh-setup", target, release)
            with (root / "manifest.txt").open("a") as output:
                output.write("\nasset|3.0.5|other|extra|wrong|url")
            with self.assertRaises(ValueError):
                recovery.validate_consumer(root, "update-gh-setup", target, release)

    def test_github_readonly_api_refuses_mutations_before_network(self):
        api = recovery.github.API("fixture", read_only=True)
        with self.assertRaisesRegex(ValueError, "forbidden"):
            api.call("POST", "releases", {})

    def test_github_rehearsal_exports_missing_assets_without_posting(self):
        api = Mock()
        release = dict(tag_name="v3.0.5", target_commitish="a" * 40, prerelease=False,
                       assets=[dict(name="archive.zip", sha256="hash")])
        api.call.side_effect = [{"object": {"type": "commit", "sha": "a" * 40}}, [], {"tag_name": "v3.0.4"}]
        result = recovery.github.mirror(api, release, dry_run=True)
        self.assertEqual(result["upload"], ["archive.zip"])
        self.assertTrue(result["create_release"])
        self.assertTrue(all(call.args[0] == "GET" for call in api.call.call_args_list))

    def test_git_preview_guard_refuses_push(self):
        git = recovery.setup_ngit.Git(Path("unused"), {"NGIT_RELEASE_PREVIEW": "1"})
        with self.assertRaisesRegex(RuntimeError, "forbidden"):
            git.run("push", "origin", "HEAD:main")
