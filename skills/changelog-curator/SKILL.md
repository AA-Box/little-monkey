---
name: Changelog Curator
description: Draft a CHANGELOG.md entry from recent commits
command: changelog
version: 1.0.0
requires:
  bins: [git]
  env: []
---
Find the last release point: the most recent tag if one exists, otherwise the
commits already reflected in CHANGELOG.md. Read `git log` from that point to HEAD.

Skip merge commits and anything already present in CHANGELOG.md. Group the rest
under Added / Changed / Fixed / Removed headings, following Keep a Changelog
formatting. Each entry should describe user-visible behavior, not implementation
detail — if a commit only touches tests, internal refactors, or CI, leave it out.

Do not invent behavior beyond what the commit messages and their diffs actually
support. If a commit message is too vague to summarize honestly, open the diff
before writing the entry rather than paraphrasing the vague message.

Output the new section as a diff against CHANGELOG.md, not a full file rewrite.
