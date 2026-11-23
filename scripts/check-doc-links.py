#!/usr/bin/env python3
"""Check that every relative Markdown link in README.md and docs/*.md resolves.

Validates two things the repository actually relies on:

* the target file exists (case-sensitive, relative to the linking file);
* when a `#fragment` is present, a heading in the target file produces that
  GitHub-style slug.

External links (`http://`, `https://`, `mailto:`) are ignored: CI has no network
and this check is about the docs tree staying navigable.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
HEADING = re.compile(r"^(#{1,6})\s+(.*)$")
INLINE_CODE = re.compile(r"`[^`]*`")


def slug(text: str) -> str:
    """Approximate GitHub's heading anchor: lowercase, drop punctuation, spaces→-."""
    text = re.sub(r"<[^>]+>", "", text)
    text = text.strip().lower()
    text = re.sub(r"[^\w\s-]", "", text, flags=re.UNICODE)
    return re.sub(r"\s", "-", text)


def anchors(path: Path) -> set[str]:
    found: set[str] = set()
    in_fence = False
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.lstrip().startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        match = HEADING.match(line)
        if match:
            found.add(slug(match.group(2)))
    return found


def check(source: Path, problems: list[str]) -> None:
    text = INLINE_CODE.sub("", source.read_text(encoding="utf-8"))
    for target in LINK.findall(text):
        if target.startswith(("http://", "https://", "mailto:")):
            continue
        if target.startswith("#"):
            file_part, fragment = source, target[1:]
        else:
            file_part, _, fragment = target.partition("#")
            file_part = (source.parent / file_part).resolve()
            if not file_part.exists():
                problems.append(f"{source.relative_to(ROOT)}: missing target {target}")
                continue
        if fragment and file_part.suffix == ".md":
            if fragment not in anchors(file_part):
                problems.append(
                    f"{source.relative_to(ROOT)}: missing anchor {target} "
                    f"(no heading slug {fragment!r} in {file_part.relative_to(ROOT)})"
                )


def main() -> int:
    sources = [ROOT / "README.md", *sorted((ROOT / "docs").glob("*.md"))]
    problems: list[str] = []
    for source in sources:
        check(source, problems)
    if problems:
        print("dead documentation links:", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1
    print(f"doc links: {len(sources)} files, all relative targets resolve")
    return 0


if __name__ == "__main__":
    sys.exit(main())
