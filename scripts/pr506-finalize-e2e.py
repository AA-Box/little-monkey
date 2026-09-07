#!/usr/bin/env python3
"""One-shot branch finalizer for PR #506.

The Python managed-runtime paths are committed directly. This script exists
only to apply the large Rust ABI/telemetry changes in CI so they can be
formatted, compiled, and tested before the verified result is committed.
The workflow deletes this bootstrap script afterwards.
"""

import runpy
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    target = Path(path)
    text = target.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}: {old[:120]!r}")
    target.write_text(text.replace(old, new, 1))


runpy.run_path("scripts/pr506-wire-mlx-cache.py", run_name="__main__")

# When Lily is active, the supervised Python service is only a lightweight
# parent and the model memory belongs to its child. Reuse Little Monkey's native
# process-tree measurement so Runtime Hub reports the full managed workload.
replace_once(
    "src-tauri/src/m3_production.rs",
    '''// `ps`-based resident-memory sampling, reached only from MLX metrics.\n#[cfg(target_os = "macos")]\nuse std::process::Command;\n''',
    '',
)
replace_once(
    "src-tauri/src/m3_production.rs",
    '''// MLX metrics only; macOS is always unix, so there is no non-unix variant to\n// keep alive here.\n#[cfg(target_os = "macos")]\nfn process_resident_memory_bytes(pid: u32) -> Option<u64> {\n    let output = Command::new("ps")\n        .args(["-o", "rss=", "-p", &pid.to_string()])\n        .output()\n        .ok()?;\n    if !output.status.success() {\n        return None;\n    }\n    String::from_utf8(output.stdout)\n        .ok()?\n        .trim()\n        .parse::<u64>()\n        .ok()?\n        .checked_mul(1_024)\n}\n''',
    '''// MLX may supervise an inference child (for example Lily). Report the native\n// tree footprint so Runtime Hub memory accounting follows the workload rather\n// than only the lightweight service parent.\n#[cfg(target_os = "macos")]\nfn process_resident_memory_bytes(pid: u32) -> Option<u64> {\n    let usage = crate::process_tree::measure_tree(pid).ok().flatten()?;\n    if usage.unmeasured_members != 0 {\n        return None;\n    }\n    usage.rss_bytes\n}\n''',
)

print("PR506 final E2E Rust patch applied")
