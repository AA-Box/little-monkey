import { useEffect, useRef, useState, type ReactNode } from "react";
import {
  Activity,
  BookOpenText,
  Check,
  ChevronRight,
  ChevronsUpDown,
  ClipboardList,
  GitBranch,
  Command,
  ClipboardCheck,
  Compass,
  FileDiff,
  Database,
  GitPullRequest,
  Globe,
  HelpCircle,
  LayoutDashboard,
  ListTodo,
  Inbox,
  Network,
  Newspaper,
  Plug,
  Radar,
  Search,
  ServerCog,
  Settings as SettingsIcon,
  ShieldAlert,
  ShieldCheck,
  SquareTerminal,
  Table2,
  Telescope,
  Wand2,
  Workflow,
  Swords,
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
  onOpenCommandPalette: () => void;
  onOpenIssueToPr: () => void;
  onOpenSecurityAutofix: () => void;
  onOpenTrustScorecards: () => void;
  onOpenSopCompiler: () => void;
  onOpenMcpGenerator: () => void;
  onOpenConnectorBuilder: () => void;
  onOpenMigrationAgent: () => void;
  onOpenSideTasks: () => void;
  onOpenAgentInbox: () => void;
  onOpenKnowledgeGraph: () => void;
  onOpenSpreadsheetCopilot: () => void;
  onOpenTerminal: () => void;
  onOpenRedTeamLab: () => void;
  onOpenEvidenceBoard: () => void;
  onOpenDebate: () => void;
  onOpenDbAdminGuardrails: () => void;
  onRestartOnboarding: () => void;
  onOpenDailyBrief: () => void;
  onOpenApiContractDiffLab: () => void;
  onOpenGoldenDatasetBuilder: () => void;
  onOpenDataNotebook: () => void;
  onOpenSyntheticMonitoring: () => void;
  onOpenCrossRepoIntelligence: () => void;
  onOpenWorkCanvas: () => void;
  onOpenPmCopilot: () => void;
  onOpenDeepResearch: () => void;
  onOpenBriefStudio: () => void;
  onOpenCrossRepoChangePlanner: () => void;
  onOpenVisualEditMode: () => void;
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

interface MenuFlyoutProps {
  icon: ReactNode;
  label: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  children: ReactNode;
}

/**
 * A submenu row that opens a flyout panel to its side (same interaction as
 * the language picker: hover to open, click to toggle). The flyout anchors
 * to the row's bottom edge and grows upward, since the whole menu sits at
 * the bottom of the sidebar.
 */
function MenuFlyout({ icon, label, open, onOpenChange, children }: MenuFlyoutProps) {
  return (
    <div
      className="relative"
      onMouseEnter={() => onOpenChange(true)}
      onMouseLeave={() => onOpenChange(false)}
    >
      <button
        type="button"
        onClick={() => onOpenChange(!open)}
        aria-haspopup="menu"
        aria-expanded={open}
        className={`flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2 ${
          open ? "bg-surface-2" : ""
        }`}
      >
        {icon}
        <span className="flex-1 truncate">{label}</span>
        <ChevronRight size={14} className="shrink-0 text-faint" />
      </button>

      {open && (
        <div className="absolute left-full bottom-0 ml-1 max-h-[70vh] min-w-[220px] overflow-y-auto whitespace-nowrap rounded-lg border border-border bg-background py-1 shadow-lg z-30">
          {children}
        </div>
      )}
    </div>
  );
}

/**
 * Sidebar footer: replaces the account/org switcher row a hosted app would
 * show here (no accounts in this local app) with an app-branded trigger that
 * opens the app menu. Everyday surfaces (search, palette, brief, inbox, runs)
 * stay top-level; the tool catalog is grouped into flyout submenus so the
 * menu itself stays a single short column; Settings/Language/help sit in a
 * footer section below the groups.
 */
export function AppMenu({
  onOpenSettings,
  onOpenRunCenter,
  onOpenGlobalSearch,
  onOpenBrowserWorkbench,
  onOpenCommandPalette,
  onOpenIssueToPr,
  onOpenSecurityAutofix,
  onOpenTrustScorecards,
  onOpenSopCompiler,
  onOpenMcpGenerator,
  onOpenConnectorBuilder,
  onOpenMigrationAgent,
  onOpenSideTasks,
  onOpenAgentInbox,
  onOpenKnowledgeGraph,
  onOpenSpreadsheetCopilot,
  onOpenTerminal,
  onOpenRedTeamLab,
  onOpenEvidenceBoard,
  onOpenDebate,
  onOpenDbAdminGuardrails,
  onRestartOnboarding,
  onOpenDailyBrief,
  onOpenApiContractDiffLab,
  onOpenGoldenDatasetBuilder,
  onOpenDataNotebook,
  onOpenSyntheticMonitoring,
  onOpenCrossRepoIntelligence,
  onOpenWorkCanvas,
  onOpenPmCopilot,
  onOpenDeepResearch,
  onOpenBriefStudio,
  onOpenCrossRepoChangePlanner,
  onOpenVisualEditMode,
}: AppMenuProps) {
  const { t } = useT();
  const locale = useLocaleStore((state) => state.locale);
  const setLocale = useLocaleStore((state) => state.setLocale);

  const [open, setOpen] = useState(false);
  const [openFlyout, setOpenFlyout] = useState<string | null>(null);
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    function handlePointerDown(event: PointerEvent) {
      if (containerRef.current && !containerRef.current.contains(event.target as Node)) {
        setOpen(false);
        setOpenFlyout(null);
      }
    }
    document.addEventListener("pointerdown", handlePointerDown);
    return () => document.removeEventListener("pointerdown", handlePointerDown);
  }, [open]);

  const closeAll = () => {
    setOpen(false);
    setOpenFlyout(null);
  };

  const iconClass = "text-faint";

  interface Item {
    key: string;
    icon: ReactNode;
    label: string;
    onOpen: () => void;
  }

  const topItems: Item[] = [
    { key: "globalSearch", icon: <Search size={14} className={iconClass} />, label: t("AppMenu.globalSearch"), onOpen: onOpenGlobalSearch },
    { key: "commandPalette", icon: <Command size={14} className={iconClass} />, label: t("AppMenu.commandPalette"), onOpen: onOpenCommandPalette },
    { key: "dailyBrief", icon: <Newspaper size={14} className={iconClass} />, label: t("AppMenu.dailyBrief"), onOpen: onOpenDailyBrief },
    { key: "agentInbox", icon: <Inbox size={14} className={iconClass} />, label: t("AppMenu.agentInbox"), onOpen: onOpenAgentInbox },
    { key: "runCenter", icon: <Activity size={14} className={iconClass} />, label: t("AppMenu.runCenter"), onOpen: onOpenRunCenter },
  ];

  const groups: { key: string; icon: ReactNode; label: string; items: Item[] }[] = [
    {
      key: "buildShip",
      icon: <GitBranch size={14} className={iconClass} />,
      label: t("AppMenu.groupBuildShip"),
      items: [
        { key: "issueToPr", icon: <GitPullRequest size={14} className={iconClass} />, label: t("AppMenu.issueToPr"), onOpen: onOpenIssueToPr },
        { key: "visualEditMode", icon: <Wand2 size={14} className={iconClass} />, label: t("AppMenu.visualEditMode"), onOpen: onOpenVisualEditMode },
        { key: "browserWorkbench", icon: <Globe size={14} className={iconClass} />, label: t("AppMenu.browserWorkbench"), onOpen: onOpenBrowserWorkbench },
        { key: "integratedTerminal", icon: <SquareTerminal size={14} className={iconClass} />, label: t("AppMenu.integratedTerminal"), onOpen: onOpenTerminal },
        { key: "migrationAgent", icon: <GitBranch size={14} className={iconClass} />, label: t("AppMenu.migrationAgent"), onOpen: onOpenMigrationAgent },
        { key: "crossRepoChangePlanner", icon: <GitBranch size={14} className={iconClass} />, label: t("AppMenu.crossRepoChangePlanner"), onOpen: onOpenCrossRepoChangePlanner },
        { key: "crossRepoIntelligence", icon: <Network size={14} className={iconClass} />, label: t("AppMenu.crossRepoIntelligence"), onOpen: onOpenCrossRepoIntelligence },
        { key: "apiContractDiffLab", icon: <FileDiff size={14} className={iconClass} />, label: t("AppMenu.apiContractDiffLab"), onOpen: onOpenApiContractDiffLab },
      ],
    },
    {
      key: "agentsAutomation",
      icon: <Workflow size={14} className={iconClass} />,
      label: t("AppMenu.groupAgentsAutomation"),
      items: [
        { key: "sopCompiler", icon: <Workflow size={14} className={iconClass} />, label: t("AppMenu.sopCompiler"), onOpen: onOpenSopCompiler },
        { key: "mcpGenerator", icon: <ServerCog size={14} className={iconClass} />, label: t("AppMenu.mcpGenerator"), onOpen: onOpenMcpGenerator },
        { key: "connectorBuilder", icon: <Plug size={14} className={iconClass} />, label: t("AppMenu.connectorBuilder"), onOpen: onOpenConnectorBuilder },
        { key: "syntheticMonitoring", icon: <Radar size={14} className={iconClass} />, label: t("AppMenu.syntheticMonitoring"), onOpen: onOpenSyntheticMonitoring },
        { key: "sideTasks", icon: <ListTodo size={14} className={iconClass} />, label: t("AppMenu.sideTasks"), onOpen: onOpenSideTasks },
        { key: "debate", icon: <Swords size={14} className={iconClass} />, label: t("AppMenu.debate"), onOpen: onOpenDebate },
      ],
    },
    {
      key: "dataResearch",
      icon: <Database size={14} className={iconClass} />,
      label: t("AppMenu.groupDataResearch"),
      items: [
        { key: "knowledgeGraphExplorer", icon: <Network size={14} className={iconClass} />, label: t("AppMenu.knowledgeGraphExplorer"), onOpen: onOpenKnowledgeGraph },
        { key: "spreadsheetCopilot", icon: <Table2 size={14} className={iconClass} />, label: t("AppMenu.spreadsheetCopilot"), onOpen: onOpenSpreadsheetCopilot },
        { key: "dataNotebook", icon: <Database size={14} className={iconClass} />, label: t("AppMenu.dataNotebook"), onOpen: onOpenDataNotebook },
        { key: "goldenDatasetBuilder", icon: <Database size={14} className={iconClass} />, label: t("AppMenu.goldenDatasetBuilder"), onOpen: onOpenGoldenDatasetBuilder },
        { key: "evidenceBoard", icon: <ClipboardCheck size={14} className={iconClass} />, label: t("AppMenu.evidenceBoard"), onOpen: onOpenEvidenceBoard },
        { key: "deepResearch", icon: <Telescope size={14} className={iconClass} />, label: t("AppMenu.deepResearch"), onOpen: onOpenDeepResearch },
      ],
    },
    {
      key: "planningDocs",
      icon: <ClipboardList size={14} className={iconClass} />,
      label: t("AppMenu.groupPlanningDocs"),
      items: [
        { key: "workCanvas", icon: <LayoutDashboard size={14} className={iconClass} />, label: t("AppMenu.workCanvas"), onOpen: onOpenWorkCanvas },
        { key: "pmCopilot", icon: <ClipboardList size={14} className={iconClass} />, label: t("AppMenu.pmCopilot"), onOpen: onOpenPmCopilot },
        { key: "briefStudio", icon: <BookOpenText size={14} className={iconClass} />, label: t("AppMenu.briefStudio"), onOpen: onOpenBriefStudio },
      ],
    },
    {
      key: "securityTrust",
      icon: <ShieldCheck size={14} className={iconClass} />,
      label: t("AppMenu.groupSecurityTrust"),
      items: [
        { key: "securityAutofix", icon: <ShieldAlert size={14} className={iconClass} />, label: t("AppMenu.securityAutofix"), onOpen: onOpenSecurityAutofix },
        { key: "redTeamLab", icon: <ShieldAlert size={14} className={iconClass} />, label: t("AppMenu.redTeamLab"), onOpen: onOpenRedTeamLab },
        { key: "trustScorecards", icon: <ShieldCheck size={14} className={iconClass} />, label: t("AppMenu.trustScorecards"), onOpen: onOpenTrustScorecards },
        { key: "dbAdminGuardrails", icon: <Database size={14} className={iconClass} />, label: t("AppMenu.dbAdminGuardrails"), onOpen: onOpenDbAdminGuardrails },
      ],
    },
  ];

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
          {topItems.map((item) => (
            <MenuRow
              key={item.key}
              icon={item.icon}
              label={item.label}
              onClick={() => {
                closeAll();
                item.onOpen();
              }}
            />
          ))}

          <div className="my-1 border-t border-border" />

          {groups.map((group) => (
            <MenuFlyout
              key={group.key}
              icon={group.icon}
              label={group.label}
              open={openFlyout === group.key}
              onOpenChange={(next) => setOpenFlyout(next ? group.key : null)}
            >
              {group.items.map((item) => (
                <MenuRow
                  key={item.key}
                  icon={item.icon}
                  label={item.label}
                  onClick={() => {
                    closeAll();
                    item.onOpen();
                  }}
                />
              ))}
            </MenuFlyout>
          ))}

          <div className="my-1 border-t border-border" />

          <MenuRow
            icon={<SettingsIcon size={14} className="text-faint" />}
            label={t("AppMenu.settings")}
            onClick={() => {
              closeAll();
              onOpenSettings();
            }}
          />

          <MenuFlyout
            icon={<Globe size={14} className="text-faint" />}
            label={t("AppMenu.language")}
            open={openFlyout === "language"}
            onOpenChange={(next) => setOpenFlyout(next ? "language" : null)}
          >
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
          </MenuFlyout>

          <MenuRow
            icon={<Compass size={14} className="text-faint" />}
            label={t("AppMenu.restartOnboarding")}
            onClick={() => {
              closeAll();
              onRestartOnboarding();
            }}
          />
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
