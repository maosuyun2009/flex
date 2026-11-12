"""Run the embedded remote picker against isolated zoxide data, never user history.
Usage: python3 -m unittest discover -s tests -p test_history_picker.py -v
Requires sh, zoxide and fzf on the test machine (normally WSL/Linux).
"""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "crates/app/src/pick_history.sh"

@unittest.skipUnless(shutil.which("zoxide") and shutil.which("fzf") and shutil.which("sh"),
                     "requires zoxide, fzf and sh")
class HistoryPickerTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="flex-history-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.cwd = self.root / "current-project"
        self.cwd.mkdir()
        (self.cwd / "local-only-directory").mkdir()
        self.history = self.root / "visited directory"
        self.history.mkdir()
        self.env = dict(os.environ)
        self.env.update(_ZO_DATA_DIR=str(self.root / "zoxide"), _ZO_EXCLUDE_DIRS="", _ZO_RESOLVE_SYMLINKS="0",
                        FZF_DEFAULT_OPTS="", FZF_DEFAULT_OPTS_FILE="/dev/null")
        subprocess.run(["zoxide", "add", str(self.history)], env=self.env, check=True)

    def pick(self, query):
        env = dict(self.env, FZF_DEFAULT_OPTS="--filter=" + query)
        return subprocess.run(["sh", str(SCRIPT)], cwd=self.cwd, env=env, input="\n", text=True,
                              capture_output=True, timeout=5)

    def test_history_can_select_a_directory_outside_the_current_tree(self):
        result = self.pick("visited")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("@@pick@@" + str(self.history), result.stdout)

    def test_unvisited_current_subdirectories_are_not_history(self):
        result = self.pick("local-only")
        self.assertNotIn("@@pick@@", result.stdout)

    def test_missing_zoxide_is_reported_without_falling_back_to_file_scan(self):
        empty_bin = self.root / "empty-bin"
        empty_bin.mkdir()
        result = subprocess.run([shutil.which("sh"), str(SCRIPT)], cwd=self.cwd,
                                env=dict(self.env, PATH=str(empty_bin)), input="\n",
                                text=True, capture_output=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("zoxide is not installed", result.stdout)
        self.assertNotIn("@@pick@@", result.stdout)

    def test_empty_history_has_a_visible_message_and_no_selection(self):
        subprocess.run(["zoxide", "remove", str(self.history)], env=self.env, check=True)
        result = self.pick("anything")
        self.assertNotIn("@@pick@@", result.stdout)
        self.assertIn("No zoxide directory history", result.stdout)

if __name__ == "__main__":
    unittest.main()
