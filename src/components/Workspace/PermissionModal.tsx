import { useEffect, useState } from "react";
import { AlertTriangle, Brain, FilePenLine, FileText, Folder, Globe, Plug, Search, TerminalSquare } from "lucide-react";
import { usePermissionStore } from "../../store/permissionStore";
import { Button } from "../ui";
import { useT } from "../../lib/i18n";

/** Maps a tool name to the icon shown in the dialog's tinted circle. An MCP
 * tool call's permission-request name (`mcp:<serverId>:<toolName>`, see
 * `src-tauri/src/mcp.rs::mcp_call_tool`) isn't a fixed key here — it's
 * matched by prefix below instead, since the server/tool half varies. */
const TOOL_ICONS: Record<string, typeof AlertTriangle> = {
  run_shell: TerminalSquare,
  write_file: FilePenLine,
  edit_file: FilePenLine,
  read_file: FileText,
  list_dir: Folder,
  grep: Search,
  remember: Brain,
  web_fetch: Globe,
};

/**
 * Centered dialog shown whenever the agent needs the user's sign-off
 * before running a sensitive tool (write_file / edit_file / run_shell / remember / web_fetch).
 * Mirrors permissionStore.pending exactly — {id, tool, detail} | null.
 */
export function PermissionModal() {
  const pending = usePermissionStore((s) => s.pending);
  const respond = usePermissionStore((s) => s.respond);
  const [entered, setEntered] = useState(false);
  const { t } = useT();

  useEffect(() => {
    if (!pending) return;
    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") {
        respond(false, false);
      }
    }
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [pending, respond]);

  // Small mount transition (opacity/scale) — duration collapses to ~0 under
  // prefers-reduced-motion via the global rule in src/index.css.
  useEffect(() => {
    if (!pending) return;
    setEntered(false);
    const raf = requestAnimationFrame(() => setEntered(true));
    return () => cancelAnimationFrame(raf);
  }, [pending?.id]);

  if (!pending) return null;

  // Shell commands are never eligible for "allow for session": the blast
  // radius of unattended shell execution is too large to silently
  // pre-authorize off the back of approving one command. Every run_shell
  // call always prompts. See src-tauri/src/permissions.rs::NO_SESSION_REMEMBER
  // for the backend-enforced (authoritative) side of this restriction.
  const canRememberForSession = pending.tool !== "run_shell";
  const isMcpTool = pending.tool.startsWith("mcp:");
  const ToolIcon = isMcpTool ? Plug : TOOL_ICONS[pending.tool] ?? AlertTriangle;
  // `detail`'s first line is exactly "<server label> → <tool name>" (see
  // `mcp_call_tool`'s `detail` construction) — friendlier than the raw
  // `mcp:<serverId>:<toolName>` permission-request string.
  const displayTool = isMcpTool ? pending.detail.split("\n", 1)[0] : pending.tool;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4 backdrop-blur-[2px]"
      role="dialog"
      aria-modal="true"
      aria-labelledby="permission-modal-title"
    >
      <div
        className={`w-full max-w-md rounded-xl border border-border bg-background p-5 shadow-xl transition-all duration-200 ease-out ${
          entered ? "scale-100 opacity-100" : "scale-95 opacity-0"
        }`}
      >
        <div className="flex items-start gap-3">
          <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-warning-soft text-warning">
            <ToolIcon size={18} />
          </div>
          <div className="min-w-0 pt-0.5">
            <h2 id="permission-modal-title" className="text-sm font-semibold text-foreground">
              {t("PermissionModal.title")}
            </h2>
            <p className="mt-0.5 text-xs text-muted">
              {t("PermissionModal.wantsToRunTool")} <span className="font-mono text-foreground">{displayTool}</span>
            </p>
          </div>
        </div>

        <div className="mt-4">
          <div className="max-h-40 overflow-auto whitespace-pre-wrap break-all rounded-md border border-border bg-surface-2 p-2.5 font-mono text-xs text-muted">
            {pending.detail}
          </div>
          {!canRememberForSession && (
            <p className="mt-2 text-xs text-faint">
              {t("PermissionModal.shellAlwaysConfirmText")}
            </p>
          )}
        </div>

        <div className="mt-4 flex flex-col gap-2 sm:flex-row sm:justify-end">
          <Button type="button" variant="secondary" onClick={() => respond(false, false)}>
            {t("PermissionModal.denyButton")}
          </Button>
          <Button
            type="button"
            variant="primary"
            onClick={() => respond(true, false)}
            autoFocus={!canRememberForSession}
          >
            {t("PermissionModal.allowOnceButton")}
          </Button>
          {canRememberForSession && (
            <Button
              type="button"
              variant="secondary"
              className="border-warning/40 text-warning hover:bg-warning-soft"
              onClick={() => respond(true, true)}
              autoFocus
            >
              {t("PermissionModal.allowForSessionButton")}
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}

export default PermissionModal;
