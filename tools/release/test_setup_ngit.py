"""Exercise promotion against real isolated Git remotes, without credentials."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import setup_ngit as setup


class SetupPromotion(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repo = self.root / "checkout"
        self.repo.mkdir()
        self.env = {**os.environ, "HOME": str(self.root), "GIT_CONFIG_GLOBAL": os.devnull,
                    "GIT_CONFIG_NOSYSTEM": "1", "GIT_AUTHOR_NAME": "test",
                    "GIT_AUTHOR_EMAIL": "test@example.invalid", "GIT_COMMITTER_NAME": "test",
                    "GIT_COMMITTER_EMAIL": "test@example.invalid"}
        self.git = setup.Git(self.repo, self.env)
        self.git.run("init", "-b", "main")
        self.old = self.commit("old")
        for remote in ("origin", "github"):
            destination = self.root / remote
            self.git.run("init", "--bare", str(destination))
            self.git.run("remote", "add", remote, str(destination))
            self.git.run("push", remote, "HEAD:refs/heads/main", "HEAD:refs/tags/v3")
        self.observed = {remote: {"main": self.old, "tag": self.old} for remote in ("origin", "github")}
        self.new = self.commit("new")

    def commit(self, contents):
        (self.repo / "manifest.txt").write_text(contents)
        self.git.run("add", "manifest.txt")
        self.git.run("-c", "core.hooksPath=/dev/null", "commit", "-m", contents)
        return self.git.run("rev-parse", "HEAD")

    def assert_promoted(self):
        for remote in ("origin", "github"):
            for ref in ("refs/heads/main", "refs/tags/v3"):
                self.assertEqual(self.git.ref(remote, ref), self.new)

    def test_complete_update_validates_before_promoting_and_is_idempotent(self):
        scripts = self.repo / "scripts"
        scripts.mkdir()
        (scripts / "install.sh").write_text('test "$NGIT_SETUP_TARGET" = linux-x86_64-musl\nmkdir -p "$RUNNER_TOOL_CACHE"\n')
        self.git.run("add", "scripts")
        baseline = self.commit("baseline")
        for remote in ("origin", "github"):
            self.git.run("push", remote, "HEAD:refs/heads/main", "+HEAD:refs/tags/v3",
                         "HEAD:refs/tags/v3.0.6")
        commands = self.root / "bin"
        commands.mkdir()
        # Fake only the externally verified release/download operations;
        # cloning, manifest commits, ref checks and pushes use real Git.
        node = commands / "node"
        node.write_text('#!/usr/bin/env python3\nimport sys\nfrom pathlib import Path\n'
                        'if sys.argv[1] != "--test": Path("manifest.txt").write_text("verified pins")\n')
        ngit = commands / "ngit"
        ngit.write_text('#!/usr/bin/env python3\nimport sys\nassert "login" in sys.argv\n')
        node.chmod(0o755)
        ngit.chmod(0o755)
        env = {**self.env, "PATH": str(commands) + os.pathsep + self.env["PATH"],
               "NGIT_CI_COMMIT_SHA": baseline}
        with patch.object(setup, "CANONICAL", str(self.root / "origin")), \
             patch.object(setup, "MIRROR", str(self.root / "github")), \
             patch.object(setup, "PRODUCT", str(self.root / "origin")), \
             patch.dict(os.environ, {"NGIT_CI_SECRETS_DIR": str(self.root)}):
            for attempt in ("first", "retry"):
                root = self.root / attempt
                root.mkdir()
                setup.update("3.0.6", root, env)
                head = self.git.ref("origin", "refs/heads/main")
                if attempt == "first":
                    promoted = head
                    self.assertNotEqual(promoted, baseline)
                    author = setup.Git(root / "setup-ngit", env).run("show", "-s", "--format=%an <%ae>|%cn <%ce>", promoted)
                    self.assertEqual(author, "ngit Release Bot <release-bot@ngit.dev>|ngit Release Bot <release-bot@ngit.dev>")
                self.assertEqual(head, promoted)
                for remote in ("origin", "github"):
                    self.assertEqual(self.git.ref(remote, "refs/tags/v3"), promoted)
                self.assertEqual((root / "setup-ngit/manifest.txt").read_text(), "verified pins")

    def test_promotes_both_mains_before_tags_and_repeats(self):
        calls = []
        original = self.git.run
        def run(*args):
            if args[0] == "push":
                calls.append(args)
            return original(*args)
        with patch.object(self.git, "run", side_effect=run):
            setup.promote(self.git, self.new, "3", self.observed)
        self.assertTrue(all(c[-1].endswith(":refs/heads/main") for c in calls[:2]))
        self.assertTrue(all(f"--force-with-lease=refs/tags/v3:{self.old}" in c for c in calls[2:]))
        setup.promote(self.git, self.new, "3", self.observed)
        self.assert_promoted()

    def test_recovers_after_canonical_main_and_tag_were_published(self):
        self.git.run("push", "origin", "HEAD:refs/heads/main", "+HEAD:refs/tags/v3")
        setup.promote(self.git, self.new, "3", self.observed)
        self.assert_promoted()

    def test_rejects_concurrently_moved_tag(self):
        other = self.commit("concurrent")
        self.git.run("push", "github", f"+{other}:refs/tags/v3")
        with self.assertRaisesRegex(ValueError, "floating tag changed"):
            setup.promote(self.git, self.new, "3", self.observed)
        self.assertEqual(self.git.ref("github", "refs/tags/v3"), other)

    def test_rejects_concurrently_moved_main(self):
        other = self.commit("concurrent")
        self.git.run("push", "origin", f"{other}:refs/heads/main")
        with self.assertRaisesRegex(ValueError, "main changed"):
            setup.promote(self.git, self.new, "3", self.observed)
        self.assertEqual(self.git.ref("github", "refs/tags/v3"), self.old)

    def test_cancellation_stops_before_push(self):
        marker = self.root / "cancelled"
        marker.touch()
        self.env["NGIT_CI_CANCEL_FILE"] = str(marker)
        with self.assertRaisesRegex(RuntimeError, "Cancelled"):
            setup.promote(self.git, self.new, "3", self.observed)
        self.assertEqual(self.git.ref("origin", "refs/heads/main"), self.old)

    def test_missing_major_tags_are_created_with_empty_leases(self):
        for remote in ("origin", "github"):
            self.git.run("push", remote, ":refs/tags/v3")
            self.observed[remote]["tag"] = ""
        setup.promote(self.git, self.new, "3", self.observed)
        self.assert_promoted()


class SetupContext(unittest.TestCase):
    def test_askpass_only_serves_github_credentials(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            helper = root / "askpass"
            token = root / "token"
            token.write_text("fixture-token")
            setup.write_askpass(helper)
            env = {**os.environ, "RELEASE_TOKEN_FILE": str(token)}
            for prompt, expected in [("Username for 'https://github.com': ", "x-access-token"),
                                     ("Password for 'https://x-access-token@github.com': ", "fixture-token")]:
                result = subprocess.run([str(helper), prompt], env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(result.stdout.strip(), expected)
            for host in ["relay.ngit.dev", "github.com.evil.invalid", "github.com@evil.invalid"]:
                result = subprocess.run([str(helper), f"Password for 'https://{host}': "],
                                        env=env, capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")

    def test_stable_and_prerelease_context(self):
        env = dict(NGIT_CI_TRIGGER="push", NGIT_CI_REF_TYPE="tag",
                   NGIT_CI_REF_NAME="v3.0.6", NGIT_CI_REF="refs/tags/v3.0.6")
        self.assertEqual(setup.stable_version(env), "3.0.6")
        env.update(NGIT_CI_REF_NAME="v3.0.6-rc.1", NGIT_CI_REF="refs/tags/v3.0.6-rc.1")
        with patch.dict(os.environ, env, clear=True), patch.object(setup, "plan") as plan:
            setup.main()
            plan.assert_not_called()
        env["NGIT_CI_TRIGGER"] = "pull_request"
        with self.assertRaises(ValueError):
            setup.stable_version(env)
        env["NGIT_CI_TRIGGER"] = "push"
        env["NGIT_CI_REF"] = "refs/tags/v3.0.7"
        with self.assertRaises(ValueError):
            setup.stable_version(env)
