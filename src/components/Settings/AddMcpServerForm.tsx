import { useCallback, useState } from "react";
import { Plus, Trash2 } from "lucide-react";
import { Button } from "../ui";
import { useMcpStore, type McpTransport } from "../../store/mcpStore";
import { useT } from "../../lib/i18n";

/** One draft `env` row in the add-server form. `id` is a local React key
 * only — never sent to the backend (the map is rebuilt from `key`/`value`
 * on submit). */
interface EnvRow {
  id: number;
  key: string;
  value: string;
}

let nextRowId = 0;

/** Parses the timeout input into `McpServerEntry.timeout_secs` — `null`
 * (fall back to `mcp.rs`'s 60s default) for empty/invalid/non-positive
 * input, otherwise a rounded positive integer. */
function parseTimeoutSecs(raw: string): number | null {
  const trimmed = raw.trim();
  if (trimmed.length === 0) return null;
  const parsed = Number(trimmed);
  return Number.isFinite(parsed) && parsed > 0 ? Math.round(parsed) : null;
}

/**
 * Mini-form for registering a new MCP server — mirrors
 * `AddCustomProviderForm.tsx`'s shape (label + fields + inline error), with
 * a transport radio: `stdio` (command + args + env, as before) or `http`
 * (URL + an optional bearer token that goes straight to the keychain via
 * `mcp_set_http_token` — never included in the `addServer` entry, so it
 * never touches `mcp_servers.json`). Both transports share a per-server
 * timeout field wired to `McpServerEntry.timeout_secs`.
 */
export function AddMcpServerForm() {
  const addServer = useMcpStore((s) => s.addServer);
  const connect = useMcpStore((s) => s.connect);
  const setHttpToken = useMcpStore((s) => s.setHttpToken);
  const { t } = useT();

  const [transportKind, setTransportKind] = useState<"stdio" | "http">("stdio");
  const [label, setLabel] = useState("");
  const [command, setCommand] = useState("");
  const [argsText, setArgsText] = useState("");
  const [envRows, setEnvRows] = useState<EnvRow[]>([]);
  const [url, setUrl] = useState("");
  const [token, setToken] = useState("");
  const [timeoutText, setTimeoutText] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const canSubmit =
    label.trim().length > 0 &&
    (transportKind === "stdio" ? command.trim().length > 0 : url.trim().length > 0) &&
    !submitting;

  const addEnvRow = useCallback(() => {
    setEnvRows((rows) => [...rows, { id: nextRowId++, key: "", value: "" }]);
  }, []);

  const updateEnvRow = useCallback((id: number, patch: Partial<EnvRow>) => {
    setEnvRows((rows) => rows.map((row) => (row.id === id ? { ...row, ...patch } : row)));
  }, []);

  const removeEnvRow = useCallback((id: number) => {
    setEnvRows((rows) => rows.filter((row) => row.id !== id));
  }, []);

  const handleAdd = useCallback(async () => {
    if (!canSubmit) return;
    setSubmitting(true);
    setError(null);

    // Slugify the label into an id, the same shape `checkpoints::validate_id`
    // requires on the Rust side (`^[a-zA-Z0-9_-]+$`) — see
    // `mcp.rs::validate_id`. Falls back to a counter suffix on collision-free
    // best effort; a genuine duplicate is still caught (and surfaced) by
    // `mcp_add_server` itself.
    const id = label.trim().toLowerCase().replace(/[^a-z0-9_-]+/g, "-").replace(/^-+|-+$/g, "") || "server";

    const transport: McpTransport =
      transportKind === "stdio"
        ? {
            type: "stdio",
            command: command.trim(),
            args: argsText
              .split("\n")
              .map((line) => line.trim())
              .filter((line) => line.length > 0),
            env: envRows.reduce<Record<string, string>>((acc, row) => {
              const key = row.key.trim();
              if (key.length > 0) acc[key] = row.value;
              return acc;
            }, {}),
          }
        : { type: "http", url: url.trim() };

    try {
      await addServer({
        id,
        label: label.trim(),
        transport,
        enabled: true,
        tool_allowlist: null,
        timeout_secs: parseTimeoutSecs(timeoutText),
      });

      // The bearer token never goes through `addServer` (it must never land
      // in `mcp_servers.json`) — saved separately, straight to the keychain,
      // right after the server config itself exists.
      if (transportKind === "http" && token.trim().length > 0) {
        try {
          await setHttpToken(id, token.trim());
        } catch (err) {
          setError(err instanceof Error ? err.message : String(err));
        }
      }

      // Connect right away — an added-but-never-connected server would just
      // sit there with an empty tool list until the user notices and hits
      // Reconnect, so do the obvious thing immediately instead.
      try {
        await connect(id);
      } catch {
        // Connection failures surface via the server row's own status pill
        // (`mcp://status` -> "error") — nothing further to do here.
      }
      setLabel("");
      setCommand("");
      setArgsText("");
      setEnvRows([]);
      setUrl("");
      setToken("");
      setTimeoutText("");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSubmitting(false);
    }
  }, [canSubmit, label, transportKind, command, argsText, envRows, url, token, timeoutText, addServer, setHttpToken, connect]);

  return (
    <div className="flex flex-col gap-2 rounded-lg border border-dashed border-border p-3">
      <p className="text-xs font-semibold uppercase tracking-wider text-faint">{t("AddMcpServerForm.heading")}</p>

      <div className="flex items-center gap-3 text-xs text-muted">
        <label className="flex cursor-pointer items-center gap-1.5">
          <input
            type="radio"
            name="mcp-transport-kind"
            checked={transportKind === "stdio"}
            onChange={() => setTransportKind("stdio")}
            className="accent-accent"
          />
          {t("AddMcpServerForm.transportStdioLabel")}
        </label>
        <label className="flex cursor-pointer items-center gap-1.5">
          <input
            type="radio"
            name="mcp-transport-kind"
            checked={transportKind === "http"}
            onChange={() => setTransportKind("http")}
            className="accent-accent"
          />
          {t("AddMcpServerForm.transportHttpLabel")}
        </label>
      </div>

      <div className="flex flex-col gap-2 sm:flex-row">
        <input
          type="text"
          value={label}
          onChange={(event) => setLabel(event.target.value)}
          placeholder={t("AddMcpServerForm.labelPlaceholder")}
          className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
        />
        {transportKind === "stdio" ? (
          <input
            type="text"
            value={command}
            onChange={(event) => setCommand(event.target.value)}
            placeholder={t("AddMcpServerForm.commandPlaceholder")}
            className="h-8 min-w-0 flex-[1.5] rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />
        ) : (
          <input
            type="text"
            value={url}
            onChange={(event) => setUrl(event.target.value)}
            placeholder={t("AddMcpServerForm.urlPlaceholder")}
            className="h-8 min-w-0 flex-[1.5] rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />
        )}
      </div>

      {transportKind === "stdio" ? (
        <>
          <div className="flex flex-col gap-1">
            <span className="text-xs text-muted">{t("AddMcpServerForm.argsLabel")}</span>
            <textarea
              value={argsText}
              onChange={(event) => setArgsText(event.target.value)}
              placeholder={t("AddMcpServerForm.argsPlaceholder")}
              rows={2}
              spellCheck={false}
              className="w-full resize-y rounded-md border border-border bg-surface px-2.5 py-1.5 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
          </div>

          <div className="flex flex-col gap-1.5">
            <span className="text-xs text-muted">{t("AddMcpServerForm.envLabel")}</span>
            {envRows.map((row) => (
              <div key={row.id} className="flex items-center gap-1.5">
                <input
                  type="text"
                  value={row.key}
                  onChange={(event) => updateEnvRow(row.id, { key: event.target.value })}
                  placeholder={t("AddMcpServerForm.envKeyPlaceholder")}
                  className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                />
                <input
                  type="text"
                  value={row.value}
                  onChange={(event) => updateEnvRow(row.id, { value: event.target.value })}
                  placeholder={t("AddMcpServerForm.envValuePlaceholder")}
                  className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                />
                <button
                  type="button"
                  onClick={() => removeEnvRow(row.id)}
                  aria-label={t("AddMcpServerForm.removeEnvRowAriaLabel")}
                  className="shrink-0 cursor-pointer rounded-md p-1.5 text-faint hover:text-danger"
                >
                  <Trash2 size={13} />
                </button>
              </div>
            ))}
            <Button variant="ghost" size="sm" onClick={addEnvRow} className="self-start">
              <Plus size={12} />
              {t("AddMcpServerForm.addEnvRowButton")}
            </Button>
          </div>
        </>
      ) : (
        <div className="flex flex-col gap-1">
          <span className="text-xs text-muted">{t("AddMcpServerForm.tokenLabel")}</span>
          <input
            type="password"
            value={token}
            onChange={(event) => setToken(event.target.value)}
            placeholder={t("AddMcpServerForm.tokenPlaceholder")}
            autoComplete="off"
            className="h-8 min-w-0 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />
          <span className="text-xs text-faint">{t("AddMcpServerForm.tokenHelpText")}</span>
        </div>
      )}

      <div className="flex flex-col gap-1">
        <span className="text-xs text-muted">{t("AddMcpServerForm.timeoutLabel")}</span>
        <input
          type="number"
          min={1}
          value={timeoutText}
          onChange={(event) => setTimeoutText(event.target.value)}
          placeholder={t("AddMcpServerForm.timeoutPlaceholder")}
          className="h-8 w-24 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
        />
      </div>

      <div className="flex items-center justify-between gap-2">
        <p className="text-xs text-faint">{t("AddMcpServerForm.helpText")}</p>
        <Button variant="secondary" size="sm" onClick={() => void handleAdd()} disabled={!canSubmit} className="shrink-0">
          {submitting ? t("AddMcpServerForm.addingButton") : t("AddMcpServerForm.addButton")}
        </Button>
      </div>
      {error && <p className="text-xs text-danger">{error}</p>}
    </div>
  );
}
