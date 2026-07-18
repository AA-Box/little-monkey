---
name: API Reference Generator
description: Produce reference docs from exported function signatures and doc comments
command: api-reference
version: 1.0.0
requires:
  bins: []
  env: []
---
For each exported function, class, or type in the given module, extract its signature and existing doc comment. Produce a reference page grouping them by file, with parameters, return type, and a one-line description.

Where a doc comment is missing, write the description from what the code actually does — never invent behavior the implementation doesn't have.
