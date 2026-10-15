# Physical headless-agent resource benchmark

This is the first deliberately **real-run** headless memory/latency acceptance
for the installed Little Monkey daemon. Unlike a synthetic model fixture, it
starts actual configured-model runs through `monkey task start`, creates one
disposable read-only-intent workspace per request, attaches to all jobs and
samples memory from the OS process table while the work is executing.

## Running on a prepared Linux/macOS installation

Review the actual installed model, available RAM, daemon settings, and
concurrency limits. Use a disposable development profile. The script neither
installs nor starts the daemon, changes grants, nor switches the selected
model. It may incur provider/model token costs. Use a target you have already
installed and approved.

```sh
pnpm test:headless-resource:contract
pnpm test:headless-resource:live -- \
  --target ollama:YOUR_INSTALLED_MODEL \
  --levels 1,2,5,10 \
  --timeout-seconds 600 \
  --confirm-disposable
```

Options: choose up to six unique concurrency levels in 1..20; 30..3600
seconds maximum per wave. Failed or timed-out test jobs are explicitly
marked, and timeout requests cancellation of ONLY the disposable jobs
created by this script. Temporary workspaces are removed.

The benchmark samples `ps -axo pid,ppid,rss`, builds a recursive subtree
rooted at the running daemon PID, and records **real total RSS in MiB**,
sample counts, per-level completion outcomes, observed p50/p95 elapsed time
and peak memory delta compared with the sampled baseline. A missing daemon
process or unavailable OS sample fails instead of manufacturing a number.

**Limits of what this proves:**

- Memory is the **sum of RSS**, which double counts shared pages; it is NOT
  JCode's PSS, and must not be compared directly against JCode's published
  PSS metrics. It excludes external providers/Ollama/model runtimes outside
  the daemon's process tree.
- We cannot certify CLI task completion solely from process exit; success
  requires a terminal success indicator in the actual `task attach` stream.
  It does not independently score source-accuracy. Use the existing
  `scripts/swarm-model-benchmark.mjs` to run actual executable fixture
  correctness checks.
- Installed model and daemon concurrency restrictions may queue work rather
  than running `N` model turns simultaneously. The output therefore
  measures real requested workload and observed elapsed time, not a promise
  of `N` active GPU kernels.
- The tool prompts agents to do only read-only file inspection, but this is
  not an independent enforcement boundary. Run ONLY on disposable test
  repositories in a prepared development profile with policy-limited tools.
- Running the script is required for benchmark evidence. Passing the offline
  accounting tests demonstrates only the parser and math, not real model
  throughput.

For a fair external JCode comparison, run its own independently documented
harness on the **same machine, model, prompts, cache state and concurrency**,
then collect comparable RSS/PSS and terminal-correctness evidence.
This PR intentionally does not invent that measurement.
