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
# starts. Reserve it from every private child allocation and bind the parent
# listener before starting Lily. Also bypass HTTPServer.server_bind: Python's
# HTTPServer performs socket.getfqdn(host) after bind, and reverse DNS for the
# loopback literal can stall on managed macOS hosts even though the socket is
# already listening. A local runtime must never depend on DNS to bind 127.0.0.1.
replace_once(
    "packaging/mlx/service/lily_managed.py",
    '''from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer\nfrom pathlib import Path''',
    '''from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer\nfrom pathlib import Path\nfrom socketserver import TCPServer''',
)
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
replace_once(
    "packaging/mlx/service/lily_managed.py",
    '''class _Handler(BaseHTTPRequestHandler):''',
    '''class _LoopbackHTTPServer(ThreadingHTTPServer):\n    """Threaded loopback server with no reverse-DNS dependency."""\n\n    def server_bind(self) -> None:\n        # HTTPServer.server_bind calls socket.getfqdn(host) after binding. That\n        # is irrelevant for this private 127.0.0.1 ABI and can block startup on\n        # hosts with slow/broken reverse DNS. TCPServer performs only the bind.\n        TCPServer.server_bind(self)\n        host, port = self.server_address[:2]\n        self.server_name = str(host)\n        self.server_port = int(port)\n\n\nclass _Handler(BaseHTTPRequestHandler):''',
)
replace_once(
    "packaging/mlx/service/lily_managed.py",
    '''    server: ThreadingHTTPServer | None = None\n    stopping = threading.Event()''',
    '''    # Own the externally assigned Runtime Hub port before any child process\n    # starts. The listener is activated now but `serve_forever` begins only after\n    # the selected heavy backend is genuinely ready, so a successful health\n    # response can never mean "Lily is still loading".\n    _Handler.backend = backend\n    server: _LoopbackHTTPServer | None = _LoopbackHTTPServer((args.host, args.port), _Handler)\n    stopping = threading.Event()''',
)
replace_once(
    "packaging/mlx/service/lily_managed.py",
    '''    try:\n        backend.start()\n        _Handler.backend = backend\n        server = ThreadingHTTPServer((args.host, args.port), _Handler)\n        sys.stderr.write(f"mlx-service listening on {args.host}:{args.port} engine={backend.mode}\\n")\n        sys.stderr.flush()\n        server.serve_forever()''',
    '''    try:\n        backend.start()\n        sys.stderr.write(f"mlx-service listening on {args.host}:{args.port} engine={backend.mode}\\n")\n        sys.stderr.flush()\n        server.serve_forever()''',
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
