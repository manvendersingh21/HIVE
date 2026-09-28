#!/usr/bin/env python3
"""Manage changelog fragments for HIVE.

Subcommands:
  check              Validate fragment files (heading, non-empty, no duplicate names).
  release <version>  Fold fragments into CHANGELOG.md under a new version heading
                     and delete the fragments.
"""

from __future__ import annotations

import argparse
import datetime
import os
from pathlib import Path
import re
import sys
from typing import Dict, List, Optional, Sequence

VALID_HEADINGS = [
    "Added",
    "Changed",
    "Deprecated",
    "Removed",
    "Fixed",
    "Security",
]

DEFAULT_REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_CHANGELOG_DIR = DEFAULT_REPO_ROOT / "changelog.d"
DEFAULT_CHANGELOG_FILE = DEFAULT_REPO_ROOT / "CHANGELOG.md"


class ChangelogError(Exception):
    """Base error for changelog management."""


class ValidationError(ChangelogError):
    """Raised when fragment validation fails."""


def find_fragments(changelog_dir: Path) -> List[Path]:
    """Return all non-hidden fragment files in changelog_dir."""
    if not changelog_dir.exists():
        return []
    fragments: List[Path] = []
    for entry in changelog_dir.iterdir():
        if entry.is_file() and not entry.name.startswith("."):
            fragments.append(entry)
    return sorted(fragments, key=lambda p: p.name)


def parse_fragment(path: Path) -> Dict[str, List[str]]:
    """Parse and validate a single fragment file.

    Returns a mapping of heading -> list of content blocks.
    Raises ValidationError if heading is invalid, content is empty, etc.
    """
    try:
        raw_text = path.read_text(encoding="utf-8")
    except Exception as exc:
        raise ValidationError(f"Could not read fragment file '{path.name}': {exc}") from exc

    stripped = raw_text.strip()
    if not stripped:
        raise ValidationError(f"Fragment file '{path.name}' is empty.")

    heading_pattern = re.compile(r"^#{1,6}\s+(.+)$")
    valid_map = {h.lower(): h for h in VALID_HEADINGS}

    sections: Dict[str, List[str]] = {}
    current_heading: Optional[str] = None
    current_lines: List[str] = []

    lines = raw_text.splitlines()
    found_any_heading = False

    for line in lines:
        match = heading_pattern.match(line)
        if match:
            found_any_heading = True
            raw_heading = match.group(1).strip()
            norm = raw_heading.lower()
            if norm not in valid_map:
                allowed_str = ", ".join(VALID_HEADINGS)
                raise ValidationError(
                    f"Fragment file '{path.name}' has invalid heading '{raw_heading}'. "
                    f"Expected one of: {allowed_str}"
                )
            if current_heading is not None:
                content = "\n".join(current_lines).strip()
                if not content:
                    raise ValidationError(
                        f"Fragment file '{path.name}' has heading '{current_heading}' with no content."
                    )
                sections.setdefault(current_heading, []).append(content)
                current_lines = []

            current_heading = valid_map[norm]
        else:
            if current_heading is None:
                # Text before any heading
                if line.strip():
                    allowed_str = ", ".join(VALID_HEADINGS)
                    raise ValidationError(
                        f"Fragment file '{path.name}' contains text before a section heading. "
                        f"Each fragment must begin with a heading such as: {allowed_str}"
                    )
            else:
                current_lines.append(line)

    if not found_any_heading:
        allowed_str = ", ".join(VALID_HEADINGS)
        raise ValidationError(
            f"Fragment file '{path.name}' does not contain any section heading. "
            f"Expected one of: {allowed_str}"
        )

    if current_heading is not None:
        content = "\n".join(current_lines).strip()
        if not content:
            raise ValidationError(
                f"Fragment file '{path.name}' has heading '{current_heading}' with no content."
            )
        sections.setdefault(current_heading, []).append(content)

    return sections


def check_fragments(changelog_dir: Path) -> List[str]:
    """Validate all fragment files in changelog_dir.

    Returns a list of error strings. Empty list indicates success.
    """
    if not changelog_dir.exists():
        return [f"Changelog directory '{changelog_dir}' does not exist."]

    fragments = find_fragments(changelog_dir)
    if not fragments:
        return []

    errors: List[str] = []
    seen_names: Dict[str, str] = {}

    for fragment in fragments:
        lower_name = fragment.name.lower()
        if lower_name in seen_names:
            errors.append(
                f"Duplicate fragment name: '{fragment.name}' collides with '{seen_names[lower_name]}'."
            )
        else:
            seen_names[lower_name] = fragment.name

        try:
            parse_fragment(fragment)
        except ValidationError as exc:
            errors.append(str(exc))

    return errors


def format_release_block(
    version: str,
    sections: Dict[str, List[str]],
    release_date: Optional[str] = None,
) -> str:
    """Format the folded release block for CHANGELOG.md."""
    clean_version = version.lstrip("v")
    date_str = release_date or datetime.date.today().isoformat()
    lines: List[str] = [f"## [{clean_version}] - {date_str}", ""]

    ordered_keys = [h for h in VALID_HEADINGS if h in sections]
    # Add any custom keys if present
    for k in sections:
        if k not in ordered_keys:
            ordered_keys.append(k)

    for heading in ordered_keys:
        blocks = sections[heading]
        lines.append(f"### {heading}")
        lines.append("")
        for block in blocks:
            lines.append(block)
        lines.append("")

    return "\n".join(lines).rstrip() + "\n"


def release(
    version: str,
    changelog_dir: Path,
    changelog_file: Path,
    release_date: Optional[str] = None,
) -> None:
    """Fold fragments into CHANGELOG.md under new version heading and delete fragments."""
    errors = check_fragments(changelog_dir)
    if errors:
        raise ChangelogError("Cannot release with invalid fragments:\n" + "\n".join(errors))

    fragments = find_fragments(changelog_dir)
    if not fragments:
        raise ChangelogError(f"No fragments found in '{changelog_dir}' to release.")

    if not changelog_file.exists():
        raise ChangelogError(f"Changelog file '{changelog_file}' does not exist.")

    combined_sections: Dict[str, List[str]] = {}
    for fragment in fragments:
        parsed = parse_fragment(fragment)
        for heading, blocks in parsed.items():
            combined_sections.setdefault(heading, []).extend(blocks)

    release_block = format_release_block(version, combined_sections, release_date)
    changelog_text = changelog_file.read_text(encoding="utf-8")

    unreleased_pattern = re.compile(r"^(##\s+\[Unreleased\]\s*)$", re.MULTILINE)
    match = unreleased_pattern.search(changelog_text)

    if match:
        end_pos = match.end()
        new_changelog = (
            changelog_text[:end_pos]
            + "\n\n"
            + release_block
            + "\n"
            + changelog_text[end_pos:].lstrip("\n")
        )
    else:
        # If no [Unreleased] heading, prepend before first version heading or append
        first_version = re.search(r"^(##\s+\[.+\])", changelog_text, re.MULTILINE)
        if first_version:
            start_pos = first_version.start()
            new_changelog = (
                changelog_text[:start_pos]
                + release_block
                + "\n"
                + changelog_text[start_pos:]
            )
        else:
            new_changelog = changelog_text.rstrip() + "\n\n" + release_block

    changelog_file.write_text(new_changelog, encoding="utf-8")

    # Delete fragments after successful release update
    for fragment in fragments:
        try:
            fragment.unlink()
        except OSError as exc:
            raise ChangelogError(f"Failed to delete fragment '{fragment.name}': {exc}") from exc


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="changelog.py",
        description="Manage changelog fragments.",
    )
    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    check_parser = subparsers.add_parser(
        "check",
        help="Validate changelog fragments (heading, non-empty, no duplicate names).",
    )
    check_parser.add_argument(
        "--dir",
        dest="changelog_dir",
        type=Path,
        default=DEFAULT_CHANGELOG_DIR,
        help="Directory containing changelog fragments (default: changelog.d)",
    )

    release_parser = subparsers.add_parser(
        "release",
        help="Fold fragments into CHANGELOG.md under new version heading and delete fragments.",
    )
    release_parser.add_argument(
        "version",
        help="Release version string, e.g. 1.0.0 or v1.0.0",
    )
    release_parser.add_argument(
        "--date",
        dest="release_date",
        help="Release date in YYYY-MM-DD format (default: today)",
    )
    release_parser.add_argument(
        "--dir",
        dest="changelog_dir",
        type=Path,
        default=DEFAULT_CHANGELOG_DIR,
        help="Directory containing changelog fragments (default: changelog.d)",
    )
    release_parser.add_argument(
        "--file",
        dest="changelog_file",
        type=Path,
        default=DEFAULT_CHANGELOG_FILE,
        help="Path to CHANGELOG.md (default: CHANGELOG.md in repository root)",
    )

    args = parser.parse_args(argv)

    if args.subcommand == "check":
        errors = check_fragments(args.changelog_dir)
        if errors:
            print("Changelog check failed:", file=sys.stderr)
            for err in errors:
                print(f"  - {err}", file=sys.stderr)
            return 1
        fragments = find_fragments(args.changelog_dir)
        if fragments:
            print(f"Changelog check passed: {len(fragments)} fragment(s) validated.")
        else:
            print("Changelog check passed: 0 fragments found.")
        return 0

    if args.subcommand == "release":
        try:
            release(
                version=args.version,
                changelog_dir=args.changelog_dir,
                changelog_file=args.changelog_file,
                release_date=args.release_date,
            )
            print(f"Successfully released version {args.version} and removed fragments.")
            return 0
        except ChangelogError as exc:
            print(f"Changelog release failed: {exc}", file=sys.stderr)
            return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
