"""Check resumable mirroring without credentials or GitHub mutations."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location("mirror", Path(__file__).with_name("github-mirror.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class Mirror(unittest.TestCase):
    def test_directory_artifact_handoff(self):
        # Provider directory artifacts retain paths relative to the declared
        # directory, not the directory's own basename.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            inputs = root / "inputs/rehearsal.ready/artifacts/release-set"
            inputs.mkdir(parents=True)
            (inputs / "builds.json").write_text("[]")
            secrets = root / "secrets"
            secrets.mkdir()
            (secrets / "GH_RELEASE_TOKEN").write_text("test-token")
            env = dict(NGIT_CI_REF_TYPE="tag", NGIT_CI_TRIGGER="manual",
                       NGIT_CI_INPUTS=str(root / "inputs"), NGIT_CI_REF_NAME="v1.2.3",
                       NGIT_CI_COMMIT_SHA="a" * 40, NGIT_CI_SECRETS_DIR=str(secrets))
            def read_plan(directory, tag, commit):
                return json.loads((directory / "builds.json").read_text())
            with patch.dict(module.os.environ, env), patch.object(module, "plan", side_effect=read_plan), patch.object(module, "mirror") as publish:
                module.main()
            publish.assert_called_once()

    def release(self):
        return dict(tag_name="v1.2.3", target_commitish="a" * 40, name="v1.2.3", body="notes",
                    prerelease=False, assets=[dict(name="ngit.zip", sha256="abc", path=Mock())])

    def test_wrong_tag_stops_before_mutations(self):
        api = Mock()
        api.call.return_value = {"object": {"type": "commit", "sha": "b" * 40}}
        with self.assertRaisesRegex(ValueError, "does not match"):
            module.mirror(api, self.release())
        self.assertEqual(api.call.call_count, 1)

    def test_existing_identical_release_is_noop(self):
        api = Mock()
        api.call.side_effect = [
            {"object": {"type": "tag", "sha": "c" * 40}},
            {"object": {"type": "commit", "sha": "a" * 40}},
            [{"tag_name": "v1.2.3", "id": 1, "draft": False}],
            [{"name": "ngit.zip", "state": "uploaded", "digest": "sha256:abc"}],
            {"tag_name": "v1.2.3"},
        ]
        module.mirror(api, self.release())
        self.assertTrue(all(call.args[0] == "GET" for call in api.call.call_args_list))

    def test_conflicting_asset_is_never_replaced(self):
        api = Mock()
        api.call.side_effect = [
            {"object": {"type": "commit", "sha": "a" * 40}},
            [{"tag_name": "v1.2.3", "id": 1, "draft": False}],
            [{"name": "ngit.zip", "state": "uploaded", "digest": "sha256:other"}],
        ]
        with self.assertRaisesRegex(ValueError, "differs"):
            module.mirror(api, self.release())
        self.assertTrue(all(call.args[0] == "GET" for call in api.call.call_args_list))

    def test_new_release_published_only_after_verified_upload(self):
        api = Mock()
        api.call.side_effect = [
            {"object": {"type": "commit", "sha": "a" * 40}}, [],
            {"id": 1, "draft": True}, [],
            {"state": "uploaded", "digest": "sha256:abc"}, {"tag_name": "v1.2.2"}, {},
        ]
        module.mirror(api, self.release())
        self.assertTrue(api.call.call_args_list[2].args[2]["draft"])
        self.assertEqual(api.call.call_args_list[-1].args, ("PATCH", "releases/1", {"draft": False, "make_latest": "true"}))

    def test_failed_verification_leaves_draft(self):
        api = Mock()
        api.call.side_effect = [
            {"object": {"type": "commit", "sha": "a" * 40}}, [],
            {"id": 1, "draft": True}, [],
            {"state": "uploaded", "digest": "sha256:other"},
        ]
        with self.assertRaisesRegex(ValueError, "verification"):
            module.mirror(api, self.release())
        self.assertFalse(any(call.args[0] == "PATCH" for call in api.call.call_args_list))

    def test_latest_advances_only_for_newer_stable_versions(self):
        for tag, latest, expected in [("v1.10.0", "v1.9.9", True), ("v1.2.3", "v2.0.0", False),
                                      ("v1.2.3+build-id", "v1.2.3", False), ("v2.0.0-rc.1", "v1.2.3", False)]:
            with self.subTest(tag=tag, latest=latest):
                api = Mock()
                api.call.return_value = {"tag_name": latest}
                release = dict(tag_name=tag, prerelease="-" in tag.split("+", 1)[0])
                self.assertEqual(module.promote_latest(api, release), expected)
                if release["prerelease"]:
                    api.call.assert_not_called()

    def test_first_stable_release_becomes_latest_but_api_errors_propagate(self):
        for code in [404, 403, 500]:
            with self.subTest(code=code):
                api = Mock()
                api.call.side_effect = module.urllib.error.HTTPError("https://api.github.com", code, "test", {}, None)
                if code == 404:
                    self.assertTrue(module.promote_latest(api, self.release()))
                else:
                    with self.assertRaises(module.urllib.error.HTTPError):
                        module.promote_latest(api, self.release())

    def test_public_older_release_retry_does_not_change_latest(self):
        api = Mock()
        api.call.side_effect = [
            {"object": {"type": "commit", "sha": "a" * 40}},
            [{"tag_name": "v1.2.3", "id": 1, "draft": False}],
            [{"name": "ngit.zip", "state": "uploaded", "digest": "sha256:abc"}],
            {"tag_name": "v2.0.0"},
        ]
        module.mirror(api, self.release())
        self.assertTrue(all(call.args[0] == "GET" for call in api.call.call_args_list))

    def test_public_newer_release_recovers_latest_without_reuploading(self):
        api = Mock()
        api.call.side_effect = [
            {"object": {"type": "commit", "sha": "a" * 40}},
            [{"tag_name": "v1.2.3", "id": 1, "draft": False}],
            [{"name": "ngit.zip", "state": "uploaded", "digest": "sha256:abc"}],
            {"tag_name": "v1.2.2"}, {},
        ]
        module.mirror(api, self.release())
        self.assertEqual(api.call.call_args_list[-1].args, ("PATCH", "releases/1", {"make_latest": "true"}))
        self.assertFalse(any(call.args[0] == "POST" for call in api.call.call_args_list))

    def test_unknown_latest_tag_is_not_silently_replaced(self):
        api = Mock()
        api.call.return_value = {"tag_name": "custom-release"}
        with self.assertRaisesRegex(ValueError, "Cannot compare"):
            module.promote_latest(api, self.release())

    def test_old_draft_publishes_without_becoming_latest(self):
        api = Mock()
        api.call.side_effect = [
            {"object": {"type": "commit", "sha": "a" * 40}},
            [{"tag_name": "v1.2.3", "id": 1, "draft": True}],
            [{"name": "ngit.zip", "state": "uploaded", "digest": "sha256:abc"}],
            {"tag_name": "v2.0.0"}, {},
        ]
        module.mirror(api, self.release())
        self.assertEqual(api.call.call_args_list[-1].args, ("PATCH", "releases/1", {"draft": False, "make_latest": "false"}))
