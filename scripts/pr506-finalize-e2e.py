#!/usr/bin/env python3
"""One-shot branch finalizer for PR #506.

Applies the permanent Python→Rust→canonical cached-token ABI patch in CI so
large Rust files are formatted, compiled, and exercised before the verified
result is committed. The workflow deletes this bootstrap script afterwards.
"""

import runpy


runpy.run_path("scripts/pr506-wire-mlx-cache.py", run_name="__main__")
print("PR506 final E2E patch applied")
