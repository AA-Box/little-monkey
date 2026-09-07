#!/usr/bin/env python3
"""One-shot branch finalizer for PR #506.

The managed-runtime implementation is committed directly except for a small set
of mechanically-applied source edits that this gate formats, compiles, and tests
before committing the verified production result. The workflow deletes this
bootstrap script afterwards.
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

# The public Runtime Hub service port is assigned before the managed adapter
# starts, but the adapter historically allocated Lily's private child port first.
# On macOS that TOCTOU can return the same supposedly-free port for both. Lily
# then owns the port the parent is about to expose, so the child reports ready
# while the parent never becomes reachable. Explicitly exclude the public port
# from every private child allocation (including the later MLX fallback).
replace_once(
    "packaging/mlx/service/lily_managed.py",
    '''def _free_port() -> int:\n    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:\n        sock.bind(("127.0.0.1", 0))\n        return int(sock.getsockname()[1])\n''',
    '''def _free_port(*, exclude: set[int] | None = None) -> int:\n    excluded = exclude or set()\n    for _attempt in range(32):\n        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:\n            sock.bind(("127.0.0.1", 0))\n            port = int(sock.getsockname()[1])\n        if port not in excluded:\n            return port\n    raise RuntimeError("could not allocate a private managed-runtime port")\n''',
)
replace_once(
    "packaging/mlx/service/lily_managed.py",
    '''        python: Path,\n        startup_timeout: float = STARTUP_TIMEOUT_SECONDS,\n    ) -> None:''',
    '''        python: Path,\n        startup_timeout: float = STARTUP_TIMEOUT_SECONDS,\n        reserved_ports: set[int] | None = None,\n    ) -> None:''',
)
replace_once(
    "packaging/mlx/service/lily_managed.py",
    '''        self._mode = "lily"\n        self._port = _free_port()\n        self._process: subprocess.Popen | None = None''',
    '''        self._mode = "lily"\n        self._reserved_ports = set(reserved_ports or ())\n        self._port = _free_port(exclude=self._reserved_ports)\n        self._process: subprocess.Popen | None = None''',
)
replace_once(
    "packaging/mlx/service/lily_managed.py",
    '''            self._port = _free_port()\n            env = dict(os.environ)''',
    '''            self._port = _free_port(exclude=self._reserved_ports)\n            env = dict(os.environ)''',
)
replace_once(
    "packaging/mlx/service/lily_managed.py",
    '''        Path(args.python),\n        args.startup_timeout_seconds,\n    )''',
    '''        Path(args.python),\n        args.startup_timeout_seconds,\n        reserved_ports={args.port},\n    )''',
)

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

print("PR506 final E2E source patch applied")
