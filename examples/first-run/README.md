# First-run coding task: verify a real fix

This small example is **intentionally broken**. It contains a JavaScript function and three Node.js tests. One test fails on the checked-in version because runs of spaces turn into multiple hyphens.

This directory is disposable example code, not a production feature or a passing repository test fixture. Use **Node.js 20+** for the example only; Little Monkey's installers do not require Node.js.

## Reproduce the bug

From this directory:

```sh
node --test normalize.test.mjs
```

Expected baseline: **2 passing tests, 1 failing test**. The failure expects `ada-lovelace` but receives `ada---lovelace`. If all three tests already pass, the example may have been edited, and is no longer a clean baseline.

## Give the agent the task

Open this directory as a workspace in Little Monkey, select a working model, and submit:

> Fix `normalizeUserId` so repeated spaces, tabs and other whitespace between words become exactly one hyphen. Preserve the existing rejection of non-string input. Add test coverage for tabs, run `node --test normalize.test.mjs`, and show the exact file diff and test result.

Review every requested permission before approving commands or edits. A model may fail or need correction; do not count the attempt as successful solely because it says "done."

To use the daemon CLI instead, after setting up an Ollama model replace `<model>` and `<absolute-path-to-this-directory>`:

```sh
monkey task start "Fix normalizeUserId: collapse whitespace to one hyphen, test tabs, run node --test normalize.test.mjs, and report the diff" --target ollama:<model> --workspace <absolute-path-to-this-directory> --json
```

The task command submits a run and returns its ID. Follow it with `monkey task attach <run-id> --follow`; approval and verification requirements still apply. The CLI task path requires the installed daemon and compatible model. See [Autonomous tasks](../../docs/autonomous-tasks.md) for its constraints.

## Verify independently

Rerun `node --test normalize.test.mjs` yourself. A successful result requires all tests to pass **and** the diff to contain only justified changes. Inspect `git diff -- normalize.mjs normalize.test.mjs` from the repository root (or the files directly if you copied the directory outside Git).

For a clean repeat, restore these example files from Git or work in a fresh copy. The [step-by-step first run](../../docs/first-run.md) covers installation and recording.
