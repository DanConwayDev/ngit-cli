"""Docs promotion: isolated real Git publication, identity, and deployment evidence."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import update_docs as docs
from setup_ngit import Git


class DocsPromotion(unittest.TestCase):
    def test_sync_push_and_retry_use_bot_identity_and_wait_for_deployment(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            repo = root / "fixture"
            repo.mkdir()
            env = {**os.environ, "HOME": str(root), "GIT_CONFIG_GLOBAL": os.devnull,
                   "GIT_CONFIG_NOSYSTEM": "1", "GIT_AUTHOR_NAME": "fixture",
                   "GIT_AUTHOR_EMAIL": "fixture@example.invalid", "GIT_COMMITTER_NAME": "fixture",
                   "GIT_COMMITTER_EMAIL": "fixture@example.invalid", "NGIT_CI_COMMIT_SHA": "a" * 40}
            git = Git(repo, env)
            git.run("init", "-b", "main")
            (repo / "sources.lock.json").write_text(json.dumps({"sources": {"ngit": {"stable": {
                "version": "3.0.5", "commit": "b" * 40}}}}))
            (repo / "release").mkdir()
            (repo / "release/installer-promotion.json").write_text(json.dumps({
                "source": {"commit": "b" * 40}, "release": {"version": "3.0.5"}}))
            git.run("add", ".")
            git.run("-c", "core.hooksPath=/dev/null", "commit", "-m", "fixture")
            remote = root / "origin.git"
            git.run("clone", "--bare", str(repo), str(remote))
            tools = root / "bin"
            tools.mkdir()
            npm = tools / "npm"
            npm.write_text('''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
if 'docs:sync:ngit' in sys.argv:
 p=Path('sources.lock.json'); d=json.loads(p.read_text())
 d['sources']['ngit']['stable']={'version':'3.0.6','commit':os.environ['NGIT_CI_COMMIT_SHA']}
 p.write_text(json.dumps(d))
 Path('docs/public').mkdir(parents=True,exist_ok=True)
 Path('docs/public/install.sh').write_text('verified shell')
 Path('docs/public/install.ps1').write_text('verified windows')
 Path('release/installer-promotion.json').write_text(json.dumps({'source':{'commit':os.environ['NGIT_CI_COMMIT_SHA']},'release':{'version':'3.0.6'}}))
''')
            ngit = tools / "ngit"
            ngit.write_text('#!/usr/bin/env python3\nimport sys\nassert "login" in sys.argv\n')
            npm.chmod(0o755)
            ngit.chmod(0o755)
            env["PATH"] = str(tools) + os.pathsep + env["PATH"]
            with patch.object(docs, "REPOSITORY", str(remote)), patch.object(docs, "wait_for_site") as wait, \
                 patch.dict(os.environ, {"NGIT_CI_SECRETS_DIR": str(root)}):
                preview_root = root / "preview"
                preview_root.mkdir()
                docs.update("3.0.6", preview_root, {**env, "NGIT_RELEASE_PREVIEW": "1"},
                            preview=preview_root / "proposal")
                self.assertTrue((preview_root / "proposal/proposed.patch").read_text())
                Git(preview_root / "ngit-docs", env).run("apply", "--reverse", "--check", str(preview_root / "proposal/proposed.patch"))
                self.assertEqual((preview_root / "proposal/install.sh").read_text(), "verified shell")
                wait.assert_not_called()
                self.assertEqual(Git(preview_root / "ngit-docs", env).ref("origin", "refs/heads/main"), git.run("rev-parse", "HEAD"))
                heads = []
                for attempt in ("first", "retry"):
                    work = root / attempt
                    work.mkdir()
                    docs.update("3.0.6", work, env)
                    checkout = Git(work / "ngit-docs", env)
                    heads.append(checkout.ref("origin", "refs/heads/main"))
                    self.assertEqual(checkout.run("show", "-s", "--format=%an <%ae>|%cn <%ce>"),
                                     "ngit Release Bot <release-bot@ngit.dev>|ngit Release Bot <release-bot@ngit.dev>")
                self.assertEqual(heads[0], heads[1])
                self.assertEqual(wait.call_count, 2)

    def test_ref_race_stops_before_push(self):
        from unittest.mock import Mock
        git = Mock()
        git.ref.return_value = "new concurrent commit"
        with self.assertRaisesRegex(ValueError, "main changed"):
            docs.publish(git, "observed", "intended")
        git.run.assert_not_called()

    def test_site_success_requires_matching_commit_workflow_and_trust(self):
        run = dict(commit="a" * 40, workflow=".ngit/nix/workflows/docs.yaml",
                   conclusion="success", classification="maintainer-directed")
        self.assertTrue(docs.deployed({"ci": {"runs": [run]}}, "a" * 40))
        for key, value in [("commit", "b" * 40), ("workflow", ".ngit/nix/workflows/ci.yaml"),
                           ("conclusion", "failure"), ("classification", None)]:
            self.assertFalse(docs.deployed({"ci": {"runs": [{**run, key: value}]}}, "a" * 40))

    def test_wait_checks_live_bytes_after_success_and_retries_stale_gateway(self):
        import io
        from types import SimpleNamespace
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)
            (repo / "docs/public").mkdir(parents=True)
            for name in ("install.sh", "install.ps1"):
                (repo / "docs/public" / name).write_bytes(b"verified")
            result = SimpleNamespace(returncode=0, stdout=json.dumps({"ci": {"runs": [{
                "commit": "a" * 40, "workflow": ".ngit/nix/workflows/docs.yaml",
                "conclusion": "success", "classification": "maintainer-directed"}]}}))
            with patch.object(docs.subprocess, "run", return_value=result) as run, \
                 patch.object(docs.urllib.request, "urlopen", side_effect=[io.BytesIO(b"old"), io.BytesIO(b"verified"), io.BytesIO(b"verified")]), \
                 patch.object(docs.time, "sleep"):
                docs.wait_for_site(repo, "a" * 40, {}, timeout=10)
            self.assertEqual(run.call_count, 2)

    def test_wait_timeout_reports_pushed_but_not_deployed(self):
        with self.assertRaisesRegex(TimeoutError, "was pushed.*pending"):
            docs.wait_for_site(Path("unused"), "a" * 40, {}, timeout=0)

    def test_generated_content_scope_excludes_scripts_and_other_imports(self):
        for path in ["docs/public/install.sh", "docs/ngit/reference/stable/index.md", "sources.lock.json"]:
            self.assertTrue(docs.allowed(path))
        for path in ["scripts/sync-ngit-docs.mjs", "package.json", "content-imports/ngit/upcoming/cli.json"]:
            self.assertFalse(docs.allowed(path))

    def test_prereleases_never_read_release_assets_or_credentials(self):
        env = dict(NGIT_CI_TRIGGER="push", NGIT_CI_REF_TYPE="tag",
                   NGIT_CI_REF_NAME="v3.0.6-rc.1", NGIT_CI_REF="refs/tags/v3.0.6-rc.1")
        with patch.dict(os.environ, env, clear=True), patch.object(docs, "plan") as plan:
            docs.main()
            plan.assert_not_called()
