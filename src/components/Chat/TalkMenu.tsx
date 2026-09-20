/**
 * The controls Talk needs that do not belong in the composer's main row.
 *
 * Talk used to be a whole page, and everything it offered beyond "listen now" —
 * which microphone, which speaker, push-to-talk or continuous, and whether a
 * transcription backend exists at all — lived only there. The composer's mic
 * button was the same feature with every choice removed. This tray is where
 * those choices went, so there is one Talk and not two that drift apart.
 *
 * The hook result arrives as a prop rather than being called here: the composer
 * already owns exactly one `useTalkSession` per conversation, and a second live
 * one would answer the same spoken sentence twice — `handledEventRef` and
 * `routeCursorRef` are per-instance. Taking it as a prop also makes this
 * testable without stubbing `getUserMedia`.
 */
import { useEffect, useRef, useState } from "react";
import { Settings2, SlidersHorizontal } from "lucide-react";

import { Button, IconButton } from "../ui";
import { useT } from "../../lib/i18n";
import { VoiceRouteSelector } from "../Talk/VoiceRouteSelector";
import type { VoiceRouteEngine, VoiceRouteRecord } from "../../lib/daemonClient";
import type { TalkMode } from "../../lib/talkEngine";
import type { UseTalkSession } from "../Talk/useTalkSession";

export function TalkMenu({
  sessionId,
  engine,
  talk,
  mode,
  onModeChange,
  onRoute,
  onOpenVoiceSettings,
}: {
  sessionId: string;
  engine: VoiceRouteEngine;
  talk: UseTalkSession;
  mode: TalkMode;
  onModeChange: (mode: TalkMode) => void;
  onRoute: (route: VoiceRouteRecord) => void;
  onOpenVoiceSettings: () => void;
}) {
  const { t } = useT();
  const [open, setOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (event: PointerEvent) => {
      if (!containerRef.current?.contains(event.target as Node)) setOpen(false);
    };
    window.addEventListener("pointerdown", onPointerDown);
    return () => window.removeEventListener("pointerdown", onPointerDown);
  }, [open]);

  return (
    <div ref={containerRef} className="relative inline-block">
      <IconButton
        type="button"
        variant="ghost"
        size="sm"
        aria-label={t("TalkMenu.voiceOptionsAriaLabel")}
        aria-haspopup="true"
        aria-expanded={open}
        onClick={() => setOpen((prev) => !prev)}
      >
        <SlidersHorizontal size={16} />
      </IconButton>

      {open && (
        <div className="absolute bottom-full right-0 z-20 mb-1 w-80 rounded-lg border border-border bg-background p-3 shadow-lg">
          {/* Only meaningful once Talk has read its configuration, which the
              hook does when it is enabled — before that `status` is null and
              claiming a backend is missing would be a guess. */}
          {talk.status?.configured === false && (
            <div role="alert" className="mb-2 rounded-md border border-warning/40 bg-warning/10 p-2 text-[11px]">
              <p className="text-warning">{t("TalkMenu.noTranscriptionBackend")}</p>
              <Button className="mt-1.5" size="sm" variant="secondary" onClick={onOpenVoiceSettings}>
                {t("TalkMenu.openVoiceSettings")}
              </Button>
            </div>
          )}

          <VoiceRouteSelector sessionId={sessionId} engine={engine} onRoute={onRoute} />

          {/* Pipeline only. The realtime engine's turn detection is a persisted
              setting rather than a runtime mode, so wiring this checkbox to it
              would toggle nothing — see RealtimeTalkBar. */}
          {engine === "pipeline" && (
            <label className="mt-2 flex items-center gap-2 text-xs text-muted">
              <input
                type="checkbox"
                checked={mode === "continuous"}
                onChange={(event) => onModeChange(event.target.checked ? "continuous" : "push_to_talk")}
              />
              {t("TalkMenu.continuous")}
            </label>
          )}

          <button
            type="button"
            onClick={onOpenVoiceSettings}
            className="mt-2 flex cursor-pointer items-center gap-1.5 text-[11px] text-faint hover:text-foreground"
          >
            <Settings2 size={12} />
            {t("TalkMenu.moreVoiceSettings")}
          </button>
        </div>
      )}
    </div>
  );
}
