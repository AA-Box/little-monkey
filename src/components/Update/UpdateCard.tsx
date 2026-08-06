import { ArrowRight } from "lucide-react";

import monkeyAvatar from "../../assets/monkey-avatar.png";
import { installsWhileRunning } from "../../lib/appUpdater";
import { useT } from "../../lib/i18n";
import { useUpdateStore } from "../../store/updateStore";

/**
 * The Claude-Desktop-style update card: a small floating panel pinned to the
 * bottom of the session sidebar, shown only once the new version is staged.
 * Clicking it restarts into the new build (macOS/Linux), or runs the
 * downloaded installer (Windows, which restarts the app itself).
 *
 * Deliberately not dismissible and deliberately not a dialog — the download
 * has already happened by the time it appears, so the only remaining choice
 * is "now" or "next time you quit anyway".
 */
export function UpdateCard() {
  const { t } = useT();
  const status = useUpdateStore((s) => s.status);
  const version = useUpdateStore((s) => s.version);
  const notes = useUpdateStore((s) => s.notes);
  const applyUpdate = useUpdateStore((s) => s.applyUpdate);

  if (status !== "ready" && status !== "applying") return null;

  const applying = status === "applying";
  // Windows runs an installer first, so the label promises an install rather
  // than the instant relaunch macOS/Linux get.
  const label = installsWhileRunning() ? t("Update.relaunchToUpdate") : t("Update.installUpdate");

  return (
    <button
      type="button"
      onClick={() => void applyUpdate()}
      disabled={applying}
      title={notes ?? undefined}
      className="group absolute inset-x-2 bottom-2 z-20 flex cursor-pointer items-center gap-3 rounded-2xl border border-border bg-background px-3 py-2.5 text-left shadow-lg disabled:cursor-default disabled:opacity-70"
    >
      <img src={monkeyAvatar} alt="" className="h-9 w-9 shrink-0 rounded-lg" />
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm font-semibold text-foreground">
          {applying ? t("Update.applying") : label}
        </span>
        {version !== null && <span className="block truncate text-xs text-muted">v{version}</span>}
      </span>
      <ArrowRight className="h-4 w-4 shrink-0 text-muted transition-transform group-hover:translate-x-0.5" />
    </button>
  );
}

export default UpdateCard;
