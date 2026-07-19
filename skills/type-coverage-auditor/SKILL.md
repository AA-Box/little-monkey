---
name: Type Coverage Auditor
description: Find untyped escape hatches in a TypeScript codebase
command: type-coverage
version: 1.0.0
requires:
  bins: []
  env: []
---
Grep the changed files (or a given directory) for `any`, `as unknown as`, `@ts-ignore`, and `@ts-expect-error`. For each hit, note whether the escape hatch looks load-bearing (genuinely hard to type) or lazy (a real type was available nearby).

Rank findings by how much surface area they touch — an `any` on an exported function signature matters more than one in a test file.
