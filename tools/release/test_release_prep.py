"""A tag cannot manufacture release approval or replace prepared bytes."""
from contextlib import nullcontext
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import release_prep


class ReleasePrep(unittest.TestCase):
    def test_only_manual_branch_context_can_approve(self):
        for trigger, ref_type in [("push", "tag"), ("push", "branch"),
                                  ("pull_request", "branch"), ("manual", "tag"),
                                  ("manual", "commit")]:
            with self.subTest(trigger=trigger, ref_type=ref_type), patch.object(release_prep, "plan") as plan:
                with self.assertRaisesRegex(ValueError, "before tagging"):
                    release_prep.prepare(dict(NGIT_CI_TRIGGER=trigger, NGIT_CI_REF_TYPE=ref_type))
                plan.assert_not_called()

    def test_manual_release_branch_preserves_bytes_and_commit_binding(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "inputs/archives.collect/artifacts/release-set"
            source.mkdir(parents=True)
            blobs = {"archive.tar.gz": b"exact archive bytes", "builds.json": b"source provenance"}
            for name, value in blobs.items():
                (source / name).write_bytes(value)
            env = dict(NGIT_CI_TRIGGER="manual", NGIT_CI_REF_TYPE="branch",
                       NGIT_CI_REF="refs/heads/release-prep/v1.2.3",
                       NGIT_CI_INPUTS=str(root / "inputs"), NGIT_CI_OUTPUT_DIR=str(root / "output"),
                       NGIT_CI_COMMIT_SHA="a" * 40)
            with patch.object(release_prep, "plan", return_value={}) as plan, \
                 patch.object(release_prep, "environment", return_value=nullcontext()):
                release_prep.prepare(env)
                self.assertEqual(plan.call_args.args[0], source)
                self.assertEqual(plan.call_args.args[2], "a" * 40)
            for name, value in blobs.items():
                self.assertEqual((root / "output/release" / name).read_bytes(), value)

    def test_failed_validation_never_exports_approval(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            env = dict(NGIT_CI_TRIGGER="manual", NGIT_CI_REF_TYPE="branch",
                       NGIT_CI_INPUTS=str(root / "inputs"), NGIT_CI_OUTPUT_DIR=str(root / "output"),
                       NGIT_CI_COMMIT_SHA="b" * 40)
            with patch.object(release_prep, "plan", side_effect=ValueError("commit mismatch")):
                with self.assertRaisesRegex(ValueError, "commit mismatch"):
                    release_prep.prepare(env)
            self.assertFalse((root / "output").exists())
