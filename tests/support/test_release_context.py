"""Pure cache-copy safety tests. No daemon, model, database, or Cargo runs."""
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from release_context import ContextDaemon, IsolatedDaemon


class ModelCacheIsolation(unittest.TestCase):
    def fixture(self, root):
        def initialize(instance, _binary, timeout):
            instance.root = root / "fixture"
            instance.root.mkdir()
        return patch.object(IsolatedDaemon, "__init__", initialize)

    def test_directory_symlink_is_rejected_before_copy(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            model = root / "cache" / "models--owner--model"
            model.mkdir(parents=True)
            outside = root / "outside"
            outside.mkdir()
            (outside / "contents").write_text("fixture")
            (model / "linked-directory").symlink_to(outside, target_is_directory=True)
            with self.fixture(root), patch("release_context.shutil.copytree") as copy:
                with self.assertRaisesRegex(ValueError, "directory symlinks"):
                    ContextDaemon("unused", root / "cache", "owner/model", 5)
                copy.assert_not_called()

    def test_file_symlink_is_copied_without_sharing_user_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            model = root / "cache" / "models--owner--model"
            model.mkdir(parents=True)
            (model / "blob").write_text("fixture")
            (model / "model.safetensors").symlink_to("blob")
            unrelated = root / "cache" / "models--unrelated"
            unrelated.mkdir()
            with self.fixture(root), patch.object(IsolatedDaemon, "record"):
                fixture = ContextDaemon("unused", root / "cache", "owner/model", 5)
            copied = fixture.models / model.name / "model.safetensors"
            self.assertFalse(copied.is_symlink())
            self.assertEqual(copied.read_text(), "fixture")
            copied.write_text("changed in isolated fixture")
            self.assertEqual((model / "blob").read_text(), "fixture")
            self.assertFalse((fixture.models / unrelated.name).exists())


if __name__ == "__main__":
    unittest.main()
