import { useState } from "react";
import { importCodingSessionFiles } from "../../lib/codingSessionImport";
import { errorMessage } from "../../lib/errors";

/** Separate from Knowledge 2.0 ingestion: these become resumable chat sessions. */
export function CodingSessionImportCard() {
  const [busy,setBusy] = useState(false);
  const [success,setSuccess] = useState("");
  const [error,setError] = useState("");
  return <section className="rounded-xl border border-border bg-surface p-4">
    <h4 className="text-sm font-semibold text-foreground">Import coding-agent sessions</h4>
    <p className="mt-1 text-xs leading-relaxed text-muted">
      Migrate Claude Code, Codex CLI, OpenCode or Pi JSON/JSONL transcripts into
      resumable Chat sessions. Select your saved session files. Tool calls,
      tool outputs and private reasoning are excluded; original files are untouched.
      Choose a Little Monkey model and explicitly approve a workspace before continuing.
    </p>
    <button type="button" disabled={busy}
      className="mt-3 rounded-lg border border-border bg-surface-2 px-3 py-2 text-sm text-foreground disabled:opacity-50"
      onClick={async () => {
        setBusy(true); setError(""); setSuccess("");
        try {
          const count = await importCodingSessionFiles();
          setSuccess(count ? `Imported ${count} sessions. Open them in Chat.` : "No new sessions imported.");
        } catch (cause) {
          setError(errorMessage(cause));
        } finally { setBusy(false); }
      }}>
      {busy ? "Importing…" : "Select coding-session files"}
    </button>
    {success && <p role="status" className="mt-2 text-xs text-success">{success}</p>}
    {error && <p role="alert" className="mt-2 text-xs text-danger">{error}</p>}
  </section>;
}
