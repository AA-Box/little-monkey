import { useEffect, useRef, useState } from "react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { join, tempDir } from "@tauri-apps/api/path";
import { save } from "@tauri-apps/plugin-dialog";
import { writeTextFile } from "@tauri-apps/plugin-fs";
import { openPath } from "@tauri-apps/plugin-opener";
import { Prism as SyntaxHighlighter } from "react-syntax-highlighter";
import oneDark from "react-syntax-highlighter/dist/esm/styles/prism/one-dark";
import oneLight from "react-syntax-highlighter/dist/esm/styles/prism/one-light";
import { Check, Copy, ExternalLink, RefreshCw, Save, X } from "lucide-react";

import { IconButton, Tabs } from "../ui";
import { useArtifactStore, type ArtifactPaneTab } from "../../store/artifactStore";
import { useSettingsStore } from "../../store/settingsStore";
import {
  artifactVersions,
  containsScriptTag,
  extractArtifacts,
  findArtifact,
  renderMermaidToSvg,
  wrapArtifactDocument,
  type ArtifactBlock,
  type ArtifactRef,
} from "../../lib/artifacts";
import { sessionMessages, useSessionStore } from "../../store/sessionStore";
import { getStoredTheme } from "../../lib/theme";
import { useT } from "../../lib/i18n";

function formatError(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

/** File extension used for `saveAs`'s default filename, one per `ArtifactKind`. */
const KIND_EXTENSION: Record<ArtifactBlock["kind"], string> = {
  html: "html",
  svg: "svg",
  mermaid: "mmd",
};

/** Turns an artifact's title into a filesystem-safe filename stem — mirrors
 * the shape `slugify` in `promptStore.ts` produces for command slugs, kept
 * as its own tiny copy here since the two have slightly different alphabets
 * (this one keeps case for readability in a saved filename) and coupling two
 * unrelated modules over a one-line string transform isn't worth it. */
function slugifyFileName(title: string): string {
  const slug = title
    .trim()
    .replace(/[^a-zA-Z0-9-_ ]+/g, "")
    .trim()
    .replace(/\s+/g, "-");
  return slug.length > 0 ? slug : "artifact";
}

/**
 * Right-panel artifact preview — rendered by `App.tsx`'s workspace `<aside>`
 * in place of `FileTree`/`DiffViewer` whenever `artifactStore.active` is set
 * (see that store's doc comment). Reads NOTHING from `artifactStore` beyond
 * `active`/`tab`: every actual `ArtifactBlock` (title/kind/content) is
 * re-derived from `sessionMessages(active.sessionId)` via
 * `extractArtifacts`/`findArtifact` on every render, so an edit, revert, or
 * compaction that changes the transcript can never leave this pane showing
 * stale content (see the design doc's "transcript-derived state" framing).
 *
 * Two rendering tiers (see `docs/roadmap/p2-artifacts-rendering.md`'s
 * SANDBOX MODEL section):
 *
 * Tier 1 (default, everything else): `<iframe sandbox="" srcdoc={...} />` —
 * the empty `sandbox` attribute plus the app's CSP (`default-src 'self'`, no
 * script-src override) is what makes an inline `<script>` tag in the content
 * inert. svg/mermaid always render this way; html renders this way too
 * whenever it has no `<script>`, `artifactScriptsEnabled` is off, or the
 * tier-2 publish hasn't resolved yet (never a blank frame in the interim).
 *
 * Tier 2 (html containing a `<script>`, only while `artifactScriptsEnabled`):
 * `<iframe sandbox="allow-scripts" src={blob:…} />`. The content is
 * republished to Rust memory via `artifact_publish`, served back by the
 * `artifact://` custom protocol (`src-tauri/src/artifacts.rs`) with a strict
 * per-document CSP (`connect-src 'none'` etc.) that the protocol now also
 * injects as a leading `<meta>`, then fetched by the trusted main frame and
 * re-served to the iframe from a `blob:` object URL. Two independent
 * properties make this safe:
 *
 *  - `sandbox` is `allow-scripts` and NOTHING else — no `allow-same-origin`
 *    — giving the frame an opaque origin: no cookies/storage, no parent-DOM
 *    access, no popups, no top-window navigation. A future change that "just
 *    adds allow-same-origin for convenience" would silently reintroduce
 *    exactly the risk this pane is built to avoid.
 *  - The frame is loaded from a `blob:` URL, NOT straight from `artifact://`.
 *    That is deliberate and load-bearing on Windows: there, `wry`'s WebView2
 *    backend injects Tauri's IPC bridge (invoke key included) into every
 *    subframe regardless of Tauri's `for_main_frame_only` flag, and Tauri's
 *    `is_local_url` treats a custom-protocol frame (`http://artifact.localhost`)
 *    as a trusted *local* origin — so a frame pointed straight at `artifact://`
 *    could invoke privileged commands with this window's full capabilities. A
 *    `blob:` origin is *remote* to Tauri's ACL, which grants it nothing (no
 *    `remote` capability is configured), so the bridge is inert even where the
 *    Windows leak still plants it. `examples/verify_artifact_ipc_isolation.rs`
 *    is the automated per-OS gate for this. A future change that points this
 *    frame back at `artifact://` directly would reopen the Windows escape.
 *
 * Published content is removed (`artifact_remove`) whenever the publish
 * effect's inputs change and on unmount (pane close or the "session switch"
 * effect below), and the blob URL is revoked alongside it — see those
 * effects' own doc comments.
 */
export function ArtifactPane() {
  const { t } = useT();
  const active = useArtifactStore((s) => s.active);
  const tab = useArtifactStore((s) => s.tab);
  const setTab = useArtifactStore((s) => s.setTab);
  const close = useArtifactStore((s) => s.close);
  const artifactScriptsEnabled = useSettingsStore((s) => s.artifactScriptsEnabled);
  const activeSessionId = useSessionStore((s) => s.activeSessionId);
  const splitSessionId = useSessionStore((s) => s.splitSessionId);

  const [selectedRef, setSelectedRef] = useState<ArtifactRef | null>(null);
  const [copyState, setCopyState] = useState<"idle" | "copied">("idle");
  const [saveError, setSaveError] = useState<string | null>(null);
  const [openInBrowserError, setOpenInBrowserError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  // The tier-2 (`artifact://`) publish id for the currently shown block, once
  // `artifact_publish` resolves — `null` while unpublished/publishing/tier-1.
  const [tier2Id, setTier2Id] = useState<string | null>(null);
  // The `blob:` object URL the tier-2 frame is actually loaded from — see the
  // fetch-and-blob effect below and this component's doc comment for why the
  // frame is loaded from a blob rather than straight from `artifact://`.
  const [tier2BlobUrl, setTier2BlobUrl] = useState<string | null>(null);
  const [tier2Error, setTier2Error] = useState<string | null>(null);
  // Mermaid rendering result for the currently shown block, when it's a
  // `mermaid` fence — `null`/`null` while still rendering (see the effect
  // below); exactly one of `mermaidSvg`/`mermaidError` is non-null once
  // `renderMermaidToSvg` settles.
  const [mermaidSvg, setMermaidSvg] = useState<string | null>(null);
  const [mermaidError, setMermaidError] = useState<string | null>(null);

  const blocks = active ? extractArtifacts(sessionMessages(active.sessionId), (n) => t("ArtifactPane.untitledArtifact", { n })) : [];
  const clicked = active ? findArtifact(blocks, active.ref) : null;
  const versions = clicked ? artifactVersions(blocks, clicked) : [];
  // Computed here (rather than after the `!active`/`!block` early returns
  // below) because the tier-2 publish effect further down is a hook and
  // needs it — hooks must run unconditionally on every render.
  //
  // Falls back to `clicked` whenever `selectedRef` no longer resolves (a
  // transcript truncate/revert removed that specific version, e.g. via
  // checkpoint restore) but the artifact the user actually clicked Preview
  // on is still perfectly valid — the version-reset effect below only
  // re-defaults `selectedRef` when the *opened* artifact identity changes,
  // not on every transcript edit, so without this fallback a still-valid
  // `clicked` artifact would incorrectly render as "no longer available"
  // just because some OTHER, later version it wasn't even showing got
  // removed. `selectedRef && findArtifact(...)` deliberately short-circuits
  // to `null` (not `undefined`) on a stale ref so `||` reaches `clicked`.
  const block = (selectedRef && findArtifact(blocks, selectedRef)) || clicked;

  // A fresh `open()` (a new session/ref) defaults the version selector to
  // the NEWEST artifact sharing that title, per the design doc's "v1..vN
  // (newest default)" — not necessarily the exact fence that was clicked,
  // since successive fences under the same title are almost always later
  // revisions of the same document the user wants to see the latest of. The
  // dropdown still lets them step back to any older version, including the
  // one they actually clicked.
  useEffect(() => {
    if (!active || versions.length === 0) {
      setSelectedRef(null);
      return;
    }
    setSelectedRef(versions[versions.length - 1].ref);
    // Only reset when the *opened* artifact identity changes — switching
    // versions via the dropdown below must not be undone by this effect
    // re-running for unrelated reasons.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active?.sessionId, active?.ref.messageIndex, active?.ref.blockIndex]);

  useEffect(() => {
    setCopyState("idle");
    setSaveError(null);
  }, [active?.sessionId, active?.ref.messageIndex, active?.ref.blockIndex, selectedRef?.messageIndex, selectedRef?.blockIndex]);

  // "Session switch" cleanup per the design doc's LIFECYCLE section: this
  // pane is shared by the main chat and the split pane (see
  // `ActiveArtifact.sessionId`'s doc comment in `artifactStore.ts`), so an
  // open artifact stays valid as long as EITHER pane still shows its owning
  // session. Once neither does (that session was switched away from
  // entirely, e.g. the split pane closed or the main pane navigated
  // elsewhere), auto-close so a stale artifact doesn't linger — closing
  // unmounts this component, which is what actually triggers the tier-2
  // publish effect's cleanup (`artifact_remove`) below.
  useEffect(() => {
    if (!active) return;
    if (active.sessionId !== activeSessionId && active.sessionId !== splitSessionId) {
      close();
    }
  }, [active, activeSessionId, splitSessionId, close]);

  // Tier-2 lifecycle: republish on every open/refresh of an HTML artifact
  // (while `artifactScriptsEnabled`), and always remove whatever this effect
  // last published before publishing again or unmounting — see the design
  // doc's LIFECYCLE section ("re-published on every preview open/refresh...
  // `artifact_remove` is called on pane close and session switch"). Kept in
  // a ref (not state) because the cleanup below must see the id from the
  // PREVIOUS run, not whatever `tier2Id` state happens to hold by the time
  // React actually calls it.
  const publishedIdRef = useRef<string | null>(null);
  useEffect(() => {
    if (!block || block.kind !== "html" || !artifactScriptsEnabled || !containsScriptTag(block.content)) {
      setTier2Id(null);
      setTier2Error(null);
      return;
    }

    let cancelled = false;
    setTier2Error(null);
    invoke<string>("artifact_publish", { content: block.content, kind: "html" })
      .then((id) => {
        if (cancelled) {
          // This run was superseded before the publish resolved — the
          // frame it would have opened is never shown, so remove it
          // immediately instead of leaking it until some later cleanup.
          void invoke("artifact_remove", { id }).catch(() => {});
          return;
        }
        publishedIdRef.current = id;
        setTier2Id(id);
      })
      .catch((err) => {
        if (!cancelled) setTier2Error(formatError(err));
      });

    return () => {
      cancelled = true;
      const id = publishedIdRef.current;
      publishedIdRef.current = null;
      if (id) void invoke("artifact_remove", { id }).catch(() => {});
    };
    // `block` is an object rebuilt every render (extractArtifacts runs
    // fresh each time) — depending on its identity would republish on every
    // keystroke-unrelated re-render, so this depends on its actual content
    // instead, plus `refreshKey` (the Refresh button) and
    // `artifactScriptsEnabled` (flipping the setting mid-session).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [block?.ref.messageIndex, block?.ref.blockIndex, block?.content, block?.kind, artifactScriptsEnabled, refreshKey]);

  // Fetch the published document (CSP meta already injected by
  // `artifacts.rs`) and re-serve it to the frame from a `blob:` object URL —
  // NOT by pointing the iframe straight at `artifact://` (see this
  // component's doc comment for the Windows IPC-isolation reason a blob
  // origin is load-bearing). The fetch runs in the trusted main frame, which
  // the app CSP's `connect-src` allows to reach the `artifact` scheme; the
  // resulting blob URL is revoked when this effect re-runs or unmounts so it
  // never outlives the frame that used it.
  useEffect(() => {
    if (!tier2Id) {
      setTier2BlobUrl(null);
      return;
    }
    let cancelled = false;
    let objectUrl: string | null = null;
    // `?v=` cache-busts the protocol response so a Refresh with unchanged
    // content still re-fetches rather than reading a webview-cached body.
    fetch(`${convertFileSrc(tier2Id, "artifact")}?v=${refreshKey}`)
      .then((res) => res.text())
      .then((html) => {
        if (cancelled) return;
        objectUrl = URL.createObjectURL(new Blob([html], { type: "text/html" }));
        setTier2BlobUrl(objectUrl);
      })
      .catch((err) => {
        if (!cancelled) setTier2Error(formatError(err));
      });
    return () => {
      cancelled = true;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
      setTier2BlobUrl(null);
    };
  }, [tier2Id, refreshKey]);

  // Mermaid rendering: lazily renders a `mermaid`-kind block's raw diagram
  // text to an SVG string via `renderMermaidToSvg` (see `artifacts.ts`'s doc
  // comment — bundled mermaid ^11.16, `startOnLoad: false`,
  // `securityLevel: 'strict'`), then that SVG is displayed through the SAME
  // tier-1 sandboxed `srcdoc` iframe used for `svg` fences below — Mermaid
  // output is always static and never takes the tier-2 `artifact://` path.
  // A malformed diagram rejects rather than throwing (see that function's
  // doc comment), so this never crashes the pane: `mermaidError` is shown
  // instead, alongside the raw code, per the design doc's error-boundary
  // requirement.
  useEffect(() => {
    if (!block || block.kind !== "mermaid") {
      setMermaidSvg(null);
      setMermaidError(null);
      return;
    }

    let cancelled = false;
    setMermaidSvg(null);
    setMermaidError(null);
    renderMermaidToSvg(block.content)
      .then((svg) => {
        if (!cancelled) setMermaidSvg(svg);
      })
      .catch((err) => {
        if (!cancelled) setMermaidError(formatError(err));
      });

    return () => {
      cancelled = true;
    };
    // Same "depend on content, not object identity" rationale as the tier-2
    // publish effect above.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [block?.ref.messageIndex, block?.ref.blockIndex, block?.content, block?.kind, refreshKey]);

  if (!active) return null;

  if (!block) {
    return (
      <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-3 p-6 text-center">
        <p className="text-sm text-faint">{t("ArtifactPane.noLongerAvailable")}</p>
        <IconButton size="sm" onClick={close} aria-label={t("ArtifactPane.close")}>
          <X size={16} />
        </IconButton>
      </div>
    );
  }

  const versionIndex = versions.findIndex(
    (v) => v.ref.messageIndex === block.ref.messageIndex && v.ref.blockIndex === block.ref.blockIndex
  );

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(block.content);
      setCopyState("copied");
      setTimeout(() => setCopyState("idle"), 1500);
    } catch {
      // Clipboard permission denied/unavailable — nothing more to do than
      // silently leave the button at "idle"; there's no destructive fallback.
    }
  };

  const handleSaveAs = async () => {
    setSaveError(null);
    try {
      const extension = KIND_EXTENSION[block.kind];
      const path = await save({
        defaultPath: `${slugifyFileName(block.title)}.${extension}`,
        filters: [{ name: extension.toUpperCase(), extensions: [extension] }],
      });
      if (!path) return;
      // Human-initiated (the user picked this exact path in a native save
      // dialog) — not permission-gated, same precedent as the app's
      // `git_commit` / other explicitly-user-triggered writes.
      await writeTextFile(path, block.content);
    } catch (err) {
      setSaveError(formatError(err));
    }
  };

  // The complete standalone HTML document "Open in browser" writes to a temp
  // file — the exact same bytes the tier-1 `srcdoc` iframe below shows for
  // html/svg, so what opens in the real browser matches what's already
  // previewed. `null` for a `mermaid` block whose render hasn't resolved yet
  // (still rendering, or errored) — there's no diagram SVG to open in that
  // case, so the button is disabled rather than opening something stale or
  // showing raw diagram text as if it were HTML.
  const openInBrowserDocument: string | null =
    block.kind === "mermaid" ? (mermaidSvg ? wrapArtifactDocument("svg", mermaidSvg) : null) : wrapArtifactDocument(block.kind, block.content);

  const handleOpenInBrowser = async () => {
    setOpenInBrowserError(null);
    if (openInBrowserDocument === null) return;
    try {
      const fileName = `little-monkey-artifact-${crypto.randomUUID()}.html`;
      const path = await join(await tempDir(), fileName);
      await writeTextFile(path, openInBrowserDocument);
      // Human-initiated (the Open in browser click itself) — not
      // permission-gated, same "explicitly user-triggered" precedent as
      // `handleSaveAs` above. Opens with the OS's default handler for
      // `.html`, which is the system's default browser on every desktop
      // platform this app targets.
      await openPath(path);
    } catch (err) {
      setOpenInBrowserError(formatError(err));
    }
  };

  const theme = getStoredTheme();
  const hasScript = containsScriptTag(block.content);
  // Whether THIS render should show the tier-2 (`sandbox="allow-scripts"`,
  // blob-loaded) iframe rather than the tier-1 (empty-sandbox `srcdoc`) one —
  // mirrors the publish effect's own eligibility check above, plus requiring
  // the `blob:` URL to have actually resolved (an in-flight publish/fetch
  // renders tier-1 in the meantime, never a blank frame).
  const useTier2 =
    block.kind === "html" && artifactScriptsEnabled && hasScript && tier2BlobUrl !== null;
  const tabs = [
    { id: "preview", label: t("ArtifactPane.previewTab") },
    { id: "code", label: t("ArtifactPane.codeTab") },
  ];

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex shrink-0 flex-col gap-2 border-b border-border px-3 py-2">
        <div className="flex items-center justify-between gap-2">
          <span className="min-w-0 truncate text-sm font-medium text-foreground" title={block.title}>
            {block.title}
          </span>
          <div className="flex shrink-0 items-center gap-1">
            <IconButton size="sm" onClick={() => setRefreshKey((k) => k + 1)} aria-label={t("ArtifactPane.refresh")}>
              <RefreshCw size={14} />
            </IconButton>
            <IconButton size="sm" onClick={() => void handleCopy()} aria-label={t("ArtifactPane.copyCode")}>
              {copyState === "copied" ? <Check size={14} /> : <Copy size={14} />}
            </IconButton>
            <IconButton size="sm" onClick={() => void handleSaveAs()} aria-label={t("ArtifactPane.saveAs")}>
              <Save size={14} />
            </IconButton>
            <IconButton
              size="sm"
              onClick={() => void handleOpenInBrowser()}
              disabled={openInBrowserDocument === null}
              aria-label={t("ArtifactPane.openInBrowser")}
            >
              <ExternalLink size={14} />
            </IconButton>
            <IconButton size="sm" onClick={close} aria-label={t("ArtifactPane.close")}>
              <X size={14} />
            </IconButton>
          </div>
        </div>

        <div className="flex items-center justify-between gap-2">
          <Tabs tabs={tabs} active={tab} onChange={(id) => setTab(id as ArtifactPaneTab)} />
          {versions.length > 1 && (
            <select
              value={versionIndex >= 0 ? versionIndex : versions.length - 1}
              onChange={(event) => setSelectedRef(versions[Number(event.target.value)].ref)}
              aria-label={t("ArtifactPane.versionSelectorAriaLabel")}
              className="shrink-0 cursor-pointer rounded-md border border-border bg-surface-2 px-2 py-1 text-xs text-foreground"
            >
              {versions.map((version, index) => (
                <option key={`${version.ref.messageIndex}-${version.ref.blockIndex}`} value={index}>
                  {t("ArtifactPane.versionLabel", { n: index + 1 })}
                </option>
              ))}
            </select>
          )}
        </div>

        {saveError && <p className="text-xs text-danger">{t("ArtifactPane.saveError", { error: saveError })}</p>}
        {openInBrowserError && <p className="text-xs text-danger">{t("ArtifactPane.openInBrowserError", { error: openInBrowserError })}</p>}
      </div>

      <div className="min-h-0 flex-1 overflow-auto">
        {tab === "code" ? (
          <SyntaxHighlighter
            key={refreshKey}
            language={block.kind === "mermaid" ? "text" : block.kind}
            style={theme === "dark" ? oneDark : oneLight}
            customStyle={{ margin: 0, minHeight: "100%", fontSize: "12px" }}
            showLineNumbers
          >
            {block.content}
          </SyntaxHighlighter>
        ) : block.kind === "mermaid" ? (
          mermaidError ? (
            // Error boundary per the design doc: a malformed diagram must
            // never crash the pane — show the raw code plus a visible error
            // instead, exactly the same "code" view the Code tab offers.
            <div className="flex h-full flex-col overflow-auto">
              <p className="shrink-0 border-b border-border bg-surface-2 px-3 py-1.5 text-xs text-danger">
                {t("ArtifactPane.mermaidRenderError", { error: mermaidError })}
              </p>
              <SyntaxHighlighter
                language="text"
                style={theme === "dark" ? oneDark : oneLight}
                customStyle={{ margin: 0, minHeight: "100%", fontSize: "12px" }}
                showLineNumbers
              >
                {block.content}
              </SyntaxHighlighter>
            </div>
          ) : mermaidSvg ? (
            // Tier 1 ONLY: Mermaid output is always static, so — like `svg`
            // fences below — it's shown through the empty-sandbox `srcdoc`
            // iframe, never the tier-2 `artifact://` protocol.
            <iframe
              key={`mermaid-${refreshKey}`}
              title={block.title}
              sandbox=""
              srcDoc={wrapArtifactDocument("svg", mermaidSvg)}
              className="h-full w-full border-0 bg-white"
            />
          ) : (
            <div className="flex h-full items-center justify-center p-6 text-center">
              <p className="text-sm text-faint">{t("ArtifactPane.mermaidRendering")}</p>
            </div>
          )
        ) : block.content.trim().length === 0 ? (
          <div className="flex h-full items-center justify-center p-6 text-center">
            <p className="text-sm text-faint">{t("ArtifactPane.renderError")}</p>
          </div>
        ) : (
          <div className="flex h-full flex-col">
            {hasScript && !useTier2 && (
              <p className="shrink-0 border-b border-border bg-surface-2 px-3 py-1.5 text-xs text-faint">
                {tier2Error
                  ? t("ArtifactPane.interactivePreviewUnavailable", { error: tier2Error })
                  : t("ArtifactPane.scriptsDisabledNotice")}
              </p>
            )}
            {useTier2 && tier2BlobUrl ? (
              // Tier 2: the document published to the `artifact://` protocol
              // (`src-tauri/src/artifacts.rs`) is fetched and re-served to
              // this frame from a `blob:` URL (see the fetch-and-blob effect
              // and this component's doc comment). `sandbox` deliberately has
              // `allow-scripts` and NOTHING else (in particular no
              // `allow-same-origin`), giving the frame an opaque origin with
              // no cookies/storage/parent-DOM-access/popups/top-navigation;
              // the blob URL additionally makes it a *remote* origin so
              // Tauri's ACL denies any IPC the Windows WebView2 subframe leak
              // might otherwise expose.
              <iframe
                key={`tier2-${tier2BlobUrl}`}
                title={block.title}
                sandbox="allow-scripts"
                src={tier2BlobUrl}
                className="min-h-0 flex-1 border-0 bg-white"
              />
            ) : (
              <iframe
                key={`tier1-${refreshKey}`}
                title={block.title}
                sandbox=""
                srcDoc={wrapArtifactDocument(block.kind, block.content)}
                className="min-h-0 flex-1 border-0 bg-white"
              />
            )}
          </div>
        )}
      </div>
    </div>
  );
}

export default ArtifactPane;
