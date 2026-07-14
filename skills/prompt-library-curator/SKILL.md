---
name: Prompt Library Curator
description: Organize saved prompts and personas into a searchable library
command: prompt-library
version: 1.0.0
requires:
  bins: []
  env: []
---
Scan the workspace's saved prompts and personas. Group them by task type (for
example: code review, writing, research, data extraction), and propose 2-4
keyword tags per entry.

Flag near-duplicates — prompts that differ only cosmetically — as merge
candidates, and name which one you'd keep as the canonical version and why. Flag
anything unused for 90+ days as an archive candidate.

This skill only proposes. It does not delete, merge, or archive anything on its
own — present the reorganization as a plan and wait for explicit approval before
touching a single saved prompt.
