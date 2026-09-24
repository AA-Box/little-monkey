import { useState } from "react";
import { Upload } from "lucide-react";
import { Button } from "../ui";
import { chooseAndParseChatExport, writeChatImport } from "../../lib/chatExportImport";
import { useKnowledgeV2Store } from "../../store/knowledgeV2Store";
import { errorMessage } from "../../lib/errors";

export function ChatExportImportCard({ stackId }: { stackId: string }) {
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const addSource = useKnowledgeV2Store((state) => state.addSource);
  const refreshStack = useKnowledgeV2Store((state) => state.refreshStack);
  const run = async () => {
    if (!stackId) return;
    setBusy(true); setError(null); setStatus(null);
    try {
      const selected = await chooseAndParseChatExport();
      if (!selected) return;
      const written = await writeChatImport(selected.sourceName, selected.conversations);
      await addSource(stackId, `${selected.sourceName} (${written.documentCount} chats)`, { kind: "local_folder", path: written.path });
      await refreshStack(stackId);
      setStatus(`Imported and indexed ${written.documentCount} conversations.`);
    } catch (cause) { setError(errorMessage(cause)); }
    finally { setBusy(false); }
  };
  return <div className="mt-3 rounded-md border border-border bg-surface p-3">
    <div className="flex items-start justify-between gap-3">
      <div className="flex min-w-0 gap-2"><Upload size={15} className="mt-0.5 shrink-0 text-accent"/><div>
        <p className="text-xs font-medium text-foreground">Import AI conversation history</p>
        <p className="mt-1 text-[11px] leading-4 text-muted">Import official ChatGPT, Claude, or Gemini JSON/ZIP exports. Conversations become ordinary local Knowledge 2.0 documents and use the existing hybrid index.</p>
      </div></div>
      <Button size="sm" variant="secondary" disabled={!stackId || busy} onClick={() => void run()}>{busy ? "Importing…" : "Import export"}</Button>
    </div>
    {status && <p className="mt-2 text-[11px] text-success">{status}</p>}
    {error && <p role="alert" className="mt-2 text-[11px] text-danger">{error}</p>}
  </div>;
}
