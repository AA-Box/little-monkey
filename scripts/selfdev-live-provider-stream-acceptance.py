#!/usr/bin/env python3
"""Observe a REAL provider stream across an explicitly authorized live daemon upgrade.

Never generates a fake session or provider event. A slow, actually running
model-backed daemon job must already exist; its run_id is passed by operator.
Only metadata (event counts, sequence, PID identities) enters the report.
"""
import argparse
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time

def require(ok,reason):
    if not ok: raise RuntimeError(reason)

def process(args,timeout=360):
    result=subprocess.run(args,text=True,stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,timeout=timeout,check=False)
    require(result.returncode==0,
        "Real supervisor command failed: "+result.stderr[-1200:])
    return json.loads(result.stdout)

def find_ledger(root,run_id):
    manifest=json.loads((root/".supervised-daemon.json").read_text())
    base=Path(manifest["agentHome"]).resolve(strict=True)
    require(base.is_relative_to(root.resolve(strict=True)),
        "Unexpected agent-home outside explicit selfdev managed root")
    matches=[]
    for path in base.rglob("profile-v1.sqlite3"):
        # Inspect DB read-only; never create/repair user ledger by accident.
        with sqlite3.connect("file:"+str(path)+"?mode=ro",uri=True,timeout=2) as conn:
            try:
                found=conn.execute(
                    "SELECT COUNT(*) FROM run_events WHERE run_id=?",(run_id,)).fetchone()[0]
            except sqlite3.Error:
                continue
            if found: matches.append(path)
    require(len(matches)==1,
        "Exactly one real run ledger containing the requested ID is required")
    return matches[0]

def evidence(path,run_id):
    with sqlite3.connect("file:"+str(path)+"?mode=ro",uri=True,timeout=3) as conn:
        rows=conn.execute(
          "SELECT sequence, occurred_at_ms FROM run_events "
          "WHERE run_id=? AND event_type='model_delta' "
          "ORDER BY sequence",(run_id,)).fetchall()
        starts=conn.execute(
          "SELECT COUNT(*) FROM run_events WHERE run_id=? AND event_type='started'",
          (run_id,)).fetchone()[0]
        terminals=conn.execute(
          "SELECT COUNT(*) FROM run_events WHERE run_id=? AND is_terminal=1",
          (run_id,)).fetchone()[0]
    return {"delta_count":len(rows),"last_delta_sequence":rows[-1][0] if rows else 0,
      "last_delta_at_ms":rows[-1][1] if rows else 0,
      "started_events":starts,"terminal_events":terminals}

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument("--root",required=True)
    p.add_argument("--run-id",required=True)
    p.add_argument("--candidate-sha",required=True)
    p.add_argument("--out",required=True)
    p.add_argument("--timeout",type=int,default=600)
    args=p.parse_args()
    require(60<=args.timeout<=1800,"Timeout must be 60-1800 seconds")
    require(len(args.candidate_sha)==64 and
            all(c in "0123456789abcdef" for c in args.candidate_sha),
            "Candidate must be exact lowercase approved SHA-256")
    root=Path(args.root).resolve(strict=True)
    supervisor=Path(__file__).with_name("selfdev-supervised-daemon.mjs")
    def status():
        return process(["node",str(supervisor),"status","--root",str(root)])
    before=status()
    require(before.get("liveHandoff") is True,"Not enrolled for Linux live handoff")
    require(before["daemon"]["active"]>=1 and before["daemon"]["handoff_attested"]>=1,
            "No attested real running worker at baseline")
    ledger=find_ledger(root,args.run_id)
    prev=evidence(ledger,args.run_id)
    require(prev["delta_count"]>=2 and prev["terminal_events"]==0,
            "A provider must already have emitted at least two real deltas and remain active")
    prior_workers=set(before["daemon"]["handoff_worker_ids"])
    upgrade=process(["node",str(supervisor),"upgrade-live",
        "--root",str(root),"--confirm-sha",args.candidate_sha],
        timeout=args.timeout)
    require(upgrade.get("accepted") is True and upgrade.get("liveSessionHandoff") is True,
            "Supervisor did not accept an actual original-worker reattachment")
    new=status()
    require(new["daemon"]["pid"]!=before["daemon"]["pid"],
            "Daemon PID did not change")
    preserved=prior_workers.intersection(new["daemon"]["handoff_worker_ids"])
    require(bool(preserved),"No same original PID/kernel birth identity survived")
    require(new["daemon"]["handoff_adopted_daemon_pid"]==new["daemon"]["pid"],
            "Replacement daemon did not prove adoption of the preserved worker")
    # A post-activation delta must be observed, not merely previously persisted
    # output replayed from SQLite. Never print the actual private stream text.
    activation_ms=int(time.time()*1000)
    deadline=time.monotonic()+args.timeout
    after=None
    while time.monotonic()<deadline:
        after=evidence(ledger,args.run_id)
        if (after["last_delta_sequence"]>prev["last_delta_sequence"] and
            after["last_delta_at_ms"]>=activation_ms and
            after["terminal_events"]>0):
            break
        time.sleep(0.5)
    require(after is not None and
            after["last_delta_sequence"]>prev["last_delta_sequence"] and
            after["last_delta_at_ms"]>=activation_ms,
            "No newly emitted original-provider stream deltas AFTER replacement daemon was healthy")
    require(after["terminal_events"]>0,
            "Original provider run did not reach a durable terminal event")
    require(after["started_events"]==prev["started_events"],
            "The run was restarted or duplicated instead of retaining its streaming worker")
    out=Path(args.out)
    require(not out.exists(),"Refusing to overwrite previous acceptance evidence")
    out.parent.mkdir(parents=True,exist_ok=True)
    result={"accepted":True,"schema":1,"original_run_id":args.run_id,
      "old_daemon_pid":before["daemon"]["pid"],"new_daemon_pid":new["daemon"]["pid"],
      "preserved_workers":sorted(preserved),
      "prior_delta_count":prev["delta_count"],"post_delta_count":after["delta_count"],
      "post_delta_at_ms":after["last_delta_at_ms"],
      "terminal_events":after["terminal_events"],
      "no_additional_started_events":True,
      "evidence":"live-provider-stream-continuity-after-actual-systemd-daemon-replacement"}
    out.write_text(json.dumps(result,indent=2)+"\n")
    print("REAL PROVIDER STREAM HANDOFF ACCEPTED:",out)

if __name__=="__main__":
    try:main()
    except (OSError,ValueError,KeyError,IndexError,sqlite3.Error,
            RuntimeError,subprocess.TimeoutExpired) as e:
        sys.exit("REAL LIVE HANDOFF ACCEPTANCE FAILED: "+str(e))
