import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ClipboardList, Pencil, Shield, ShieldAlert, Zap } from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { usePermissionStore } from "../../store/permissionStore";
import type { PermissionMode } from "../../store/permissionStore";
import { Button } from "../ui";
import { useT } from "../../lib/i18n";

interface ModeMeta {
  icon: LucideIcon;
  labelKey: string;
  descriptionKey: string;
}

const MODE_META: Record<PermissionMode, ModeMeta> = {
  manual: {
    icon: Shield,
    labelKey: "ModeSelector.modeManualLabel",
    descriptionKey: "ModeSelector.modeManualDescription",
  },
  acceptEdits: {
    icon: Pencil,
    labelKey: "ModeSelector.modeAcceptEditsLabel",
    descriptionKey: "ModeSelector.modeAcceptEditsDescription",
  },
  plan: {
    icon: ClipboardList,
    labelKey: "ModeSelector.modePlanLabel",
    descriptionKey: "ModeSelector.modePlanDescription",
  },
  auto: {
    icon: Zap,
    labelKey: "ModeSelector.modeAutoLabel",
    descriptionKey: "ModeSelector.modeAutoDescription",
  },
  bypass: {
    icon: ShieldAlert,
    labelKey: "ModeSelector.modeBypassLabel",
    descriptionKey: "ModeSelector.modeBypassDescription",
  },
};

const ALL_MODES: PermissionMode[] = ["manual", "acceptEdits", "plan", "auto", "bypass"];

/**
 * Pill button + dropdown for switching the active permission mode (see
 * src-tauri/src/permissions.rs for what each mode actually does). Rendered
 * in ChatWindow's input area.
 */
export function ModeSelector() {
  const mode = usePermissionStore((s) => s.mode);
  const setMode = usePermissionStore((s) => s.setMode);

  const [open, setOpen] = useState(false);
  const [confirmingBypass, setConfirmingBypass] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

  // One-time startup sync: the Rust side always boots at "manual" (its own
  // Default impl), but the frontend may have restored a non-"manual" mode
  // from localStorage. Push that restored mode to the backend once so the
  // two sides agree, without going through `setMode` (which would re-write
  // the value we just read out of localStorage).
  useEffect(() => {
    if (mode === "manual") return;
    invoke("set_permission_mode", { mode }).catch((error) => {
      console.error("Failed to sync restored permission mode to backend", error);
    });
    // Intentionally run only once on mount.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (!open) return;
    function handlePointerDown(event: PointerEvent) {
      if (containerRef.current && !containerRef.current.contains(event.target as Node)) {
        setOpen(false);
        setConfirmingBypass(false);
      }
    }
    window.addEventListener("pointerdown", handlePointerDown);
    return () => window.removeEventListener("pointerdown", handlePointerDown);
  }, [open]);

  const { t } = useT();

  function closeDropdown() {
    setOpen(false);
    setConfirmingBypass(false);
  }

  function handleSelect(nextMode: PermissionMode) {
    if (nextMode === "bypass") {
      setConfirmingBypass(true);
      return;
    }
    void setMode(nextMode);
    closeDropdown();
  }

  function handleConfirmBypass() {
    void setMode("bypass");
    closeDropdown();
  }

  const current = MODE_META[mode];
  const CurrentIcon = current.icon;
  const isBypass = mode === "bypass";

  return (
    <div ref={containerRef} className="relative inline-block">
      <button
        type="button"
        onClick={() => setOpen((prev) => !prev)}
        aria-haspopup="true"
        aria-expanded={open}
        className={`inline-flex items-center gap-1.5 rounded-full px-2.5 py-1 text-xs font-medium transition-colors duration-150 cursor-pointer focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background ${
          isBypass ? "bg-danger-soft text-danger" : "bg-surface-2 text-muted hover:bg-surface hover:text-foreground"
        }`}
      >
        <CurrentIcon size={13} className="shrink-0" />
        {t(current.labelKey)}
      </button>

      {open && (
        <div className="absolute bottom-full left-0 z-20 mb-1 w-72 rounded-lg border border-border bg-background py-1 shadow-lg">
          {confirmingBypass ? (
            <div className="p-3">
              <p className="text-xs text-danger">{t("ModeSelector.bypassConfirmWarning")}</p>
              <div className="mt-3 flex justify-end gap-2">
                <Button type="button" variant="secondary" size="sm" onClick={() => setConfirmingBypass(false)}>
                  {t("ModeSelector.cancelButton")}
                </Button>
                <Button type="button" variant="danger" size="sm" onClick={handleConfirmBypass}>
                  {t("ModeSelector.confirmBypassButton")}
                </Button>
              </div>
            </div>
          ) : (
            ALL_MODES.map((m) => {
              const meta = MODE_META[m];
              const Icon = meta.icon;
              const isActive = m === mode;
              return (
                <button
                  key={m}
                  type="button"
                  onClick={() => handleSelect(m)}
                  className={`flex w-full cursor-pointer items-start gap-2 px-3 py-2 text-left ${
                    isActive ? "bg-accent-soft" : "hover:bg-surface-2"
                  }`}
                >
                  <Icon size={14} className={`mt-0.5 shrink-0 ${m === "bypass" ? "text-danger" : "text-faint"}`} />
                  <span className="min-w-0">
                    <span
                      className={`block text-sm font-medium ${
                        m === "bypass" ? "text-danger" : isActive ? "text-accent" : "text-foreground"
                      }`}
                    >
                      {t(meta.labelKey)}
                    </span>
                    <span className="block text-xs text-muted">{t(meta.descriptionKey)}</span>
                  </span>
                </button>
              );
            })
          )}
        </div>
      )}
    </div>
  );
}

export default ModeSelector;
