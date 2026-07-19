---
name: Accessibility Auditor
description: Scan JSX/HTML for missing alt text, labels, and ARIA attributes
command: a11y-audit
version: 1.0.0
requires:
  bins: []
  env: []
---
Scan the given files for `<img>` without `alt`, form inputs without an associated `<label>` or `aria-label`, and interactive elements (`onClick` on a `<div>`) that should be a real button or link.

Report each finding with the minimal fix — the goal is correctness, not rewriting the component's structure.
