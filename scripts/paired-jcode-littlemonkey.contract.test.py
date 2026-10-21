#!/usr/bin/env python3
"""Offline contract tests plus actual OS kernel sampler; no fake model pass."""
import os
import json
from pathlib import Path
import runpy
import signal
import subprocess
import sys
import tempfile
import time
import unittest

MODULE = runpy.run_path(str(Path(__file__).with_name(
    "paired-jcode-littlemonkey-live.py"
)))


class PhysicalComparisonContracts(unittest.TestCase):
    def test_real_linux_self_process_kernel_pss(self):
        if not Path("/proc").is_dir():
            self.skipTest("Linux OS PSS required")
        # The process tree includes a short-lived 'ps' child in its snapshot.
        # Linux may reap that PID before we read /proc: root-only PSS tests
        # the actual kernel path without fabricating a complete tree.
        measured = MODULE["read_process_pss_kib"](os.getpid())
        self.assertIsInstance(measured, int)
        self.assertGreater(measured, 0)

    def test_fixture_digest_rejects_mutation_and_symlinks(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / "fixture.txt").write_text("Original marker\n")
            digest = MODULE["fixture_digest"](root)
            self.assertEqual(MODULE["fixture_digest"](root), digest)
            (root / "fixture.txt").write_text("Changed marker\n")
            self.assertNotEqual(MODULE["fixture_digest"](root), digest)
            (root / "fixture.txt").write_text("Original marker\n")
            (root / "empty-directory").mkdir()
            self.assertNotEqual(MODULE["fixture_digest"](root), digest)
            (root / "bad-link").symlink_to("fixture.txt")
            with self.assertRaisesRegex(RuntimeError, "symlink"):
                MODULE["fixture_digest"](root)

    def test_incomplete_memory_evidence_never_yields_a_pss_peak(self):
        globals_ = MODULE["wave"].__globals__
        old = globals_["live_pss_kib"]
        try:
            globals_["live_pss_kib"] = lambda _: None
            result = MODULE["wave"](1, {"jcode": 123}, "jcode",
                                   [lambda: {"completed": True, "seconds": .4}], 30)
            self.assertIsNone(result["peakPssMiB"])
        finally:
            globals_["live_pss_kib"] = old

    def test_command_caps_output_and_reaps_timeout(self):
        with self.assertRaisesRegex(RuntimeError, "unbounded output"):
            MODULE["command"]([sys.executable, "-c", "print('x'*2100000)"], timeout=2)
        with self.assertRaises(TimeoutError):
            MODULE["command"]([sys.executable, "-c", "import time; time.sleep(5)"], timeout=.1)

    def test_malformed_process_samples_are_incomplete(self):
        if not Path("/proc").is_dir():
            self.skipTest("Linux OS PSS required")
        self.assertIsNone(MODULE["read_process_pss_kib"](-1))
        self.assertIsNone(MODULE["live_pss_kib"](-1))

    def test_end_to_end_simulated_process_contract(self):
        if not Path("/proc").is_dir():
            self.skipTest("Linux process-tree PSS required")
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            socket_path = root / "jcode.sock"
            cli = root / "monkey-mock"
            state = root / "state"
            state.mkdir()
            cli.write_text('''#!/usr/bin/env python3
import json, os, sys, uuid
from pathlib import Path
args=sys.argv[1:]
root=Path(os.environ['LM_MOCK_STATE'])
if args[:2]==['daemon','status']:
    print(json.dumps({'installed':True,'service_running':True,
      'heartbeat_fresh':True,'pid':int(os.environ['LM_MOCK_DAEMON_PID'])}))
elif args[:2]==['task','start']:
    folder=Path(args[args.index('--workspace')+1])
    marker=(folder/'fixture.txt').read_text().strip().split(': ',1)[1]
    run_id='task-'+uuid.uuid4().hex
    (root/run_id).write_text(marker)
    print(json.dumps({'run_id':run_id}))
elif args[:2]==['task','attach']:
    if os.environ.get('LM_MOCK_MODE')=='bad-event': print('{bad')
    else:
        print(json.dumps({'run_id':args[2],'event':{'type':'model_delta',
          'payload':{'channel':'assistant','text':(root/args[2]).read_text()}}}))
elif args[:2]==['task','status']:
    run_id=args[args.index('--run-id')+1]
    print(json.dumps([{'run_id':run_id,'status':'succeeded'}]))
else: sys.exit(2)
''')
            cli.chmod(0o700)
            server_code = '''import json, socketserver, sys, uuid
from pathlib import Path
sessions={}
class Handler(socketserver.StreamRequestHandler):
  def handle(self):
    request=json.loads(self.rfile.readline())
    op=request['command']; sid=request.get('session_id')
    if op.startswith('create_session:'):
      sid=uuid.uuid4().hex
      sessions[sid]=(Path(op.split(':',1)[1])/'fixture.txt').read_text().strip().split(': ',1)[1]
      result=json.dumps({'session_id':sid})
    elif op.startswith('set_model:') or op.startswith('message:'):
      result='ok' if sid in sessions else ''
    elif op=='last_response': result=sessions.get(sid,'')
    else: result=''
    self.wfile.write((json.dumps({'type':'debug_response','ok':bool(result),'output':result})+'\\n').encode())
class Server(socketserver.ThreadingUnixStreamServer): daemon_threads=True
Server(sys.argv[1],Handler).serve_forever()
'''
            server = subprocess.Popen([sys.executable, "-u", "-c", server_code, str(socket_path)],
                                      start_new_session=True, stdout=subprocess.DEVNULL,
                                      stderr=subprocess.DEVNULL)
            daemon = subprocess.Popen([sys.executable, "-c", "import subprocess,sys,time; "
                                       "subprocess.Popen([sys.executable,'-c',"
                                       "'import time;a=bytearray(8*1024*1024);time.sleep(60)']); "
                                       "time.sleep(60)"],
                                      start_new_session=True, stdout=subprocess.DEVNULL,
                                      stderr=subprocess.DEVNULL)
            try:
                deadline = time.monotonic() + 5
                while not socket_path.exists() and time.monotonic() < deadline:
                    time.sleep(.01)
                self.assertTrue(socket_path.exists(), "mock socket did not start")
                deadline = time.monotonic() + 3
                root_pss = tree_pss = 0
                while time.monotonic() < deadline:
                    root_pss = MODULE["read_process_pss_kib"](daemon.pid)
                    tree_pss = MODULE["live_pss_kib"](daemon.pid)
                    if root_pss and tree_pss and tree_pss > root_pss + 2048:
                        break
                    time.sleep(.02)
                self.assertGreater(tree_pss, root_pss + 2048,
                                   "process-tree PSS must include the daemon child")
                env = {**os.environ, "LM_MOCK_STATE": str(state),
                       "LM_MOCK_DAEMON_PID": str(daemon.pid)}
                argv = [sys.executable, str(Path(__file__).with_name(
                    "paired-jcode-littlemonkey-live.py")),
                    "--jcode-debug-socket", str(socket_path), "--jcode-pid", str(server.pid),
                    "--jcode-model", "same-model", "--little-monkey-target", "same-model",
                    "--jcode-backend", "same-backend", "--little-monkey-backend", "same-backend",
                    "--monkey-cli", str(cli), "--levels", "1,2", "--simulation",
                    "--output", str(root / "report.json")]
                result = subprocess.run(argv, text=True, capture_output=True, timeout=30, env=env)
                self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
                report = json.loads(result.stdout)
                self.assertEqual(report, json.loads((root / "report.json").read_text()))
                self.assertEqual(report["status"], "PASS")
                self.assertFalse(report["verifiedLiveRuns"])
                self.assertTrue(all(row[who]["peakPssMiB"] > 0
                    for row in report["results"] for who in ("jcode", "little_monkey")))
                bad = subprocess.run(argv, text=True, capture_output=True, timeout=30,
                                     env={**env, "LM_MOCK_MODE": "bad-event"})
                self.assertEqual(bad.returncode, 1)
                self.assertEqual(json.loads(bad.stdout)["status"], "FAILED")
                mismatch = subprocess.run([*argv, "--little-monkey-backend", "different"],
                                          text=True, capture_output=True, timeout=5, env=env)
                self.assertNotEqual(mismatch.returncode, 0)
            finally:
                for child in (server, daemon):
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=5)


if __name__ == "__main__":
    unittest.main()
