import { useEffect, useRef, useState, type ReactNode } from "react";
import {
  Activity,
  Check,
  ChevronRight,
  ChevronsUpDown,
  GitPullRequest,
  Globe,
  HelpCircle,
  Search,
  Settings as SettingsIcon,
  SquareTerminal,
  Workflow,
} from "lucide-react";
import monkeyAvatar from "../../assets/monkey-avatar.png";
import { useT, LOCALES } from "../../lib/i18n";
import { useLocaleStore } from "../../store/localeStore";

const APP_VERSION = "0.1.0";

interface AppMenuProps {
  onOpenSettings: () => void;
  onOpenRunCenter: () => void;
  onOpenGlobalSearch: () => void;
  onOpenBrowserWorkbench: () => void;
  onOpenIssueToPr: () => void;
  onOpenSopCompiler: () => void;
  onOpenTerminal: () => void;
}

interface MenuRowProps {
  icon: ReactNode;
  label: string;
  onClick: () => void;
}

function MenuRow({ icon, label, onClick }: MenuRowProps) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2"
    >
      {icon}
      <span className="flex-1 truncate">{label}</span>
    </button>
  );
}

/**
 * Sidebar footer: replaces the account/org switcher row a hosted app would
 * show here (no accounts in this local app) with an app-branded trigger that
 * opens Settings and a language flyout (native-name list with a
 * checkmark on the active locale, mirroring the reference language picker).
 */
export function AppMenu({ onOpenSettings, onOpenRunCenter, onOpenGlobalSearch, onOpenBrowserWorkbench, onOpenIssueToPr, onOpenSopCompiler, onOpenTerminal }: AppMenuProps) {
  const { t } = useT();
  const locale = useLocaleStore((state) => state.locale);
  const setLocale = useLocaleStore((state) => state.setLocale);

  const [open, setOpen] = useState(false);
  const [langOpen, setLangOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    function handlePointerDown(event: PointerEvent) {
      if (containerRef.current && !containerRef.current.contains(event.target as Node)) {
        setOpen(false);
        setLangOpen(false);
      }
    }
    document.addEventListener("pointerdown", handlePointerDown);
    return () => document.removeEventListener("pointerdown", handlePointerDown);
  }, [open]);

  const closeAll = () => {
    setOpen(false);
    setLangOpen(false);
  };

  return (
    <div className="relative border-t border-border p-2" ref={containerRef}>
      <button
        type="button"
        onClick={() => setOpen((prev) => !prev)}
        aria-haspopup="menu"
        aria-expanded={open}
        className="flex w-full cursor-pointer items-center gap-2 rounded-md px-2 py-1.5 text-left hover:bg-surface-2"
      >
        <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md">
          <img src={monkeyAvatar} alt="" className="h-6 w-6 object-contain" />
        </span>
        <span className="flex-1 truncate text-sm font-medium text-foreground">Little Monkey</span>
        <ChevronsUpDown size={14} className="shrink-0 text-faint" />
      </button>

      {open && (
        <div className="absolute bottom-full left-2 mb-1 min-w-[220px] whitespace-nowrap rounded-lg border border-border bg-background py-1 shadow-lg z-30">
          <MenuRow
            icon={<Search size={14} className="text-faint" />}
            label={t("AppMenu.globalSearch")}
            onClick={() => {
              closeAll();
              onOpenGlobalSearch();
            }}
          />
          <MenuRow
            icon={<Activity size={14} className="text-faint" />}
            label={t("AppMenu.runCenter")}
            onClick={() => {
              closeAll();
              onOpenRunCenter();
            }}
          />
          <MenuRow
            icon={<Globe size={14} className="text-faint" />}
            label={t("AppMenu.browserWorkbench")}
            onClick={() => {
              closeAll();
              onOpenBrowserWorkbench();
            }}
          />
          <MenuRow
            icon={<GitPullRequest size={14} className="text-faint" />}
            label={t("AppMenu.issueToPr")}
            onClick={() => {
              closeAll();
              onOpenIssueToPr();
            }}
          />
          <MenuRow
            icon={<Workflow size={14} className="text-faint" />}
            label={t("AppMenu.sopCompiler")}
            onClick={() => {
              closeAll();
              onOpenSopCompiler();
            }}
          />
          <MenuRow
            icon={<SquareTerminal size={14} className="text-faint" />}
            label={t("AppMenu.integratedTerminal")}
            onClick={() => {
              closeAll();
              onOpenTerminal();
            }}
          />
          <MenuRow
            icon={<SettingsIcon size={14} className="text-faint" />}
            label={t("AppMenu.settings")}
            onClick={() => {
              closeAll();
              onOpenSettings();
            }}
          />
          <div className="my-1 border-t border-border" />

          <div
            className="relative"
            onMouseEnter={() => setLangOpen(true)}
            onMouseLeave={() => setLangOpen(false)}
          >
            <button
              type="button"
              onClick={() => setLangOpen((prev) => !prev)}
              aria-haspopup="menu"
              aria-expanded={langOpen}
              className={`flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2 ${
                langOpen ? "bg-surface-2" : ""
              }`}
            >
              <Globe size={14} className="text-faint" />
              <span className="flex-1 truncate">{t("AppMenu.language")}</span>
              <ChevronRight size={14} className="shrink-0 text-faint" />
            </button>

            {langOpen && (
              <div className="absolute left-full bottom-0 ml-1 max-h-[70vh] min-w-[220px] overflow-y-auto whitespace-nowrap rounded-lg border border-border bg-background py-1 shadow-lg z-30">
                {LOCALES.map((entry) => {
                  const isActive = entry.code === locale;
                  return (
                    <button
                      key={entry.code}
                      type="button"
                      onClick={() => {
                        setLocale(entry.code);
                        closeAll();
                      }}
                      className="flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2"
                    >
                      <span className="flex-1 truncate">{entry.nativeName}</span>
                      {isActive && <Check size={14} className="shrink-0 text-accent" />}
                    </button>
                  );
                })}
              </div>
            )}
          </div>

          <MenuRow
            icon={<HelpCircle size={14} className="text-faint" />}
            label={t("AppMenu.getHelp")}
            onClick={closeAll}
          />

          <div className="my-1 border-t border-border" />

          <div className="px-3 py-1.5 text-xs text-faint">
            {t("AppMenu.version", { version: APP_VERSION })}
          </div>
        </div>
      )}
    </div>
  );
}

export default AppMenu;
