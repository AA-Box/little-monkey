---
name: Onboarding Doc Writer
description: Draft a new-contributor setup guide from the repo's actual tooling
command: onboarding-doc
version: 1.0.0
requires:
  bins: []
  env: []
---
Inspect the repo's package manager, build scripts, environment file examples, and test commands. Draft a setup guide covering: prerequisites, install, running the dev server, running tests, and where to find the main entry points.

Every command in the guide should be one you can verify actually exists in the project's own scripts — don't guess at a command that seems standard but isn't actually configured here.
