# In-app updates

Little Monkey updates the way Claude Desktop does: silently, in the
background, with a single small card as the only interruption.

## Behaviour

| Step | What happens |
| --- | --- |
| Check | 8s after launch, then every 6h, plus on window refocus if the last check is over an hour old (`src/lib/appUpdater.ts`). Main window only. |
| Stage | macOS/Linux: `downloadAndInstall()` — the new bundle replaces the old one on disk while the app keeps running. Windows: `download()` only. |
| Notify | Only once the update is staged does the card appear at the bottom of the session sidebar: app icon, "Relaunch to update" (Windows: "Install update"), the version, an arrow (`src/components/Update/UpdateCard.tsx`). |
| Apply | macOS/Linux: `relaunch()`, instant — the new version is already on disk. Windows: `install()` runs the NSIS installer, which closes and restarts the app itself. |
| Failure | Silent *in the app chrome*. `updateStore.lastError` records it, the next scheduled check retries, and Settings → Updates & integrity shows the failure and when the last check ran. |

No modal, no "update available?" question, no dismiss button. The Windows
split exists so an update never kills a running turn: the installer needs to
close the app to replace locked files, so that step waits for the click.

## Settings → Updates & integrity

`src/components/Settings/UpdatesPanel.tsx` is the manual surface for all of
this: a **Check now** button (the one trigger that ignores the poll interval),
the current state and last check time, the last failure if there was one, the
rollback controls below, and the startup integrity verdict below that. A Linux
install that is not an AppImage says so instead of silently never updating —
`update_install_info` reports whether this install shape can be replaced in
place at all.

## Rollback

Every update replaces the installed app, so the version you were running stops
existing anywhere on the machine — unless a copy was taken first. One is
(`src-tauri/src/update_rollback.rs`):

| Step | What happens |
| --- | --- |
| Snapshot | Taken immediately before the install replaces the app: `downloadAndInstall()` on macOS/Linux, the card click on Windows. Exactly one is kept, replacing any earlier one. |
| Cost | A full copy of the install (`.app` bundle, install directory, or AppImage). The panel reports its size, and **Discard** deletes it. |
| Restore | Writes a small script, starts it detached, and exits. The script waits for this pid to go, puts the copy back — moving the current install aside first, so a failed copy restores rather than destroys — and relaunches. |
| Failure | A snapshot that cannot be taken never blocks the update: an un-rollback-able update still beats no update. The panel says the snapshot failed. |

A rollback is a *local* restore, not a downgrade request to the endpoint: the
updater serves one release (the latest) and has no way to ask for an older
one, which is precisely why the copy exists.

## Startup integrity check

Before any native runtime is executed, the app verifies itself
(`src-tauri/src/self_integrity.rs`): its own code signature, and every file of
every managed runtime against the trusted manifest digest baked into the
binary. The verdict is computed once per process and read by every path that
resolves a runtime binary — `llama.rs`, the Studio image/video and speech
engines, and `monkey-cli`.

A **mismatch** — a signature that is present and invalid, or a file that
disagrees with an authenticated manifest — refuses to launch any native
runtime for the rest of the process, and says so in the panel. The three
non-failures stay distinct from it: *absent* (nothing installed), *unsupported*
(no build published for this target), and *unverified* (a source build with no
signature, a `LITTLE_MONKEY_*_RUNTIME` developer override, or a tree staged
with no trusted digest). Only the first refuses; treating "cannot verify" as
"tampered" would mean nobody could run this app from source.

The check runs *after* the launch-time materialization of the bundled runtimes,
because materialization is the repair pass — it replaces an invalid installed
tree with the bundle it verified — and checking first would latch a refusal on
a fault that was about to be fixed.

## Turning it on

Signing is set up as of 2026-08-04: the keypair exists, `plugins.updater.pubkey`
holds the public half, and `TAURI_SIGNING_PRIVATE_KEY` is a repository secret.
One thing still gates a working update: the key password secret. Publishing is
automatic — the `publish` job flips the draft once every matrix target has
uploaded.

To (re)do the setup from scratch:

1. Generate a signing keypair (keep the password somewhere safe — it is
   needed by CI, and neither key nor password can be recovered):

   ```bash
   pnpm tauri signer generate -w ~/.tauri/little-monkey.key
   ```

2. Put the **public** key (contents of `~/.tauri/little-monkey.key.pub`) into
   `plugins.updater.pubkey` in `src-tauri/tauri.conf.json`.

3. Add the **private** key and its password as repository secrets:

   ```bash
   gh secret set TAURI_SIGNING_PRIVATE_KEY -R AA-Box/little-monkey < ~/.tauri/little-monkey.key
   gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD -R AA-Box/little-monkey
   ```

   The release workflow already passes both to `tauri-action`, which then
   signs each updater artifact and publishes `latest.json`
   (`includeUpdaterJson: true`). A missing password secret fails the signing
   step, so both are required.

4. Nothing. Publishing is automated. `tauri-action` still creates the release
   as a draft (`releaseDraft: true`), because six matrix jobs write to one
   release and publishing from inside the matrix would expose whichever
   platform finished first along with an incomplete `latest.json`. The
   `publish` job then runs `gh release edit <version> --draft=false --latest`
   once every target has uploaded. GitHub serves no draft assets, so that flip
   is what ships the update to every installed app — and a failed target leaves
   the release a draft rather than shipping a partial one.

Never commit or rotate away the private key carelessly: losing it means no
existing install can accept a future update, and leaking it lets anyone sign
an "update" your users' apps will install.

## Which assets the updater uses

The release already builds everything needed; only the signatures were
missing.

| Platform | Updater artifact | Notes |
| --- | --- | --- |
| macOS | `Little.Monkey_{aarch64,x64}.app.tar.gz` | `.dmg` is install-only |
| Windows | `Little.Monkey_{x64,arm64}-setup.exe` (NSIS) | `.msi` also works if preferred |
| Linux | `Little.Monkey_{amd64,aarch64}.AppImage` | `.deb`/`.rpm` installs can never self-update — the app detects this and says so rather than failing quietly |

Each gets a matching `.sig` file once signing is on, and `latest.json` maps
`{platform}-{arch}` to the artifact URL plus its signature.

## Testing without a release

Point `plugins.updater.endpoints` at a locally served `latest.json` (any
static file server), bump `version` in `src-tauri/tauri.conf.json` on the
served build, and launch the older build. The launch check runs 8 seconds
after boot; the card appears once the download finishes.
