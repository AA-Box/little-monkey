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
  --monkey-cli /absolute/path/to/monkey-cli \
  --little-monkey-target EXACT_SAME_MODEL_IDENTIFIER \
  --levels 1,2,5 \
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
manufactured. Warming advantage is reduced by alternating wave order.
It reports observed completed requests, p50 completion and sampled peak
PSS, with no invented baseline or model cost normalization.

**Limitations:** This is a *one-turn* real-tool workload, not an identical
five-turn stateful-session benchmark, and different runtime architectures
may include different processes in the sampled trees. Processes outside
the daemon tree, notably externally hosted model inference, are excluded.
Passing the runner proves its selected fixture path only, not general
quality superiority. It explicitly refuses to pass if the two exact
model ID strings differ or any PSS snapshot is incomplete.

The operator must inspect the actual model IDs/configuration, cache state,
CPU/GPU and backend accounting to establish strict comparability. Do not
compare numbers from different hardware, model versions, tool grants or
source revisions.

\`\`\`sh
pnpm test:paired-headless:contract
\`\`\`

The default CI checks actual Linux kernel PSS access and strict report
handling, but DOES NOT run model inference or claim real JCode parity.
Real physical runs require both installed runtimes and authorized model
credentials in the operator environment.
