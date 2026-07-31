import { lazy } from "react";

// Keep every on-demand surface behind its own import boundary. Importing the
// component files directly (instead of their barrel files) prevents unrelated
// exports from being retained in the same async chunk.
export const CompareView = lazy(() => import("../components/Chat/CompareView"));
export const CrewView = lazy(() =>
  import("../components/Chat/CrewView").then(({ CrewView }) => ({ default: CrewView })),
);
export const RunCenter = lazy(() =>
  import("../components/Runs/RunCenter").then(({ RunCenter }) => ({ default: RunCenter })),
);
export const BrowserPane = lazy(() =>
  import("../components/Browser/BrowserPane").then(({ BrowserPane }) => ({ default: BrowserPane })),
);
export const BrowserWorkbench = lazy(() =>
  import("../components/Browser/BrowserWorkbench").then(({ BrowserWorkbench }) => ({ default: BrowserWorkbench })),
);
export const IssueToPrPanel = lazy(() =>
  import("../components/IssueToPr/IssueToPrPanel").then(({ IssueToPrPanel }) => ({ default: IssueToPrPanel })),
);
export const ProductionDebuggingPanel = lazy(() =>
  import("../components/ProductionDebugging/ProductionDebuggingPanel").then(({ ProductionDebuggingPanel }) => ({
    default: ProductionDebuggingPanel,
  })),
);
export const IncidentCommanderPanel = lazy(() =>
  import("../components/IncidentCommander/IncidentCommanderPanel").then(({ IncidentCommanderPanel }) => ({
    default: IncidentCommanderPanel,
  })),
);
export const DesignToAppPanel = lazy(() =>
  import("../components/DesignToApp/DesignToAppPanel").then(({ DesignToAppPanel }) => ({
    default: DesignToAppPanel,
  })),
);
export const SecurityAutofixPanel = lazy(() =>
  import("../components/SecurityAutofix/SecurityAutofixPanel").then(({ SecurityAutofixPanel }) => ({
    default: SecurityAutofixPanel,
  })),
);
export const TrustScorecardsPanel = lazy(() =>
  import("../components/TrustScorecards/TrustScorecardsPanel").then(({ TrustScorecardsPanel }) => ({
    default: TrustScorecardsPanel,
  })),
);
export const SopCompilerPanel = lazy(() =>
  import("../components/SopCompiler/SopCompilerPanel").then(({ SopCompilerPanel }) => ({
    default: SopCompilerPanel,
  })),
);
export const McpGeneratorPanel = lazy(() =>
  import("../components/McpGenerator/McpGeneratorPanel").then(({ McpGeneratorPanel }) => ({
    default: McpGeneratorPanel,
  })),
);
export const ConnectorBuilderPanel = lazy(() =>
  import("../components/ConnectorBuilder/ConnectorBuilderPanel").then(({ ConnectorBuilderPanel }) => ({
    default: ConnectorBuilderPanel,
  })),
);
export const MigrationAgentPanel = lazy(() =>
  import("../components/MigrationAgent/MigrationAgentPanel").then(({ MigrationAgentPanel }) => ({
    default: MigrationAgentPanel,
  })),
);
export const SideTaskPane = lazy(() =>
  import("../components/SideTasks/SideTaskPane").then(({ SideTaskPane }) => ({ default: SideTaskPane })),
);
export const BackgroundTasksPanel = lazy(() =>
  import("../components/BackgroundTasks/BackgroundTasksPanel").then(({ BackgroundTasksPanel }) => ({
    default: BackgroundTasksPanel,
  })),
);
export const GlobalSearch = lazy(() =>
  import("../components/Search/GlobalSearch").then(({ GlobalSearch }) => ({ default: GlobalSearch })),
);
export const CommandPalette = lazy(() =>
  import("../components/Palette/CommandPalette").then(({ CommandPalette }) => ({ default: CommandPalette })),
);
export const AgentInbox = lazy(() =>
  import("../components/Inbox/AgentInbox").then(({ AgentInbox }) => ({ default: AgentInbox })),
);
export const RedTeamLabPanel = lazy(() =>
  import("../components/RedTeamLab/RedTeamLabPanel").then(({ RedTeamLabPanel }) => ({ default: RedTeamLabPanel })),
);
export const KnowledgeGraphExplorerPanel = lazy(() =>
  import("../components/KnowledgeGraphExplorer/KnowledgeGraphExplorerPanel").then(
    ({ KnowledgeGraphExplorerPanel }) => ({ default: KnowledgeGraphExplorerPanel }),
  ),
);
export const SpreadsheetCopilotPanel = lazy(() =>
  import("../components/SpreadsheetCopilot/SpreadsheetCopilotPanel").then(({ SpreadsheetCopilotPanel }) => ({
    default: SpreadsheetCopilotPanel,
  })),
);
export const EvidenceBoardPanel = lazy(() =>
  import("../components/EvidenceBoard/EvidenceBoardPanel").then(({ EvidenceBoardPanel }) => ({
    default: EvidenceBoardPanel,
  })),
);
export const GoldenDatasetBuilderPanel = lazy(() =>
  import("../components/GoldenDatasetBuilder/GoldenDatasetBuilderPanel").then(({ GoldenDatasetBuilderPanel }) => ({
    default: GoldenDatasetBuilderPanel,
  })),
);
export const DailyBriefPanel = lazy(() =>
  import("../components/DailyBrief/DailyBriefPanel").then(({ DailyBriefPanel }) => ({ default: DailyBriefPanel })),
);
export const DataNotebookPanel = lazy(() =>
  import("../components/DataNotebook/DataNotebookPanel").then(({ DataNotebookPanel }) => ({
    default: DataNotebookPanel,
  })),
);
export const SyntheticMonitoringPanel = lazy(() =>
  import("../components/SyntheticMonitoring/SyntheticMonitoringPanel").then(({ SyntheticMonitoringPanel }) => ({
    default: SyntheticMonitoringPanel,
  })),
);
export const CrossRepoIntelligencePanel = lazy(() =>
  import("../components/CrossRepoIntelligence/CrossRepoIntelligencePanel").then(
    ({ CrossRepoIntelligencePanel }) => ({ default: CrossRepoIntelligencePanel }),
  ),
);
export const WorkCanvasPanel = lazy(() =>
  import("../components/WorkCanvas/WorkCanvasPanel").then(({ WorkCanvasPanel }) => ({
    default: WorkCanvasPanel,
  })),
);
export const PmCopilotPanel = lazy(() =>
  import("../components/PmCopilot/PmCopilotPanel").then(({ PmCopilotPanel }) => ({ default: PmCopilotPanel })),
);
export const DeepResearchWorkspacePanel = lazy(() =>
  import("../components/DeepResearchWorkspace/DeepResearchWorkspacePanel").then(
    ({ DeepResearchWorkspacePanel }) => ({ default: DeepResearchWorkspacePanel }),
  ),
);
export const BriefStudioPanel = lazy(() =>
  import("../components/BriefStudio/BriefStudioPanel").then(({ BriefStudioPanel }) => ({
    default: BriefStudioPanel,
  })),
);
export const CrossRepoChangePlannerPanel = lazy(() =>
  import("../components/CrossRepoChangePlanner/CrossRepoChangePlannerPanel").then(
    ({ CrossRepoChangePlannerPanel }) => ({ default: CrossRepoChangePlannerPanel }),
  ),
);
export const VisualEditModePanel = lazy(() =>
  import("../components/VisualEditMode/VisualEditModePanel").then(({ VisualEditModePanel }) => ({
    default: VisualEditModePanel,
  })),
);
export const TerminalPanel = lazy(() =>
  import("../components/Terminal/TerminalPanel").then(({ TerminalPanel }) => ({ default: TerminalPanel })),
);
export const DebatePanel = lazy(() =>
  import("../components/Debate/DebatePanel").then(({ DebatePanel }) => ({ default: DebatePanel })),
);
export const DatabaseAdminGuardrailsPanel = lazy(() =>
  import("../components/DatabaseAdminGuardrails/DatabaseAdminGuardrailsPanel").then(
    ({ DatabaseAdminGuardrailsPanel }) => ({ default: DatabaseAdminGuardrailsPanel }),
  ),
);
export const ApiContractDiffLabPanel = lazy(() =>
  import("../components/ApiContractDiffLab/ApiContractDiffLabPanel").then(({ ApiContractDiffLabPanel }) => ({
    default: ApiContractDiffLabPanel,
  })),
);
export const EvalHarnessPanel = lazy(() =>
  import("../components/EvalHarness/EvalHarnessPanel").then(({ EvalHarnessPanel }) => ({
    default: EvalHarnessPanel,
  })),
);
export const SettingsModal = lazy(() =>
  import("../components/Settings/SettingsModal").then(({ SettingsModal }) => ({ default: SettingsModal })),
);
export const OnboardingWizard = lazy(() =>
  import("../components/Onboarding/OnboardingWizard").then(({ OnboardingWizard }) => ({
    default: OnboardingWizard,
  })),
);
export const ArtifactPane = lazy(() =>
  import("../components/Workspace/ArtifactPane").then(({ ArtifactPane }) => ({ default: ArtifactPane })),
);
export const FileTree = lazy(() =>
  import("../components/Workspace/FileTree").then(({ FileTree }) => ({ default: FileTree })),
);
export const DiffPanel = lazy(() =>
  import("../components/Workspace/DiffPanel").then(({ DiffPanel }) => ({ default: DiffPanel })),
);
export const DiffViewer = lazy(() =>
  import("../components/Workspace/DiffViewer").then(({ DiffViewer }) => ({ default: DiffViewer })),
);
export const PermissionModal = lazy(() =>
  import("../components/Workspace/PermissionModal").then(({ PermissionModal }) => ({ default: PermissionModal })),
);
export const ApprovalChainModal = lazy(() =>
  import("../components/Workspace/ApprovalChainModal").then(({ ApprovalChainModal }) => ({
    default: ApprovalChainModal,
  })),
);
export const ReviewPanel = lazy(() =>
  import("../components/Workspace/ReviewPanel").then(({ ReviewPanel }) => ({ default: ReviewPanel })),
);
