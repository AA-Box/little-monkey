import { useEffect, useState } from "react";
import { studioClient } from "../../lib/studioClient";

type Region = "eu" | "uk" | "us" | "kr" | "other";
const choices: { value: Region; label: string }[] = [
  { value: "eu", label: "European Union" },
  { value: "uk", label: "United Kingdom" },
  { value: "us", label: "United States" },
  { value: "kr", label: "South Korea" },
  { value: "other", label: "Outside all four excluded territories" },
];

/** Jurisdiction must be consciously declared; no IP, timezone or UI language
 * is silently translated into a legal conclusion. This is not a license grant.
 * Only the backend decides whether a local model may download or execute. */
export function StudioLicenseRegionControl({ onChanged }: { onChanged?: () => void }) {
  const [region, setRegion] = useState<Region | "">("");
  const [acknowledged, setAcknowledged] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState<Region | null>(null);

  useEffect(() => {
    let active = true;
    studioClient.licenseRegion().then((current) => {
      if (!active) return;
      setSaved(current);
      setRegion(current ?? "");
    }).catch((reason) => {
      if (active) setError(String(reason));
    });
    return () => { active = false; };
  }, []);

  async function save() {
    if (!region || !acknowledged) return;
    setBusy(true);
    setError(null);
    try {
      await studioClient.setLicenseRegion(region);
      setSaved(region);
      setAcknowledged(false);
      onChanged?.();
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="grid gap-2 rounded-md border border-border bg-background/60 p-3 text-xs">
      <div className="font-semibold text-foreground">Local model license jurisdiction</div>
      <p className="text-muted">
        Choose where you will use local model weights. Some weights (MiniMax H3 / VDN-H3)
        do not grant rights in the EU, UK, US or South Korea. This declaration is
        not a separate license and is never used to infer your location.
      </p>
      <label className="grid gap-1">
        <span className="text-muted">Territory of use</span>
        <select
          aria-label="Local model territory of use"
          className="rounded border border-border bg-surface px-2 py-1 text-foreground"
          value={region}
          onChange={(event) => {
            setRegion(event.target.value as Region | "");
            setAcknowledged(false);
          }}
          disabled={busy}
        >
          <option value="">Not declared</option>
          {choices.map((choice) => <option key={choice.value} value={choice.value}>{choice.label}</option>)}
        </select>
      </label>
      <label className="flex items-start gap-2 text-muted">
        <input type="checkbox" checked={acknowledged}
          onChange={(event) => setAcknowledged(event.target.checked)} disabled={busy || !region} />
        <span>I confirm this reflects the territory where I will actually use the weights.</span>
      </label>
      <div className="flex flex-wrap items-center gap-2">
        <button className="rounded border border-border px-2 py-1 text-foreground disabled:opacity-50"
          type="button" disabled={!region || !acknowledged || busy}
          onClick={() => void save()}>
          {busy ? "Saving…" : "Save model jurisdiction"}
        </button>
        {saved && <span className="text-muted">Saved: {choices.find((item) => item.value === saved)?.label}</span>}
      </div>
      <p className="text-muted">
        For a MiniMax H3 deployment inside excluded territories, contact MiniMax for
        separate written authorization. A self-declared exemption cannot unlock these weights.
        {" "}<a className="underline" target="_blank" rel="noreferrer"
          href="https://huggingface.co/OpenVDN/vdn-minimax-h3-edge/blob/main/LICENSE">
          Read original license
        </a>.
      </p>
      {error && <p role="alert" className="text-danger">{error}</p>}
    </div>
  );
}
