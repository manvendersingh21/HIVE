#!/usr/bin/env python3
"""Unit tests for scripts/changelog.py."""

import datetime
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

# Allow running this test file directly: python3 scripts/test_changelog.py
_REPO_ROOT = Path(__file__).resolve().parent.parent
if str(_REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(_REPO_ROOT))

from scripts.changelog import (
    ChangelogError,
    ValidationError,
    check_fragments,
    find_fragments,
    format_release_block,
    main,
    parse_fragment,
    release,
)


class TestChangelogCheck(unittest.TestCase):
    def setUp(self):
        self.test_dir = tempfile.mkdtemp()
        self.changelog_dir = Path(self.test_dir) / "changelog.d"
        self.changelog_dir.mkdir()
        (self.changelog_dir / ".gitkeep").write_text("# gitkeep\n")

    def tearDown(self):
        shutil.rmtree(self.test_dir)

    def test_find_fragments_ignores_hidden_files(self):
        (self.changelog_dir / "001-fix.md").write_text("### Fixed\n- Fixed bug\n")
        (self.changelog_dir / ".hidden.md").write_text("### Added\n- Hidden\n")
        fragments = find_fragments(self.changelog_dir)
        self.assertEqual(len(fragments), 1)
        self.assertEqual(fragments[0].name, "001-fix.md")

    def test_check_empty_dir_passes(self):
        errors = check_fragments(self.changelog_dir)
        self.assertEqual(errors, [])

    def test_check_valid_single_fragment(self):
        frag = self.changelog_dir / "feature.md"
        frag.write_text("### Added\n- New feature description.\n")
        errors = check_fragments(self.changelog_dir)
        self.assertEqual(errors, [])

    def test_check_valid_multiple_sections(self):
        frag = self.changelog_dir / "combo.md"
        frag.write_text("### Added\n- Added item.\n\n### Fixed\n- Fixed issue.\n")
        errors = check_fragments(self.changelog_dir)
        self.assertEqual(errors, [])

    def test_check_empty_fragment_file(self):
        frag = self.changelog_dir / "empty.md"
        frag.write_text("")
        errors = check_fragments(self.changelog_dir)
        self.assertTrue(any("is empty" in err for err in errors))

    def test_check_whitespace_only_fragment(self):
        frag = self.changelog_dir / "whitespace.md"
        frag.write_text("   \n\n\t  \n")
        errors = check_fragments(self.changelog_dir)
        self.assertTrue(any("is empty" in err for err in errors))

    def test_check_missing_heading(self):
        frag = self.changelog_dir / "no_heading.md"
        frag.write_text("- Just a bullet point without heading\n")
        errors = check_fragments(self.changelog_dir)
        self.assertTrue(any("before a section heading" in err or "does not contain any section heading" in err for err in errors))

    def test_check_invalid_heading(self):
        frag = self.changelog_dir / "bad_heading.md"
        frag.write_text("### RandomHeading\n- Some change\n")
        errors = check_fragments(self.changelog_dir)
        self.assertTrue(any("invalid heading 'RandomHeading'" in err for err in errors))

    def test_check_empty_heading_content(self):
        frag = self.changelog_dir / "empty_heading.md"
        frag.write_text("### Added\n\n")
        errors = check_fragments(self.changelog_dir)
        self.assertTrue(any("with no content" in err for err in errors))

    def test_check_duplicate_fragment_names(self):
        frag1 = self.changelog_dir / "fragment.md"
        frag1.write_text("### Added\n- Fragment 1\n")
        # Test duplicate detection by calling check_fragments with mocked or directly simulated duplicate name
        from unittest.mock import patch
        with patch("scripts.changelog.find_fragments") as mock_find:
            mock_find.return_value = [
                self.changelog_dir / "fragment.md",
                self.changelog_dir / "Fragment.md",
            ]
            errors = check_fragments(self.changelog_dir)
            self.assertTrue(any("Duplicate fragment name" in err for err in errors))


class TestChangelogRelease(unittest.TestCase):
    def setUp(self):
        self.test_dir = tempfile.mkdtemp()
        self.changelog_dir = Path(self.test_dir) / "changelog.d"
        self.changelog_dir.mkdir()
        (self.changelog_dir / ".gitkeep").write_text("# gitkeep\n")
        self.changelog_file = Path(self.test_dir) / "CHANGELOG.md"
        self.changelog_file.write_text(
            "# Changelog\n\n## [Unreleased]\n\nExisting unreleased text.\n"
        )

    def tearDown(self):
        shutil.rmtree(self.test_dir)

    def test_release_folds_fragments_and_deletes_them(self):
        (self.changelog_dir / "01-feat.md").write_text("### Added\n- Feature A\n")
        (self.changelog_dir / "02-fix.md").write_text("### Fixed\n- Fix B\n")
        (self.changelog_dir / "03-feat2.md").write_text("### Added\n- Feature C\n")

        release(
            version="1.0.0",
            changelog_dir=self.changelog_dir,
            changelog_file=self.changelog_file,
            release_date="2026-09-28",
        )

        content = self.changelog_file.read_text()
        self.assertIn("## [1.0.0] - 2026-09-28", content)
        self.assertIn("### Added", content)
        self.assertIn("- Feature A", content)
        self.assertIn("- Feature C", content)
        self.assertIn("### Fixed", content)
        self.assertIn("- Fix B", content)

        # Check fragment deletion
        fragments = find_fragments(self.changelog_dir)
        self.assertEqual(len(fragments), 0)
        # .gitkeep preserved
        self.assertTrue((self.changelog_dir / ".gitkeep").exists())

    def test_release_section_order(self):
        # Order should follow Keep a Changelog standard: Added, Changed, Fixed
        (self.changelog_dir / "01-fix.md").write_text("### Fixed\n- Fix B\n")
        (self.changelog_dir / "02-add.md").write_text("### Added\n- Feature A\n")
        (self.changelog_dir / "03-change.md").write_text("### Changed\n- Change C\n")

        release(
            version="v2.1.0",
            changelog_dir=self.changelog_dir,
            changelog_file=self.changelog_file,
            release_date="2026-09-28",
        )

        content = self.changelog_file.read_text()
        self.assertIn("## [2.1.0] - 2026-09-28", content)

        pos_added = content.find("### Added")
        pos_changed = content.find("### Changed")
        pos_fixed = content.find("### Fixed")

        self.assertTrue(pos_added < pos_changed < pos_fixed)

    def test_release_no_fragments_fails(self):
        with self.assertRaises(ChangelogError) as ctx:
            release(
                version="1.0.0",
                changelog_dir=self.changelog_dir,
                changelog_file=self.changelog_file,
            )
        self.assertIn("No fragments found", str(ctx.exception))

    def test_release_with_invalid_fragments_aborts(self):
        (self.changelog_dir / "bad.md").write_text("invalid content without heading")
        with self.assertRaises(ChangelogError):
            release(
                version="1.0.0",
                changelog_dir=self.changelog_dir,
                changelog_file=self.changelog_file,
            )
        # Fragment was not deleted
        self.assertTrue((self.changelog_dir / "bad.md").exists())
        # Changelog was not modified
        self.assertNotIn("## [1.0.0]", self.changelog_file.read_text())

    def test_release_refuses_existing_version(self):
        frag = self.changelog_dir / "01-feat.md"
        frag.write_text("### Added\n- Feature A\n")
        initial_changelog = (
            "# Changelog\n\n## [Unreleased]\n\n## [1.0.0] - 2026-01-01\n\n### Added\n- Initial release\n"
        )
        self.changelog_file.write_text(initial_changelog)

        with self.assertRaises(ChangelogError) as ctx:
            release(
                version="1.0.0",
                changelog_dir=self.changelog_dir,
                changelog_file=self.changelog_file,
            )
        self.assertIn("already exists", str(ctx.exception))

        # Test with 'v' prefix as well
        with self.assertRaises(ChangelogError) as ctx:
            release(
                version="v1.0.0",
                changelog_dir=self.changelog_dir,
                changelog_file=self.changelog_file,
            )
        self.assertIn("already exists", str(ctx.exception))

        # Verify nothing was written to CHANGELOG.md and fragments were preserved
        self.assertEqual(self.changelog_file.read_text(), initial_changelog)
        self.assertTrue(frag.exists())


class TestChangelogCLI(unittest.TestCase):
    def setUp(self):
        self.test_dir = tempfile.mkdtemp()
        self.changelog_dir = Path(self.test_dir) / "changelog.d"
        self.changelog_dir.mkdir()
        (self.changelog_dir / ".gitkeep").write_text("# gitkeep\n")
        self.changelog_file = Path(self.test_dir) / "CHANGELOG.md"
        self.changelog_file.write_text("# Changelog\n\n## [Unreleased]\n")

    def tearDown(self):
        shutil.rmtree(self.test_dir)

    def test_cli_check_success(self):
        (self.changelog_dir / "test.md").write_text("### Added\n- Something\n")
        ret = main(["check", "--dir", str(self.changelog_dir)])
        self.assertEqual(ret, 0)

    def test_cli_check_failure(self):
        (self.changelog_dir / "test.md").write_text("")
        ret = main(["check", "--dir", str(self.changelog_dir)])
        self.assertEqual(ret, 1)

    def test_cli_release_success(self):
        (self.changelog_dir / "test.md").write_text("### Fixed\n- A bug\n")
        ret = main([
            "release",
            "1.2.0",
            "--dir",
            str(self.changelog_dir),
            "--file",
            str(self.changelog_file),
            "--date",
            "2026-09-28",
        ])
        self.assertEqual(ret, 0)
        self.assertIn("## [1.2.0] - 2026-09-28", self.changelog_file.read_text())

    def test_cli_release_refuses_existing_version(self):
        (self.changelog_dir / "test.md").write_text("### Fixed\n- A bug\n")
        self.changelog_file.write_text("# Changelog\n\n## [Unreleased]\n\n## [1.2.0] - 2026-09-01\n")
        ret = main([
            "release",
            "1.2.0",
            "--dir",
            str(self.changelog_dir),
            "--file",
            str(self.changelog_file),
            "--date",
            "2026-09-28",
        ])
        self.assertEqual(ret, 1)
        self.assertTrue((self.changelog_dir / "test.md").exists())


class TestChangelogDirectExecution(unittest.TestCase):
    def test_run_directly_as_script(self):
        # Prevent infinite recursion if child process executes all tests
        if os.environ.get("_CHANGELOG_SUBPROCESS_TEST"):
            return
        env = dict(os.environ, _CHANGELOG_SUBPROCESS_TEST="1")
        result = subprocess.run(
            [sys.executable, str(Path(__file__).resolve())],
            capture_output=True,
            text=True,
            env=env,
        )
        self.assertEqual(
            result.returncode,
            0,
            f"Failed to run test_changelog.py directly via python3:\nSTDOUT:\n{result.stdout}\nSTDERR:\n{result.stderr}",
        )


if __name__ == "__main__":
    unittest.main()
