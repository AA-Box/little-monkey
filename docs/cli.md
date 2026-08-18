# CLI

The terminal client. For the composer's slash commands see
[Features](features.md#desktop-slash-commands).

The installed command is `monkey`. The preferred chat form is model first:

```sh
monkey llama3.2 "Summarize this project"       # auto-resolve target/provider
monkey llama3.2                                # interactive REPL
monkey --provider openai gpt-4.1-mini "Review this codebase"
monkey --local-url http://127.0.0.1:8090 local-model "Inspect the workspace"
```

For the app-owned path that needs neither Ollama nor a separate `llama.cpp`, pull or run a public Ollama Registry tag or a public Hugging Face single-file GGUF reference:

```sh
monkey pull llama3.2:3b
monkey run llama3.2:3b "Summarize this project"
monkey run hf.co/Qwen/Qwen2.5-Coder-0.5B-Instruct-GGUF:Q4_K_M
```

`monkey run` resolves immutable metadata, verifies the expected model SHA-256, inspects the checksum-bound GGUF's embedded chat template before advertising tool support, resumes interrupted downloads, reuses verified installs offline, and starts the bundled loopback-only runtime for that session. Ollama's separate Go-template layer is never passed to `llama.cpp`, and the runtime's per-file manifest is authenticated by a digest embedded in the compiled app. Private or gated Hugging Face repositories, non-GGUF or sharded artifacts, and Ollama models requiring separate adapters or projectors are rejected with a clear error.

If a non-local model is exposed by more than one configured provider, `monkey` asks for `--provider <id>` rather than guessing. The legacy `--ollama` and `--model` forms remain aliases.

Useful chat flags:

- `--workspace <path>` — sandbox tool access to a workspace; defaults to the current directory.
- `--permission-mode manual|acceptEdits|smart|plan|auto|bypass` — terminal permission policy.
- `--provider <id>` — override or disambiguate a configured provider.
- `--local-url <url>` — explicit local OpenAI-compatible endpoint.
- `--persona <slash-command>`, repeatable `--stack <name>` — attach saved context.
- `--verify` / `--no-verify`, `--subagents`, `--no-rules`, `--no-mcp` — opt into verification or subagents, or suppress configured context.
- `--temperature`, `--top-p`, `--seed`, `--stop`, `--num-predict`, `--system`, `--format`, `--verbose`, `--attach-images` — generation controls.
- `--num-ctx` — managed-runtime or Ollama context size; `--keepalive`, `--think`, `--hidethinking` remain Ollama-native.

Ollama-daemon compatibility commands still require a user-installed Ollama runtime:

```sh
monkey list
monkey ps
monkey show <model>
monkey rm <model> [model...]
monkey cp <source> <destination>
monkey stop <model>
monkey push <model>
monkey create <model> --file Modelfile
monkey signin
monkey signout
monkey serve
```

Shared desktop and headless commands:

```sh
monkey acp
monkey revert [checkpoint-id]
monkey api-serve [--port <port>]
monkey processes [--kind <kind>] [--all] [--json]
monkey processes show <id>
monkey processes signal <id> stop|suspend|resume|kill
monkey processes signals [--json]
monkey processes limits [--json]

monkey profiles list [--json]
monkey profiles create <name>
monkey profiles switch <id>
monkey profiles rename <id> <name>
monkey profiles limits <id> [--weight <n>] [--max-concurrent-runs <n>] [--max-memory-bytes <n>] [--max-runtime-ms <n>] [--clear]
monkey profiles delete <id> --yes
monkey profiles current [--json]

monkey stacks list | reindex <name>
monkey stacks embed-server start --model-path <embedding.gguf> | status | stop

monkey task list | validate <recipe-file>
monkey task run <name-or-path> [--param key=value ...] [--json]
monkey task schedule <name-or-path> --cron "<expr>"

monkey workflow list | validate <definition.json>
monkey workflow run <workflow-id> [--inputs '{}'] [--secrets '{}']
monkey workflow history [run-id]
monkey workflow replay <workflow-id> <source-run-id> <boundary-node-id> --approval

monkey skills list [--json]
monkey skills preview-local <folder> [--scope global|workspace]
monkey skills install-local <folder> --approval-digest <sha256> --yes
monkey skills preview-git <repository-url> <40-char-commit> [--subdirectory <path>]
monkey skills install-git <repository-url> <40-char-commit> --approval-digest <sha256> --yes
monkey skills enable|disable|rollback|uninstall <command>
monkey skills learned list [--json]
monkey skills learned candidates [--json]
monkey skills learned inspect <candidate-id>
monkey skills learned evaluate <candidate-id> [--report <case-reports.json>]
monkey skills learned promote <candidate-id> --yes
monkey skills learned reject <candidate-id> [--reason <text>]
monkey skills learned deprecate <command> [--scope global|workspace]
monkey skills learned mode [off|suggest-only|auto-stage|auto-promote-safe]

# `learned evaluate --report` records a preflight result: it describes what some
# runtime did, and can never back an unattended promotion. Only the app's own
# isolated executor, which really runs the arms in disposable workspace copies,
# produces a promotion-grade pass.

monkey plugins list [--json]
monkey plugins health [--json]
monkey security audit [--deep] [--fix] [--json]
monkey security verify-run-chain <run-id> [--json]
monkey security permission-trail <tool-call-id> [--json]
monkey security permission-gaps <run-id> [--json]
monkey security subsystem-events [--subsystem <name>] [--limit <n>] [--json]
monkey security egress-evidence [--limit <n>] [--json]
monkey security admission-trail [--limit <n>] [--json]

monkey revisions [--change <change-id>] [--limit <n>]

monkey daemon install | status [--json]
monkey daemon ensure [--json]
monkey daemon run <recipe> [--owned-worktree] [--json]
monkey daemon attach <run-id> [--follow] [--json]
monkey daemon pause|resume|cancel <run-id>
monkey daemon retry <run-id> [--acknowledge-side-effects]
monkey daemon kill-switch engage|release|status
monkey daemon trigger --help
monkey daemon remote --help
monkey daemon remote pair-create --output <file> --run <run-id> --device <capability> [--qr] [--json]
monkey daemon remote device-list [--json]
monkey daemon remote device-grant <device-id> [--capability <capability>]...
monkey daemon remote device-action <action> [--device-id <id>] [--wait-ms <n>] [--json]
monkey daemon remote device-commands <device-id> [--limit <n>] [--json]
monkey daemon remote device-cancel <command-id>
monkey daemon remote voice-start [--device-id <id>] [--duration-ms <n>]
monkey daemon remote voice-list [--device-id <id>] [--limit <n>] [--json]
monkey daemon remote voice-stop <session-id>
monkey daemon remote voice-save <session-id> --output <file>
monkey daemon remote push-configure --web-push [--vapid-subject <url>] [--include-detail]
monkey daemon remote push-configure --project-id <id> --service-account <file> [--include-detail]
monkey daemon remote push-status [--json] | push-disable | push-test <device-id>
```

The `device-*`, `voice-*` and `push-*` commands are documented in
[Paired devices](paired-devices.md).

In the REPL, `/help` lists terminal-only controls such as `/set`, `/show`, `/save`, `/load`, `/revert`, `/persona`, `/prompts`, `/verify`, `/clear`, and `/bye`. Installed skill invocations use the same frozen, turn-scoped prompt composition as desktop chat.

The desktop bundle stages `monkey-cli` as a Tauri sidecar and performs a best-effort, non-elevated install of the `monkey` command on first launch — `/usr/local/bin/monkey` when writable, otherwise `~/.local/bin/monkey`; on Windows `%LOCALAPPDATA%\Programs\monkey-cli\monkey.exe`, with that directory added to the user `PATH`. Shell startup files are not edited. The Rust target remains named `monkey-cli`.
