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
| Failure | Silent. `updateStore.lastError` records it; the user sees nothing and the next scheduled check retries. |

No modal, no "update available?" question, no dismiss button. The Windows
split exists so an update never kills a running turn: the installer needs to
close the app to replace locked files, so that step waits for the click.

## Turning it on

Signing is set up as of 2026-08-04: the keypair exists, `plugins.updater.pubkey`
holds the public half, and `TAURI_SIGNING_PRIVATE_KEY` is a repository secret.
Two things still gate a working update — the key password secret, and
publishing the draft release.

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

4. **Publish the release.** The workflow creates it as a draft
   (`releaseDraft: true`), and GitHub does not serve draft assets — the
   endpoint keeps 404ing until the draft is published. Publishing is the step
   that ships an update to every installed app.

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
| Linux | `Little.Monkey_{amd64,aarch64}.AppImage` | `.deb`/`.rpm` installs can never self-update |

Each gets a matching `.sig` file once signing is on, and `latest.json` maps
`{platform}-{arch}` to the artifact URL plus its signature.

## Testing without a release

Point `plugins.updater.endpoints` at a locally served `latest.json` (any
static file server), bump `version` in `src-tauri/tauri.conf.json` on the
served build, and launch the older build. The launch check runs 8 seconds
after boot; the card appears once the download finishes.
