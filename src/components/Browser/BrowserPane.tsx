/**
 * Claude-Desktop-style in-app browser pane: a right-docked, resizable pane
 * with a tab strip, address bar, and real web content. Pages render in
 * native child webviews (`src-tauri/src/browser_pane.rs`) overlaid on this
 * component's content area — the frontend continuously reports that area's
 * rect so the native layer stays glued to it.
 *
 * Because native webviews always paint above the app's DOM, the pane must be
 * told when something should cover it (modals, command palette): pass
 * `obscured` and the active webview hides until the overlay goes away.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import { ArrowLeft, ArrowRight, ExternalLink, Globe2, Loader2, Plus, RotateCw, X } from "lucide-react";

import { useT } from "../../lib/i18n";
import {
  MIN_PANE_WIDTH,
  useBrowserPaneStore,
  type BrowserPaneTab,
  type BrowserTabEvent,
} from "../../store/browserPaneStore";
import { IconButton } from "../ui";

interface BrowserPaneProps {
  /** True while a fullscreen overlay (settings, palette, …) covers the pane,
   * or — embedded — while the hosting tab is not the active one. */
  obscured?: boolean;
  /** Space (px) the tab strip leaves free at its right end for the app's
   * fixed dock-toggle cluster, which floats above this pane's top-right
   * corner. Applied as a margin on the strip's row so the new-tab "+" and
   * the pane's close X can never sit underneath those transparent icons. */
  trailingInset?: number;
  /** Rendered as a right-sidebar TAB rather than as its own dock column: the
   * host owns width and closing, so the pane drops its own width style and
   * left-edge resize handle. */
  embedded?: boolean;
  /** Close handler for the pane's X. Defaults to closing the dock column. */
  onClose?: () => void;
}

function tabDisplayName(tab: BrowserPaneTab, fallback: string): string {
  if (tab.title) return tab.title;
  if (tab.url) {
    try {
      return new URL(tab.url).host || tab.url;
    } catch {
      return tab.url;
    }
  }
  return fallback;
}

function TabButton({
  tab,
  active,
  newTabLabel,
  closeLabel,
  onSelect,
  onClose,
}: {
  tab: BrowserPaneTab;
  active: boolean;
  newTabLabel: string;
  closeLabel: string;
  onSelect: () => void;
  onClose: () => void;
}) {
  return (
    <div
      role="tab"
      aria-selected={active}
      tabIndex={0}
      onClick={onSelect}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") onSelect();
      }}
      onAuxClick={(event) => {
        // Middle-click closes, like every desktop browser.
        if (event.button === 1) onClose();
      }}
      title={tab.url || newTabLabel}
      className={`group flex h-8 w-40 shrink-0 cursor-pointer items-center gap-1.5 rounded-lg px-2.5 text-xs transition-colors ${
        active
          ? "bg-surface-2 text-foreground"
          : "text-muted hover:bg-surface-2/60 hover:text-foreground"
      }`}
    >
      {tab.loading ? (
        <Loader2 size={13} className="shrink-0 animate-spin text-faint" />
      ) : tab.favicon ? (
        <img src={tab.favicon} alt="" className="h-3.5 w-3.5 shrink-0 rounded-[3px]" />
      ) : (
        <Globe2 size={13} className="shrink-0 text-faint" />
      )}
      <span className="min-w-0 flex-1 truncate text-left">{tabDisplayName(tab, newTabLabel)}</span>
      <span
        role="button"
        aria-label={closeLabel}
        onClick={(event) => {
          event.stopPropagation();
          onClose();
        }}
        className={`shrink-0 rounded p-0.5 text-faint hover:bg-border hover:text-foreground ${
          active ? "" : "opacity-0 group-hover:opacity-100 focus-visible:opacity-100"
        }`}
      >
        <X size={12} />
      </span>
    </div>
  );
}

export function BrowserPane({
  obscured = false,
  trailingInset = 0,
  embedded = false,
  onClose,
}: BrowserPaneProps) {
  const { t } = useT();
  const tabs = useBrowserPaneStore((state) => state.tabs);
  const activeId = useBrowserPaneStore((state) => state.activeId);
  const width = useBrowserPaneStore((state) => state.width);
  const error = useBrowserPaneStore((state) => state.error);
  const setOpen = useBrowserPaneStore((state) => state.setOpen);
  const setWidth = useBrowserPaneStore((state) => state.setWidth);
  const setError = useBrowserPaneStore((state) => state.setError);
  const openTab = useBrowserPaneStore((state) => state.openTab);
  const closeTab = useBrowserPaneStore((state) => state.closeTab);
  const selectTab = useBrowserPaneStore((state) => state.selectTab);
  const navigate = useBrowserPaneStore((state) => state.navigate);
  const goBack = useBrowserPaneStore((state) => state.goBack);
  const goForward = useBrowserPaneStore((state) => state.goForward);
  const reload = useBrowserPaneStore((state) => state.reload);

  const active = tabs.find((tab) => tab.id === activeId) ?? null;
  const activeIsNative = active !== null && !active.id.startsWith("local-");

  const contentRef = useRef<HTMLDivElement | null>(null);
  const addressRef = useRef<HTMLInputElement | null>(null);
  const [draft, setDraft] = useState("");
  const [editing, setEditing] = useState(false);

  // Address bar mirrors the active tab's URL except while the user types.
  useEffect(() => {
    if (!editing) setDraft(active?.url ?? "");
  }, [active?.url, active?.id, editing]);

  // A brand-new local tab wants the address bar focused, Claude-Desktop-style.
  useEffect(() => {
    if (active !== null && !activeIsNative) addressRef.current?.focus();
  }, [active, activeIsNative]);

  // First open with no tabs: start on a New Tab.
  useEffect(() => {
    if (useBrowserPaneStore.getState().tabs.length === 0) void openTab();
  }, [openTab]);

  // Rust-side page metadata + denied-popup events.
  useEffect(() => {
    if (!isTauri()) return;
    const unlistenTab = listen<BrowserTabEvent>("browser-pane://tab", (event) => {
      useBrowserPaneStore.getState().applyTabEvent(event.payload);
    });
    const unlistenPopup = listen<{ url: string }>("browser-pane://new-window", (event) => {
      void useBrowserPaneStore.getState().openTab(event.payload.url);
    });
    return () => {
      void unlistenTab.then((fn) => fn());
      void unlistenPopup.then((fn) => fn());
    };
  }, []);

  // Keep the native webview glued to the content area.
  useEffect(() => {
    if (!isTauri()) return;
    const el = contentRef.current;
    if (!el) return;
    const report = () => {
      const rect = el.getBoundingClientRect();
      void invoke("browser_pane_set_bounds", {
        bounds: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
      }).catch(() => undefined);
    };
    report();
    const observer = new ResizeObserver(report);
    observer.observe(el);
    window.addEventListener("resize", report);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", report);
    };
  }, [width]);

  // Show the active native webview only while the pane is mounted and not
  // covered by an overlay; hide everything when the pane unmounts.
  useEffect(() => {
    if (!isTauri()) return;
    void invoke("browser_pane_set_visible", { visible: !obscured && activeIsNative }).catch(
      () => undefined,
    );
    return () => {
      void invoke("browser_pane_set_visible", { visible: false }).catch(() => undefined);
    };
  }, [obscured, activeIsNative]);

  const submitAddress = useCallback(() => {
    if (active === null) return;
    setEditing(false);
    addressRef.current?.blur();
    void navigate(active.id, draft);
  }, [active, draft, navigate]);

  // Drag-to-resize on the left edge, same pattern as the terminal panel.
  const onResizeStart = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      event.preventDefault();
      const startX = event.clientX;
      const startWidth = width;
      const target = event.currentTarget;
      target.setPointerCapture(event.pointerId);
      const move = (moveEvent: PointerEvent) => {
        setWidth(startWidth + (startX - moveEvent.clientX));
      };
      const up = () => {
        target.removeEventListener("pointermove", move);
        target.removeEventListener("pointerup", up);
        target.removeEventListener("pointercancel", up);
      };
      target.addEventListener("pointermove", move);
      target.addEventListener("pointerup", up);
      target.addEventListener("pointercancel", up);
    },
    [width, setWidth],
  );

  return (
    <aside
      className={
        embedded
          ? "relative flex h-full min-w-0 flex-1 flex-col overflow-hidden bg-surface"
          : "relative flex h-full shrink-0 flex-col overflow-hidden border-l border-border bg-surface"
      }
      style={embedded ? undefined : { width, minWidth: MIN_PANE_WIDTH }}
      aria-label={t("BrowserPane.title")}
    >
      {!embedded && (
        <div
          role="separator"
          aria-orientation="vertical"
          aria-label={t("BrowserPane.resize")}
          onPointerDown={onResizeStart}
          className="absolute inset-y-0 left-0 z-10 w-1.5 cursor-ew-resize bg-transparent transition-colors hover:bg-accent/40 active:bg-accent/60"
        />
      )}

      {/* Tab strip; doubles as this pane's draggable title-bar strip. The
          border-b lives on the outer wrapper so it always spans the full pane
          width; the row inside stops short of the app's fixed dock-toggle
          cluster (`trailingInset`), which floats over this corner. */}
      <div className="shrink-0 border-b border-border">
        <div
          data-tauri-drag-region={embedded ? undefined : true}
          className="flex h-11 items-center gap-1 px-2"
          style={trailingInset > 0 ? { marginRight: trailingInset } : undefined}
        >
          <div role="tablist" className="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto">
            {tabs.map((tab) => (
              <TabButton
                key={tab.id}
                tab={tab}
                active={tab.id === activeId}
                newTabLabel={t("BrowserPane.newTab")}
                closeLabel={t("BrowserPane.closeTab")}
                onSelect={() => void selectTab(tab.id)}
                onClose={() => void closeTab(tab.id)}
              />
            ))}
          </div>
          <IconButton
            size="sm"
            onClick={() => void openTab()}
            aria-label={t("BrowserPane.newTab")}
            title={t("BrowserPane.newTab")}
          >
            <Plus size={15} />
          </IconButton>
          <IconButton
            size="sm"
            onClick={() => (onClose ? onClose() : setOpen(false))}
            aria-label={t("BrowserPane.closePane")}
            title={t("BrowserPane.closePane")}
          >
            <X size={15} />
          </IconButton>
        </div>
      </div>

      {/* Toolbar: back / forward / reload + address bar + open external. */}
      <div className="flex h-11 shrink-0 items-center gap-1 border-b border-border px-2">
        <IconButton
          size="sm"
          disabled={!activeIsNative}
          onClick={() => active && void goBack(active.id)}
          aria-label={t("BrowserPane.back")}
          title={t("BrowserPane.back")}
        >
          <ArrowLeft size={15} />
        </IconButton>
        <IconButton
          size="sm"
          disabled={!activeIsNative}
          onClick={() => active && void goForward(active.id)}
          aria-label={t("BrowserPane.forward")}
          title={t("BrowserPane.forward")}
        >
          <ArrowRight size={15} />
        </IconButton>
        <IconButton
          size="sm"
          disabled={!activeIsNative}
          onClick={() => active && void reload(active.id)}
          aria-label={t("BrowserPane.reload")}
          title={t("BrowserPane.reload")}
        >
          <RotateCw size={15} />
        </IconButton>
        <input
          ref={addressRef}
          type="text"
          value={draft}
          spellCheck={false}
          autoCorrect="off"
          autoCapitalize="off"
          placeholder={t("BrowserPane.addressPlaceholder")}
          onChange={(event) => {
            setEditing(true);
            setDraft(event.target.value);
          }}
          onFocus={(event) => event.currentTarget.select()}
          onBlur={() => setEditing(false)}
          onKeyDown={(event) => {
            if (event.key === "Enter") submitAddress();
            if (event.key === "Escape") {
              setEditing(false);
              setDraft(active?.url ?? "");
              event.currentTarget.blur();
            }
          }}
          className="h-8 min-w-0 flex-1 rounded-lg border border-border bg-surface-2 px-3 text-xs text-foreground placeholder:text-faint focus:border-accent focus:outline-none"
        />
        <IconButton
          size="sm"
          disabled={!active?.url}
          onClick={() => active?.url && void openUrl(active.url)}
          aria-label={t("BrowserPane.openExternal")}
          title={t("BrowserPane.openExternal")}
        >
          <ExternalLink size={15} />
        </IconButton>
      </div>

      {error !== null && (
        <div className="flex shrink-0 items-start justify-between gap-2 border-b border-border bg-danger/10 px-3 py-2 text-xs text-danger">
          <span className="min-w-0 break-words">{error}</span>
          <button
            type="button"
            onClick={() => setError(null)}
            className="shrink-0 font-medium hover:underline"
          >
            {t("BrowserPane.dismiss")}
          </button>
        </div>
      )}

      {/* Content area. Native tabs paint a webview exactly over this div;
          local (not-yet-navigated) tabs render the start surface below. */}
      <div ref={contentRef} className="relative min-h-0 flex-1 bg-surface-2">
        {!activeIsNative && (
          <div className="flex h-full flex-col items-center justify-center gap-3 px-6 text-center">
            <Globe2 size={28} className="text-faint" />
            <p className="max-w-64 text-sm text-muted">
              {isTauri() ? t("BrowserPane.startHint") : t("BrowserPane.desktopOnly")}
            </p>
          </div>
        )}
      </div>
    </aside>
  );
}
