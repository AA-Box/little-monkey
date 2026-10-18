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
pnpm test:headless-resource:os  # Linux: physical kernel PSS for this stable Node process
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

The OS-only CI smoke intentionally measures the currently running Node
process (a stable PID), because build helpers can exit between `ps` and
`/proc` sampling. It verifies the real kernel PSS path and parser but
**does not** claim a complete daemon/workload memory sample.

Each disposable workspace is now initialized as a **real Git repository**
with a committed fixture before it is handed to `monkey task start`.
Native autonomous tasks require that Git HEAD and workspace revision.
After all model outcomes the harness independently compares the original
HEAD and complete tracked/untracked checkout state. Any edited, newly created
or committed workspace file turns that task into a failed acceptance, even
when the model claims it completed correctly. These verification checks
protect the measurement; they do **not** make the native task sandbox
read-only on their own.

The benchmark samples `ps -axo pid,ppid,rss`, builds a recursive subtree
rooted at the running daemon PID, and records **real total RSS in MiB**,
sample counts, per-level completion outcomes, observed p50/p95 elapsed time
and peak memory delta compared with the sampled baseline. On **Linux only**,
it additionally samples real `/proc/PID/smaps_rollup` `Pss:` for each process
in the daemon tree. **peakPssMiB is null** unless every process in every
observed sample has a valid OS PSS reading; `completePssSamples` and
`totalRssSamples` show coverage. A missing daemon or unavailable RSS
fails instead of manufacturing a number.

**Limits of what this proves:**

- Memory totals use **sum of RSS**, which double counts shared pages.
  `peakPssMiB` (when non-null) is the **sum of measured Linux process-tree
  proportional set sizes**, comparable in definition to JCode's Linux PSS,
  **not** a controlled head-to-head benchmark. Neither metric includes
  external providers/Ollama/model runtimes outside the daemon's process tree.
  On macOS and incomplete Linux `smaps_rollup` sampling, PSS is null.
- Task completion requires both `succeeded` in the durable run ledger
  **and** a matching, real assistant model-delta event containing the known
  disposable fixture marker. Arbitrary model text claiming "success" or a
  process exit code alone is insufficient. This source marker is a narrow
  task correctness oracle, not a comprehensive independent quality score. Use the existing
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
