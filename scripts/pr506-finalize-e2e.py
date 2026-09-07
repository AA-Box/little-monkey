#!/usr/bin/env python3
"""One-shot branch finalizer for PR #506.

Applied in CI so the large Rust files are formatted/compiled before the
verified patch is committed. The workflow deletes this script afterwards.
"""

from pathlib import Path
import runpy


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}: {old[:120]!r}")
    p.write_text(text.replace(old, new, 1))


# Apply the already-reviewed Python→Rust→canonical cached-token ABI patch.
runpy.run_path("scripts/pr506-wire-mlx-cache.py", run_name="__main__")

# Lily is an optional accelerator. Even after the conservative router gate, the
# upstream binary remains the final authority on exact architecture/weights. If
# it rejects startup, transparently enter normal MLX instead of making a model
# that worked before this PR fail to load.
replace_once(
    "packaging/mlx/service/lily_managed.py",
    """        self._process: subprocess.Popen | None = None\n        self._start_lily()""",
    """        self._process: subprocess.Popen | None = None\n        try:\n            self._start_lily()\n        except Exception as error:  # Lily validation is stricter than the router gate\n            sys.stderr.write(f\"mlx-service engine=lily startup_failed={error!s} fallback=mlx\\n\")\n            sys.stderr.flush()\n            # RLock makes this safe from inside initialization while preserving\n            # the same transition code used by per-request capability fallback.\n            self.ensure_fallback()""",
)

print("PR506 final E2E patch applied")
