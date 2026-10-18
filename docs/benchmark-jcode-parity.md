# Controlled Little Monkey versus JCode real-model acceptance

**This is an execution harness, not a benchmark result.** It performs two genuine
repeats at 1, 5 and 10 concurrent sessions, five sequential turns per session,
on the same workspace and host. Every turn uses the same prompt and requested
model identifier, and Linux PSS is summed over the full process trees
(including resident daemon and CLI children). A process crash, timeout,
missing debug socket, invalid run, or unreadable PSS aborts the experiment;
no successful summary is written.

On a Linux machine, install both tools and authenticate both **to the same
authorized API model/provider**, then run:

```sh
python3 scripts/test-bench-jcode-parity.py
python3 scripts/bench-jcode-parity.py \
  --workspace /path/to/your/checkout \
  --model gpt-5.4 --provider openai \
  --monkey /absolute/path/to/monkey-cli \
  --jcode /absolute/path/to/jcode \
  --output /tmp/little-monkey-jcode-parity.json
```

The model name is an **example**, not a statement that access is configured.
Do not publish token-bearing logs. The script fails if either CLI is missing
or any run fails. Memory is disabled in JCode. Little Monkey uses
`--no-rules --no-mcp --permission-mode plan` and a matching explicit model;
review JCode's provider mapping and Little Monkey's provider settings to
ensure both resolve to the **same vendor model, deployment and inference
configuration**. JCode model selection is not independently attested by the
harness; a matching model argument alone is not sufficient proof. Compare
provider request receipts and supported model metadata before accepting parity.

One JCode resident daemon hosts all concurrent sessions; Little Monkey uses
one persistent headless REPL per session, reflecting their actual process
architectures. Each Little Monkey turn emits a **content-free** completion
marker on stderr only when `LITTLE_MONKEY_BENCH_EVENTS=1`; the Python harness
refuses failed turns and verifies that native tool-result records appeared.
JCode uses its opt-in local debug socket and verifies session history length.
The process trees are independently measured after every round, and process
failures invalidate the entire report.

Important remaining limitations: real installations and credentials are not
available in this authoring environment; no performance numbers have been
measured. JCode debug control and CLI versions may change, and require live
compatibility verification. The benchmark does not yet independently verify
each product's provider usage/billing receipts or normalise unrelated
feature toggles. Do not describe numerical parity until the complete live
run and configuration review have succeeded.
