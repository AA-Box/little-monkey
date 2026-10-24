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
  const [grantJson, setGrantJson] = useState("");
  const [grantResult, setGrantResult] = useState<string | null>(null);

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

  async function importGrant() {
    if (!grantJson.trim()) return;
    setBusy(true);
    setGrantResult(null);
    setError(null);
    try {
      await studioClient.importMinimaxEntitlement(grantJson.trim());
      setGrantJson("");
      setGrantResult("Signed entitlement verified for the declared territory.");
      onChanged?.();
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(false);
    }
  }

  async function clearGrant() {
    setBusy(true);
    setError(null);
    try {
      await studioClient.clearMinimaxEntitlement();
      setGrantResult("Saved entitlement revoked for this profile.");
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
      <details className="rounded border border-border p-2">
        <summary className="cursor-pointer font-medium text-foreground">
          Verified enterprise MiniMax H3 license
        </summary>
        <p className="mt-2 text-muted">
          Only for organizations holding separate written MiniMax permission.
          The distributor must independently review that permission and issue
          an Ed25519-signed entitlement for this installation. Standard builds
          have no trusted issuer and cannot unlock excluded territories.
        </p>
        <textarea rows={3} aria-label="Signed MiniMax entitlement JSON"
          className="mt-2 w-full rounded border border-border bg-surface px-2 py-1 font-mono text-xs text-foreground"
          placeholder='{"payloadBase64":"…","signatureBase64":"…"}'
          value={grantJson} onChange={(event) => setGrantJson(event.target.value)}
          disabled={busy} />
        <div className="mt-2 flex flex-wrap gap-2">
          <button type="button" disabled={busy || !saved || !grantJson.trim()}
            className="rounded border border-border px-2 py-1 text-foreground disabled:opacity-50"
            onClick={() => void importGrant()}>
            Verify & import signed entitlement
          </button>
          <button type="button" disabled={busy}
            className="rounded border border-border px-2 py-1 text-foreground disabled:opacity-50"
            onClick={() => void clearGrant()}>
            Revoke imported entitlement
          </button>
        </div>
        {grantResult && <p role="status" className="mt-2 text-muted">{grantResult}</p>}
      </details>
      {error && <p role="alert" className="text-danger">{error}</p>}
    </div>
  );
}
