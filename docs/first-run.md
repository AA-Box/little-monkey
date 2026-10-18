# First run: prove a coding agent can fix a bug

This is a **real, reproducible experiment**, not a pre-recorded success claim. The example has one deliberately failing test and needs no npm packages.

## Prerequisites

1. Install a signed asset for your platform from the [latest stable release](https://github.com/AA-Box/little-monkey/releases/latest). Read its release notes; this document belongs to `develop` and may include newer behavior.
2. Set up a model capable of calling tools, either through **Settings → Local Models**, **Settings → Ollama**, or **Settings → AI Providers**. A small general chat model may not be a reliable coding agent.
3. Install Node.js 20+ **only to run the example's tests**, and Git if you want to clone the source.

## 1. Take a disposable workspace

```sh
git clone --depth 1 --branch develop https://github.com/AA-Box/little-monkey.git
cd little-monkey/examples/first-run
node --test normalize.test.mjs
```

The initial run is expected to **fail one of three tests**. If it doesn't, verify you haven't already edited the example.

## 2. Have the app work on it

Open Little Monkey and attach the `examples/first-run` folder as the workspace. Select your configured tool-capable model. Paste this objective:

> Fix normalizeUserId so repeated spaces, tabs and other whitespace between words become one hyphen. Preserve non-string rejection. Add test coverage for tabs, run `node --test normalize.test.mjs`, and report the diff and exact test outcome.

Authorize only the file edits and commands you understand and intend. The app's tool execution depends on your permission policy and configured model. Deny unexpected network or outside-workspace access.

Alternatively, if the installed release supports daemon-backed autonomous tasks, use the [CLI example](../examples/first-run/README.md#give-the-agent-the-task). Don't confuse submitting a background task with proof of completion.

## 3. Check the result without trusting the model's summary

- Run `node --test normalize.test.mjs` **outside the agent** after the run. All tests must pass.
- Inspect `git diff -- examples/first-run/normalize.mjs examples/first-run/normalize.test.mjs` from the repository root. Ensure the change is limited to the behavior requested.
- Check the app's recorded tool steps, permission decisions and test evidence. If the agent could not run tests, that is an incomplete result, not a pass.
- Report the model, runtime, OS, app release, actual outcome and any setup obstacle in the [first-run feedback form](https://github.com/AA-Box/little-monkey/issues/new/choose). Avoid pasting API keys, tokens, private paths or personal code.

## 4. What this proves — and what it does not

A passing attempt demonstrates one bounded coding task on one configured machine and model. It does not prove autonomous recovery, universal security, superior benchmark quality, or any success rate across other tasks. The [limitations](limitations.md) and [live acceptance instructions](autonomous-tasks.md) distinguish these claims.

For a genuine screenshot or recording, see the [demo publishing protocol](adoption.md#publish-a-real-demo-not-a-mockup).
