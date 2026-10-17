# Controlled *physical* JCode / Little Monkey headless comparison

The runner starts **no mock agents**. It requires authenticated, independently
running JCode and Little Monkey resident processes on the same Linux host,
and submits the exact same read-only fixture prompt to each.

## Prepare

Install both agent runtimes, configure a genuinely identical model target,
then manually run their native services. For JCode, enable its official
debug control socket (as used in JCode's own
\`scripts/bench_headless_memory.py\`) on a temporary authenticated
development instance. For Little Monkey, use the installed resident daemon
and \`monkey-cli task start/status/attach\` commands.

Both services must be operating in **a disposable development profile with
approved model billing**. JCode's debug socket allows agent commands;
bind it to a private local Unix socket, do not expose it over the network.
Never upload OAuth tokens, provider secrets or native debug command logs.

\`\`\`sh
python3 scripts/paired-jcode-littlemonkey-live.py \
  --jcode-debug-socket /private/path/to/jcode-debug.sock \
  --jcode-pid ACTUAL_JCODE_SERVER_PID \
  --jcode-model EXACT_SAME_MODEL_IDENTIFIER \
  --jcode-backend EXACT_SAME_BACKEND_IDENTIFIER \
  --monkey-cli /absolute/path/to/monkey-cli \
  --little-monkey-target EXACT_SAME_MODEL_IDENTIFIER \
  --little-monkey-backend EXACT_SAME_BACKEND_IDENTIFIER \
  --levels 1,2,5 \
  --output ./paired-headless-report.json \
  --confirm-live-model-costs
\`\`\`

One independently generated temporary workspace and random secret-free
marker is shared by both runtimes per wave. Each native tool gets the
same exact prompt and marker. JCode success is checked against the real
session's \`last_response\`, Little Monkey success against its independently
verified durable \`succeeded\` ledger status **and model-delta events**.
The runner fails if either modifies the disposable fixture.

PSS comes from genuine Linux \`/proc/PID/smaps_rollup\` for each daemon and
its process descendants; missing/denied measurements are **null**, never
manufactured. The sampler reads the kernel process table directly, retries
short-lived process races, and rejects an incomplete tree. CLI calls run in
isolated process groups that are reaped on timeout or excess output; the two
pre-existing services remain under the operator's control. Warming advantage
is reduced by alternating wave order.
It reports observed completed requests, p50 completion and sampled peak
PSS in JSON on stdout and, with \`--output\`, an atomically written file, with
no invented baseline or model cost normalization.

The two model identifiers and the two operator-declared backend identifiers
must match exactly before any workload starts. The harness cannot inspect a
provider's internal model revision or remote billing account. Verify those
settings, hardware, tool permissions, cache state and source revisions on both
services before treating numbers as comparable.

A real run succeeds only with exit code 0, \`status: "PASS"\`,
\`verifiedLiveRuns: true\`, zero workload failures, and a non-null peak PSS for
every wave on both sides. Any malformed event, incomplete task status,
mutation of the committed fixture, missing process sample, or configuration
mismatch is a failure. The JSON report records the completed counts, timing,
PSS sample counts, and peak memory for each level.

**Limitations:** This is a *one-turn* real-tool workload, not an identical
five-turn stateful-session benchmark, and different runtime architectures
may include different processes in the sampled trees. Processes outside
the daemon tree, notably externally hosted model inference, are excluded.
Passing the runner proves its selected fixture path only, not general
quality superiority. It explicitly refuses to pass if the model/backend
identifiers differ or any PSS snapshot is incomplete.

The operator must inspect the actual model IDs/configuration, cache state,
CPU/GPU and backend accounting to establish strict comparability. Do not
compare numbers from different hardware, model versions, tool grants or
source revisions.

\`\`\`sh
pnpm test:paired-headless:contract
\`\`\`

The default CI checks actual Linux kernel PSS access and strict report
handling. It runs an end-to-end \`--simulation\` with controlled local socket
and CLI processes, committed Git fixtures, real kernel PSS, and malformed
event injection. Its report can show \`status: "PASS"\` but always has
\`verifiedLiveRuns: false\`. CI DOES NOT run model inference or claim real JCode parity.
Real physical runs require both installed runtimes and authorized model
credentials in the operator environment.
