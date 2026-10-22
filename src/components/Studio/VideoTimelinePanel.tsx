import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { ArrowDown, ArrowUp, Plus, Trash2 } from "lucide-react";
import { Button, IconButton } from "../ui";
import { studioClient, type VideoJob, type VideoTimeline, type VideoTimelineSegment } from "../../lib/studioClient";
import { useStudioRunStore } from "../../store/studioRunStore";

type TimelineRow = VideoTimelineSegment & { key: string };
const uuid = () => crypto.randomUUID();

export function VideoTimelinePanel({ jobs }: { jobs: VideoJob[] }) {
  const [title, setTitle] = useState("My video");
  const [parts, setParts] = useState<TimelineRow[]>([]);
  const [music, setMusic] = useState<string | null>(null);
  const [timelines, setTimelines] = useState<VideoTimeline[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let mounted = true;
    let initialized = false;
    let known = new Set<string>();
    const refresh = async () => {
      try {
        const records = await studioClient.videoTimelines();
        if (!mounted) return;
        const done = new Set(records.filter((item) => item.status === "completed").map((item) => item.id));
        if (initialized && [...done].some((id) => !known.has(id))) {
          useStudioRunStore.setState((state) => ({ completions: state.completions + 1 }));
        }
        initialized = true;
        known = done;
        setTimelines(records);
      } catch (e) {
        if (mounted) setError(String(e));
      }
    };
    void refresh();
    const interval = setInterval(() => void refresh(), 2500);
    return () => { mounted = false; clearInterval(interval); };
  }, []);

  const candidates = jobs.filter((job) => job.status !== "cancelled" && job.status !== "failed");
  const add = (id: string) => {
    if (!id || parts.some((p) => p.jobId === id) || parts.length >= 16) return;
    setParts((prev) => [...prev, { key: uuid(), jobId: id, trimStartMs: 0, trimEndMs: null }]);
  };
  const move = (at: number, direction: number) => {
    setParts((prev) => {
      const dest = at + direction;
      if (dest < 0 || dest >= prev.length) return prev;
      const copy = [...prev];
      [copy[at], copy[dest]] = [copy[dest], copy[at]];
      return copy;
    });
  };
  const trim = (key: string, field: "trimStartMs" | "trimEndMs", value: string) => {
    const parsed = value.trim() === "" ? null : Number(value);
    setParts((rows) => rows.map((part) => part.key === key
      ? { ...part, [field]: field === "trimStartMs" ? (parsed ?? 0) : parsed }
      : part));
  };
  const chooseMusic = async () => {
    try {
      const result = await open({
        multiple: false,
        filters: [{ name: "Soundtrack", extensions: ["wav", "mp3", "m4a", "aac", "flac", "ogg"] }],
      });
      if (typeof result === "string") setMusic(result);
    } catch (e) { setError(String(e)); }
  };
  const submit = async () => {
    if (parts.length < 2) { setError("Add at least two clips."); return; }
    setError(null);
    setBusy(true);
    try {
      const segments = parts.map(({ jobId, trimStartMs, trimEndMs }) => ({
        jobId, trimStartMs: Math.max(0, Math.round(trimStartMs)),
        trimEndMs: trimEndMs === null ? null : Math.round(trimEndMs),
      }));
      await studioClient.videoTimelineSubmit(title, segments, music);
      setTimelines(await studioClient.videoTimelines());
    } catch (e) {
      setError(String(e));
    } finally { setBusy(false); }
  };
  const retry = async (id: string) => {
    setBusy(true);
    setError(null);
    try {
      await studioClient.videoTimelineRetry(id);
      setTimelines(await studioClient.videoTimelines());
    } catch (e) { setError(String(e)); }
    finally { setBusy(false); }
  };

  return (
    <section className="mt-3 rounded-lg border border-border bg-surface p-3" aria-label="Video timeline editor">
      <h3 className="text-xs font-semibold">Timeline and MP4 export</h3>
      <p className="mt-1 text-[11px] text-muted">
        Arrange clips, trim each one, and optionally add a soundtrack. Export waits for unfinished
        clips, then combines them into an MP4 in the Studio gallery. FFmpeg is required.
      </p>
      <label className="mt-3 grid gap-1 text-[11px] text-muted">
        Timeline name
        <input value={title} maxLength={200} onChange={(event) => setTitle(event.target.value)}
          className="h-8 rounded border border-border bg-background px-2 text-xs text-foreground" />
      </label>
      <div className="mt-2 flex items-center gap-2">
        <select value="" aria-label="Add video clip to timeline"
          onChange={(event) => add(event.target.value)}
          className="h-8 min-w-0 flex-1 rounded border border-border bg-background px-2 text-xs">
          <option value="">Choose a clip to add…</option>
          {candidates.filter((job) => !parts.some((p) => p.jobId === job.id)).map((job) =>
            <option key={job.id} value={job.id}>{job.prompt.slice(0, 65)} ({job.status})</option>
          )}
        </select>
        <span className="text-[11px] text-muted">{parts.length}/16</span>
      </div>
      <div className="mt-2 max-h-52 space-y-1.5 overflow-y-auto">
        {parts.map((part, index) => {
          const job = jobs.find((item) => item.id === part.jobId);
          return (
            <div key={part.key} className="flex flex-wrap items-center gap-1 rounded border border-border p-2 text-xs">
              <span className="min-w-20 flex-1 truncate" title={job?.prompt}>{index + 1}. {job?.prompt ?? part.jobId}</span>
              <label className="flex items-center gap-1 text-[10px] text-muted">
                Start ms
                <input type="number" min={0} max={600000} value={part.trimStartMs}
                  onChange={(e) => trim(part.key, "trimStartMs", e.target.value)}
                  className="w-20 rounded border border-border bg-background px-1 py-1 text-xs"/>
              </label>
              <label className="flex items-center gap-1 text-[10px] text-muted">
                End ms
                <input type="number" min={0} max={600000} placeholder="Full"
                  value={part.trimEndMs ?? ""}
                  onChange={(e) => trim(part.key, "trimEndMs", e.target.value)}
                  className="w-20 rounded border border-border bg-background px-1 py-1 text-xs"/>
              </label>
              <IconButton size="sm" aria-label="Move clip earlier" disabled={index === 0} onClick={() => move(index, -1)}>
                <ArrowUp size={12}/>
              </IconButton>
              <IconButton size="sm" aria-label="Move clip later" disabled={index === parts.length - 1} onClick={() => move(index, 1)}>
                <ArrowDown size={12}/>
              </IconButton>
              <IconButton size="sm" aria-label="Remove timeline clip"
                onClick={() => setParts((p) => p.filter((v) => v.key !== part.key))}>
                <Trash2 size={12}/>
              </IconButton>
            </div>
          );
        })}
      </div>
      <div className="mt-2 flex flex-wrap items-center gap-2">
        <Button size="sm" variant="secondary" onClick={() => void chooseMusic()}>
          <Plus size={12}/> {music ? "Change soundtrack" : "Add soundtrack"}
        </Button>
        {music && <>
          <span className="max-w-40 truncate text-[11px] text-muted" title={music}>{music.split(/[/\\]/).pop()}</span>
          <Button size="sm" variant="secondary" onClick={() => setMusic(null)}>Remove</Button>
        </>}
        <Button size="sm" disabled={parts.length < 2 || busy} onClick={() => void submit()}>
          {busy ? "Working…" : "Export MP4"}
        </Button>
      </div>
      {error && <p role="alert" className="mt-2 text-[11px] text-danger">{error}</p>}
      {timelines.length > 0 && <div className="mt-3 space-y-1 border-t border-border pt-2">
        <p className="text-[11px] font-medium">Timeline exports</p>
        {timelines.slice(0, 12).map((timeline) =>
          <div key={timeline.id} className="flex flex-wrap items-center gap-2 text-[11px]">
            <span className="min-w-0 flex-1 truncate">{timeline.title}</span>
            <span className="text-muted">{timeline.status}</span>
            {timeline.outputEntryId && <span className="text-success">Saved in gallery</span>}
            {timeline.error && <span className="max-w-48 truncate text-danger" title={timeline.error}>{timeline.error}</span>}
            {(timeline.status === "failed" || timeline.status === "interrupted") &&
              <Button size="sm" variant="secondary" disabled={busy} onClick={() => void retry(timeline.id)}>
                Retry
              </Button>
            }
          </div>
        )}
      </div>}
    </section>
  );
}
