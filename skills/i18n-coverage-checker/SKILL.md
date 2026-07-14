---
name: i18n Coverage Checker
description: Find hardcoded user-facing strings missing from translation keys
command: i18n-coverage
version: 1.0.0
requires:
  bins: []
  env: []
---
Scan the given files for string literals in JSX text nodes, `alt`, `placeholder`, and `title` attributes that aren't already routed through the project's translation function.

Skip strings that are clearly not user-facing (class names, test ids, internal log messages). Report each candidate with its file:line and suggested key name.
