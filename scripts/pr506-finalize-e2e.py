#!/usr/bin/env python3
"""One-shot branch finalizer for PR #506.

Applies the permanent production wiring in CI so large Rust files and the
managed runtime adapter are formatted/compiled/exercised before the verified
result is committed. The workflow deletes this bootstrap script afterwards.
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

# The managed MLX service is intentionally a lightweight supervisor when Lily
# is active. Measuring only that parent PID would hide the Metal model process
# that actually owns the memory. Reuse the native process-tree meter already
# used by Little Monkey's resource enforcement so Runtime Hub reports the whole
# supervised workload rather than one process in it.
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

# M3 cancellation drops the private streaming HTTP response. Lily itself only
# exposes a blocking completion API, so the adapter must keep the stream alive
# while Lily computes and observe that disconnect. A failed keepalive write is
# cancellation evidence: terminate Lily's child process immediately so GPU work
# really stops, and put the model into a lazy normal-MLX fallback state.
replace_once(
    "packaging/mlx/service/lily_managed.py",
    '''import os\nimport signal''',
    '''import os\nimport queue\nimport signal''',
)
replace_once(
    "packaging/mlx/service/lily_managed.py",
    '''    def close(self) -> None:\n        with self._lock:\n            _terminate(self._process)\n            self._process = None\n\n    def lily_generate(self, request: dict) -> dict:''',
    '''    def close(self) -> None:\n        with self._lock:\n            _terminate(self._process)\n            self._process = None\n\n    def abort_lily_request(self) -> None:\n        \"\"\"Stop in-flight Lily compute after the downstream request is cancelled.\"\"\"\n        with self._lock:\n            if self._mode != \"lily\":\n                return\n            _terminate(self._process)\n            self._process = None\n            self._mode = \"mlx_pending\"\n            sys.stderr.write(\"mlx-service engine=lily reason=request_cancelled status=stopped\\n\")\n            sys.stderr.flush()\n\n    def lily_generate(self, request: dict) -> dict:''',
)
replace_once(
    "packaging/mlx/service/lily_managed.py",
    '''    def _serve_lily(self, request: dict) -> None:\n        self._begin_sse()\n        request_id = str(request.get(\"requestId\", \"\"))\n        self._event({\"type\": \"started\", \"request_id\": request_id})\n        try:\n            response = self._backend().lily_generate(request)\n            choices = response.get(\"choices\") or []\n            content = str((choices[0].get(\"message\") or {}).get(\"content\") or \"\") if choices else \"\"\n            if content:\n                self._event({\"type\": \"text_delta\", \"text\": content})\n            usage = response.get(\"usage\") or {}\n            details = usage.get(\"prompt_tokens_details\") or {}\n            cached = max(0, int(details.get(\"cached_tokens\") or 0))\n            input_tokens = max(0, int(usage.get(\"prompt_tokens\") or 0))\n            output_tokens = max(0, int(usage.get(\"completion_tokens\") or 0))\n            self._event(\n                {\n                    \"type\": \"completed\",\n                    \"input_tokens\": input_tokens,\n                    \"output_tokens\": output_tokens,\n                    \"cached_input_tokens\": cached,\n                }\n            )\n            sys.stderr.write(\n                f\"mlx-service engine=lily request={request_id} input_tokens={input_tokens} \"\n                f\"output_tokens={output_tokens} cached_input_tokens={cached}\\n\"\n            )\n            sys.stderr.flush()\n        except Exception as error:  # noqa: BLE001 - protocol must surface terminal failure\n            self._event({\"type\": \"error\", \"code\": \"generation_failed\", \"message\": str(error)})\n            self._event(\n                {\n                    \"type\": \"completed\",\n                    \"input_tokens\": 0,\n                    \"output_tokens\": 0,\n                    \"cached_input_tokens\": 0,\n                }\n            )\n        self._finish()''',
    '''    def _serve_lily(self, request: dict) -> None:\n        self._begin_sse()\n        request_id = str(request.get(\"requestId\", \"\"))\n        self._event({\"type\": \"started\", \"request_id\": request_id})\n\n        result: queue.Queue[tuple[str, object]] = queue.Queue(maxsize=1)\n\n        def generate() -> None:\n            try:\n                result.put((\"ok\", self._backend().lily_generate(request)))\n            except Exception as error:  # noqa: BLE001 - handed back to request thread\n                result.put((\"error\", RuntimeError(str(error))))\n\n        threading.Thread(target=generate, name=f\"lily-request-{request_id}\", daemon=True).start()\n        try:\n            while True:\n                try:\n                    outcome, payload = result.get(timeout=0.25)\n                    break\n                except queue.Empty:\n                    # A comment is valid SSE and the Rust parser intentionally\n                    # ignores `:` lines. Its write is also our cancellation\n                    # detector while Lily's upstream API remains blocking.\n                    try:\n                        self._chunk(b\": keepalive\\n\\n\")\n                    except (BrokenPipeError, ConnectionResetError):\n                        self._backend().abort_lily_request()\n                        return\n\n            if outcome == \"error\":\n                raise payload if isinstance(payload, Exception) else RuntimeError(str(payload))\n            if not isinstance(payload, dict):\n                raise RuntimeError(\"Lily returned a non-object completion\")\n            response = payload\n            choices = response.get(\"choices\") or []\n            content = str((choices[0].get(\"message\") or {}).get(\"content\") or \"\") if choices else \"\"\n            if content:\n                self._event({\"type\": \"text_delta\", \"text\": content})\n            usage = response.get(\"usage\") or {}\n            details = usage.get(\"prompt_tokens_details\") or {}\n            cached = max(0, int(details.get(\"cached_tokens\") or 0))\n            input_tokens = max(0, int(usage.get(\"prompt_tokens\") or 0))\n            output_tokens = max(0, int(usage.get(\"completion_tokens\") or 0))\n            self._event(\n                {\n                    \"type\": \"completed\",\n                    \"input_tokens\": input_tokens,\n                    \"output_tokens\": output_tokens,\n                    \"cached_input_tokens\": cached,\n                }\n            )\n            self._finish()\n            sys.stderr.write(\n                f\"mlx-service engine=lily request={request_id} input_tokens={input_tokens} \"\n                f\"output_tokens={output_tokens} cached_input_tokens={cached}\\n\"\n            )\n            sys.stderr.flush()\n        except (BrokenPipeError, ConnectionResetError):\n            return\n        except Exception as error:  # noqa: BLE001 - protocol must surface terminal failure\n            try:\n                self._event({\"type\": \"error\", \"code\": \"generation_failed\", \"message\": str(error)})\n                self._event(\n                    {\n                        \"type\": \"completed\",\n                        \"input_tokens\": 0,\n                        \"output_tokens\": 0,\n                        \"cached_input_tokens\": 0,\n                    }\n                )\n                self._finish()\n            except (BrokenPipeError, ConnectionResetError):\n                return''',
)

# Extend the hardware-free managed test to prove that dropping the downstream
# stream while Lily is blocked kills the inference child and causes the next
# request to enter the normal managed-MLX fallback path.
replace_once(
    "packaging/mlx/service/test_lily_managed.py",
    '''import stat\nimport subprocess''',
    '''import stat\nimport struct\nimport subprocess''',
)
replace_once(
    "packaging/mlx/service/test_lily_managed.py",
    '''            import argparse, json\n            parser=argparse.ArgumentParser()\n            parser.add_argument('--model'); parser.add_argument('--bind'); parser.add_argument('--max-seq'); args=parser.parse_args()''',
    '''            import argparse, json, time\n            parser=argparse.ArgumentParser()\n            parser.add_argument('--model'); parser.add_argument('--bind'); parser.add_argument('--max-seq'); args=parser.parse_args()''',
)
replace_once(
    "packaging/mlx/service/test_lily_managed.py",
    '''                    request=json.loads(body)\n                    assert request['model'] == 'Qwen3.6-35B-A3B'\n                    payload=json.dumps({''',
    '''                    request=json.loads(body)\n                    assert request['model'] == 'Qwen3.6-35B-A3B'\n                    if any(message.get('content') == 'block' for message in request.get('messages', [])):\n                        time.sleep(30)\n                    payload=json.dumps({''',
)
replace_once(
    "packaging/mlx/service/test_lily_managed.py",
    '''            assert \"lily-ok\" in lily\n            assert '\"cached_input_tokens\":7' in lily\n\n            fallback_response = _post(''',
    '''            assert \"lily-ok\" in lily\n            assert '\"cached_input_tokens\":7' in lily\n\n            cancel_body = json.dumps(\n                {\n                    \"requestId\": \"cancel-me\",\n                    \"messages\": [{\"role\": \"user\", \"text\": \"block\", \"images\": []}],\n                    \"tools\": [],\n                    \"maxTokens\": 8,\n                },\n                separators=(\",\", \":\"),\n            ).encode()\n            cancelled = socket.create_connection((\"127.0.0.1\", port), timeout=2)\n            cancelled.settimeout(3)\n            cancelled.sendall(\n                (\n                    \"POST /v1/generate HTTP/1.1\\r\\n\"\n                    \"Host: 127.0.0.1\\r\\n\"\n                    \"Content-Type: application/json\\r\\n\"\n                    f\"Content-Length: {len(cancel_body)}\\r\\n\"\n                    \"Connection: close\\r\\n\\r\\n\"\n                ).encode()\n                + cancel_body\n            )\n            observed = b\"\"\n            while b'\"type\":\"started\"' not in observed:\n                chunk = cancelled.recv(4096)\n                assert chunk, \"managed Lily stream closed before started\"\n                observed += chunk\n            cancelled.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack(\"ii\", 1, 0))\n            cancelled.close()\n\n            deadline = time.monotonic() + 6\n            while time.monotonic() < deadline:\n                with urllib.request.urlopen(f\"http://127.0.0.1:{port}/health\", timeout=0.5) as response:\n                    engine = json.load(response)[\"engine\"]\n                if engine == \"mlx_pending\":\n                    break\n                time.sleep(0.05)\n            else:\n                raise AssertionError(\"dropping the stream did not stop Lily compute\")\n\n            fallback_response = _post(''',
)

print("PR506 final E2E patch applied")
