import { useEffect, useState } from "react";
import { AlertTriangle, X } from "lucide-react";
import { usePermissionStore } from "../../store/permissionStore";
import { IconButton } from "../ui";
import { useT } from "../../lib/i18n";

/**
 * Persistent, dismissible banner shown whenever one or more tools have been
 * granted "allow for session" status — i.e. the agent can invoke them going
 * forward without a further prompt. Session-wide grants are powerful (see
 * src-tauri/src/permissions.rs), so the user should always have a visible,
 * ambient reminder that unattended execution is active for those tools, not
 * just the one-time confirmation click that created the grant.
 *
 * Dismissing the banner only hides this indicator — it does not revoke the
 * underlying grant, which is intentional (this is a visibility affordance,
 * not a permissions control surface). The banner reappears automatically if
 * a new tool is granted session-wide access.
 */
export function SessionGrantBanner() {
  const sessionGrants = usePermissionStore((s) => s.sessionGrants);
  const [dismissed, setDismissed] = useState(false);
  const { t } = useT();

  // Re-surface the banner whenever the set of granted tools changes (e.g. a
  // second tool gets remembered after the user dismissed the banner for the
  // first one).
  useEffect(() => {
    setDismissed(false);
  }, [sessionGrants.join(",")]);

  if (sessionGrants.length === 0 || dismissed) return null;

  return (
    <div
      role="status"
      className="flex shrink-0 items-center gap-2 border-b border-border bg-warning-soft px-4 py-1.5 text-xs text-warning"
    >
      <AlertTriangle size={14} className="shrink-0" />
      <span className="min-w-0 flex-1 truncate">
        {t("SessionGrantBanner.unattendedAccessGranted")}{" "}
        <span className="font-mono font-medium">{sessionGrants.join(", ")}</span>
      </span>
      <IconButton
        size="sm"
        variant="ghost"
        onClick={() => setDismissed(true)}
        aria-label={t("SessionGrantBanner.dismissAriaLabel")}
        className="shrink-0 text-warning hover:text-warning"
      >
        <X size={14} />
      </IconButton>
    </div>
  );
}

export default SessionGrantBanner;
