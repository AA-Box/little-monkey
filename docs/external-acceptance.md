# External acceptance for native runtimes and signed updates

Run these checks on disposable test installations. Save command output, OS and
architecture, app version, runtime revision, and a link to the tested commit or
release. Automated contract tests prove the harness behavior; only the physical
runs below can establish production acceptance.

## Local gates before a physical run

```sh
pnpm test:updater-security
pnpm test:updater-acceptance
pnpm test:managed-runtime
```

The native authentication and paired process comparison procedures live in
`docs/native-subscription-auth.md` and
`docs/paired-jcode-headless-acceptance.md` on their respective PR branches.
Their local contract commands are `pnpm test:native-auth:contract` and
`pnpm test:paired-headless:contract`. The paired runner needs an actual Linux
host, both running agents, matching model and backend identifiers, and
authorized services before its report may set `verifiedLiveRuns: true`.

## Apple Silicon and macOS runtime

On a physical Apple Silicon Mac, check out the runtime revision under review:

```sh
pnpm stage:runtime:sd
pnpm stage:cli:placeholder
pnpm test:managed-runtime
cargo check --locked --manifest-path src-tauri/Cargo.toml
```

Inspect the staged `sd-server --help` version and the staging digest output.
Success requires the declared release revision and asset SHA-256 to match the
pin; a command exit code of zero alone does not prove image inference. Repeat
the runtime stage and Rust check on an Intel Mac when Intel support is being
accepted. CI's platform matrix supplies compilation and smoke coverage for
other architectures; record the exact job result instead of claiming a local
machine ran it.

## Windows runtime

On Windows ARM64 and x64 separately, with Node, pnpm, Rust MSVC toolchain, and
the Visual Studio C++ build tools installed, run in PowerShell:

```powershell
pnpm stage:runtime:sd
pnpm stage:cli:placeholder
pnpm test:managed-runtime
cargo check --locked --manifest-path src-tauri/Cargo.toml
```

Inspect the staged binary's `--help` output for the pinned revision. A
failure to download, build, verify the digest, start the binary, or compile the
app is a failed target acceptance. Do not substitute an x64 result for ARM64.

## Physical GPU image inference

Provide a supported physical GPU and a locally licensed, compatible SD
checkpoint. Set the staged binary and checkpoint paths, then run the ignored
Rust integration test on that same host:

```sh
export SD_SERVER=/absolute/path/to/sd-server
export SD_CHECKPOINT=/absolute/path/to/model.safetensors
pnpm stage:cli:placeholder
cargo test --locked --manifest-path src-tauri/Cargo.toml --lib \
  generation::tests::a_real_engine_reports_its_model_and_its_step_count \
  -- --ignored --exact --nocapture
```

On PowerShell, set `$env:SD_SERVER` and `$env:SD_CHECKPOINT` to absolute paths
and run the same `cargo test` command. A pass requires the requested model to
load, one 512×512 PNG from a 25-step request, and progress with a valid step
count observed mid-job. The harness stops the engine and fails after 15 minutes
if it cannot finish. A CPU-only run, unit test, or `--help` smoke test is not
GPU inference acceptance.

## Signed updater staging

For a release newer than the currently installed app, first inspect the
published manifest and updater artifact. This command works on any host with
Node; add `--artifact` after downloading the selected updater file and
installing `minisign` to verify the bytes against the app's configured public
key:

```sh
node scripts/updater-acceptance-preflight.mjs \
  --platform darwin-aarch64 --installed-version 1.9.0 \
  --artifact /absolute/path/to/Little.Monkey_aarch64.app.tar.gz
```

Other platform values are `darwin-x86_64`, `windows-aarch64`,
`windows-x86_64`, `linux-aarch64`, and `linux-x86_64`. For offline fixture
testing, add `--manifest-file /absolute/path/to/latest.json`. Exit code zero
with `manifestValidated: true` validates the selected version, release URL,
format, and signature metadata. `signatureVerified: true` requires the
downloaded artifact and an actual successful minisign verification.
`installationVerified` remains false because the preflight never installs.

To accept the updater change itself, build and sign a **new release containing
the dependency fix**, install the preceding signed release on a disposable
machine for each supported OS, and point its configured endpoint at the new
release. Open Settings → Updates & integrity and choose **Check now**. Wait
for the update card, apply it, relaunch, and confirm the exact new version plus
a healthy startup integrity verdict. On Windows, accept the installer after
the card appears; on Linux, use an AppImage installation. Record failures from
the panel and verify that an altered artifact is refused. An already published
release can prove the public key and signing pipeline, but it cannot prove the
new Rust dependency selection at installation time.

## Evidence and remaining checks

The operator report should contain the command, exit code, full structured
output, tested commit or release, OS/architecture, and whether the run used a
fixture, real service, or physical GPU. A fixture pass is never a live service
pass. Report each unrun target explicitly; do not infer results from another
platform.
