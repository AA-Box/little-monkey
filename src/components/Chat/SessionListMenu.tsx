import { useEffect, useRef } from "react";
import { createPortal } from "react-dom";
import { Check, ChevronRight, Square, SquareCheck } from "lucide-react";

import { useT } from "../../lib/i18n";
import { useSessionListViewStore } from "../../store/sessionListViewStore";
import {
  environmentProvider,
  LOCAL_ENVIRONMENT,
  REMOTE_CONTROL_ENVIRONMENT,
} from "../../lib/conversationsClient";
import type { GroupBy, SortBy, StatusFilter } from "./sessionListView";

/** Matches the panel width below — used to place the portaled menu. */
const MENU_WIDTH = 232;
const VIEWPORT_MARGIN = 8;

const itemClass =
  "flex w-full cursor-pointer items-center justify-between gap-3 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2";

// Flush against the parent panel, for the same reason `SessionMenu`'s are: a
// gap between the two is a hover dead zone that closes the submenu.
const submenuClass =
  "invisible absolute left-full top-0 z-30 w-56 rounded-lg border border-border bg-background py-1 opacity-0 shadow-lg transition-opacity";

const STATUSES: StatusFilter[] = ["active", "archived", "all"];
const GROUP_BYS: GroupBy[] = ["date", "folder", "state", "groups", "none"];
const SORT_BYS: SortBy[] = ["alphabetical", "created", "recency"];

/**
 * How an environment names itself. The two built-ins that are not a messaging
 * provider have their own wording; a provider environment is titled by the
 * provider, which is the only name a user would recognize.
 */
export function useEnvironmentLabel() {
  const { t } = useT();
  return (environment: string) => {
    if (environment === LOCAL_ENVIRONMENT) return t("ChatSessionList.view.environment.local");
    if (environment === REMOTE_CONTROL_ENVIRONMENT) {
      return t("ChatSessionList.view.environment.remoteControl");
    }
    const provider = environmentProvider(environment);
    if (!provider) return environment;
    // Providers are proper nouns; capitalizing the stored token beats
    // shipping a translation table that goes stale with every new adapter.
    return provider.charAt(0).toUpperCase() + provider.slice(1);
  };
}

/**
 * The sidebar list's view menu: Status / Environment / Group by / Sort by,
 * each a hover submenu showing the value currently in force. Portaled and
 * positioned from the trigger's rect, exactly like `SessionMenu` — the
 * sidebar is an `overflow-y-auto` column, so an absolutely-positioned panel
 * (and any submenu of it) would be clipped at its edge.
 *
 * Choices are per-device and persist across launches
 * (`sessionListViewStore`); nothing here touches a session.
 */
export function SessionListMenu({
  anchorRect,
  environments,
  onClose,
}: {
  anchorRect: DOMRect;
  /** Every environment the filter offers — see `sessionListView`. */
  environments: readonly string[];
  onClose: () => void;
}) {
  const { t } = useT();
  const environmentLabel = useEnvironmentLabel();
  const prefs = useSessionListViewStore((state) => state.prefs);
  const setPrefs = useSessionListViewStore((state) => state.setPrefs);
  const toggleEnvironment = useSessionListViewStore((state) => state.toggleEnvironment);
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    function handlePointerDown(event: PointerEvent) {
      if (menuRef.current && !menuRef.current.contains(event.target as Node)) onClose();
    }
    function handleKeyDown(event: KeyboardEvent) {
      if (event.key === "Escape") onClose();
    }
    document.addEventListener("pointerdown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("pointerdown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const allEnvironments = prefs.environments.length === 0;
  const environmentSummary = allEnvironments
    ? t("ChatSessionList.view.environmentAll")
    : prefs.environments.length === 1
      ? environmentLabel(prefs.environments[0])
      : t("ChatSessionList.view.environmentCount", { count: prefs.environments.length });

  const left = Math.min(
    Math.max(anchorRect.right - MENU_WIDTH, VIEWPORT_MARGIN),
    window.innerWidth - MENU_WIDTH - VIEWPORT_MARGIN,
  );

  const row = (label: string, value: string) => (
    <>
      <span>{label}</span>
      <span className="flex min-w-0 items-center gap-1">
        <span className="truncate text-xs text-faint">{value}</span>
        <ChevronRight size={14} className="shrink-0 text-faint" />
      </span>
    </>
  );

  return createPortal(
    <div
      ref={menuRef}
      style={{ position: "fixed", top: anchorRect.bottom + 4, left, width: MENU_WIDTH }}
      className="z-30 rounded-lg border border-border bg-background py-1 shadow-lg"
      onClick={(event) => event.stopPropagation()}
      onPointerDown={(event) => event.stopPropagation()}
    >
      <div className="group/status relative">
        <button type="button" className={itemClass}>
          {row(t("ChatSessionList.view.status"), t(`ChatSessionList.view.status.${prefs.status}`))}
        </button>
        <div className={`${submenuClass} group-hover/status:visible group-hover/status:opacity-100`}>
          {STATUSES.map((status) => (
            <button key={status} type="button" onClick={() => setPrefs({ status })} className={itemClass}>
              <span className="truncate">{t(`ChatSessionList.view.status.${status}`)}</span>
              {prefs.status === status && <Check size={14} className="shrink-0 text-accent" />}
            </button>
          ))}
        </div>
      </div>

      <div className="group/environment relative">
        <button type="button" className={itemClass}>
          {row(t("ChatSessionList.view.environment"), environmentSummary)}
        </button>
        <div
          className={`${submenuClass} group-hover/environment:visible group-hover/environment:opacity-100`}
        >
          {/* Multi-select, so these are checkboxes rather than one chosen
              row: two environments at once is an ordinary thing to want, and
              "all" is simply none of them excluded. */}
          <button
            type="button"
            role="menuitemcheckbox"
            aria-checked={allEnvironments}
            onClick={() => setPrefs({ environments: [] })}
            className={itemClass}
          >
            <span className="flex min-w-0 items-center gap-2">
              {allEnvironments ? (
                <SquareCheck size={14} className="shrink-0 text-accent" />
              ) : (
                <Square size={14} className="shrink-0 text-faint" />
              )}
              <span className="truncate">{t("ChatSessionList.view.environmentAll")}</span>
            </span>
          </button>
          <div className="my-1 border-t border-border" />
          {environments.map((environment) => {
            const checked = allEnvironments || prefs.environments.includes(environment);
            return (
              <button
                key={environment}
                type="button"
                role="menuitemcheckbox"
                aria-checked={checked}
                onClick={() => toggleEnvironment(environment, environments)}
                className={itemClass}
              >
                <span className="flex min-w-0 items-center gap-2">
                  {checked ? (
                    <SquareCheck size={14} className="shrink-0 text-accent" />
                  ) : (
                    <Square size={14} className="shrink-0 text-faint" />
                  )}
                  <span className="truncate">{environmentLabel(environment)}</span>
                </span>
              </button>
            );
          })}
        </div>
      </div>

      <div className="my-1 border-t border-border" />

      <div className="group/groupby relative">
        <button type="button" className={itemClass}>
          {row(t("ChatSessionList.view.groupBy"), t(`ChatSessionList.view.groupBy.${prefs.groupBy}`))}
        </button>
        <div className={`${submenuClass} group-hover/groupby:visible group-hover/groupby:opacity-100`}>
          {GROUP_BYS.map((groupBy) => (
            <div key={groupBy}>
              {/* "None" is the absence of grouping rather than another way to
                  group, and reads as one only when it is set apart. */}
              {groupBy === "none" && <div className="my-1 border-t border-border" />}
              <button type="button" onClick={() => setPrefs({ groupBy })} className={itemClass}>
                <span className="truncate">{t(`ChatSessionList.view.groupBy.${groupBy}`)}</span>
                {prefs.groupBy === groupBy && <Check size={14} className="shrink-0 text-accent" />}
              </button>
            </div>
          ))}
        </div>
      </div>

      <div className="group/sortby relative">
        <button type="button" className={itemClass}>
          {row(t("ChatSessionList.view.sortBy"), t(`ChatSessionList.view.sortBy.${prefs.sortBy}`))}
        </button>
        <div className={`${submenuClass} group-hover/sortby:visible group-hover/sortby:opacity-100`}>
          {SORT_BYS.map((sortBy) => (
            <button key={sortBy} type="button" onClick={() => setPrefs({ sortBy })} className={itemClass}>
              <span className="truncate">{t(`ChatSessionList.view.sortBy.${sortBy}`)}</span>
              {prefs.sortBy === sortBy && <Check size={14} className="shrink-0 text-accent" />}
            </button>
          ))}
        </div>
      </div>
    </div>,
    document.body,
  );
}

export default SessionListMenu;
