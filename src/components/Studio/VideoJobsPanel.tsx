import { useEffect, useState } from "react";
import { Button } from "../ui";
import { studioClient, type VideoJob } from "../../lib/studioClient";
import { useStudioRunStore } from "../../store/studioRunStore";

export function VideoJobsPanel() {
  const [jobs, setJobs] = useState<VideoJob[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  useEffect(() => {
    let mounted = true;
    let initial = true;
    let previous = new Set<string>();
    const refresh = async () => {
      try {
        const next = await studioClient.videoJobs();
        if (!mounted) return;
        const completed = new Set(next.filter((job) => job.status === "completed").map((job) => job.id));
        if (!initial && [...completed].some((id) => !previous.has(id))) {
          useStudioRunStore.setState((state) => ({ completions: state.completions + 1 }));
        }
        initial = false;
        previous = completed;
        setJobs(next);
      } catch (cause) {
        if (mounted) setError(String(cause));
      }
    };
    void refresh();
    const interval = setInterval(() => void refresh(), 2000);
    return () => { mounted = false; clearInterval(interval); };
  }, []);

  const act = async (job: VideoJob, action: "resume" | "cancel") => {
    setBusy(job.id);
    setError(null);
    try {
      if (action === "resume") await studioClient.videoResume(job.id);
      else await studioClient.videoCancel(job.id);
      setJobs(await studioClient.videoJobs());
    } catch (cause) {
      setError(String(cause));
    } finally {
      setBusy(null);
    }
  };

  if (jobs.length === 0 && !error) return null;
  return (
    <section className="mx-1 rounded-lg border border-border bg-surface p-3" aria-label="Durable video jobs">
      <h3 className="mb-1 text-xs font-semibold">Video jobs</h3>
      <p className="mb-2 text-[11px] text-muted">
        Jobs survive closing the Studio panel. Interrupted renders can be explicitly restarted with
        the saved seed; the native engines do not expose a mid-frame checkpoint.
      </p>
      {error && <p role="alert" className="mb-2 text-xs text-danger">{error}</p>}
      <div className="max-h-32 space-y-2 overflow-y-auto">
        {jobs.slice(0, 24).map((job) => (
          <div key={job.id} className="flex items-center gap-2 text-xs">
            <span className="min-w-0 flex-1 truncate" title={job.prompt}>{job.prompt || job.modelId}</span>
            <span className="shrink-0 font-mono text-[11px] text-muted">
              {job.status} · seed {job.seed}{job.status === "running" ? " · rendering" : ""}
            </span>
            {(job.status === "interrupted" || job.status === "failed") && (
              <Button size="sm" variant="secondary" disabled={busy === job.id}
                onClick={() => void act(job, "resume")}>Resume</Button>
            )}
            {(job.status === "queued" || job.status === "running") && (
              <Button size="sm" variant="secondary" disabled={busy === job.id}
                onClick={() => void act(job, "cancel")}>Cancel</Button>
            )}
          </div>
        ))}
      </div>
    </section>
  );
}
