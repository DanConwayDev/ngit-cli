"""Check resumable mirroring without credentials or GitHub mutations."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import Mock

spec = importlib.util.spec_from_file_location("mirror", Path(__file__).with_name("github-mirror.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class Mirror(unittest.TestCase):
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
            {"state": "uploaded", "digest": "sha256:abc"}, {},
        ]
        module.mirror(api, self.release())
        self.assertTrue(api.call.call_args_list[2].args[2]["draft"])
        self.assertEqual(api.call.call_args_list[-1].args, ("PATCH", "releases/1", {"draft": False, "make_latest": "false"}))

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
