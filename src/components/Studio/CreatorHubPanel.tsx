import { useEffect, useMemo, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { readFile, readTextFile } from "@tauri-apps/plugin-fs";
import { Download, Search, Sparkles } from "lucide-react";
import { Button, StatusPill } from "../ui";
import { errorMessage } from "../../lib/errors";
import { studioClient, type GenerationEntry } from "../../lib/studioClient";
import {
  CURATED_STUDIO_PROFILES,
  WORKFLOW_PRESETS,
  hardwareFit,
  modelSpecForDownload,
  studioParityClient,
  type CharacterTrainerCapabilities,
  type DiscoveryAssetKind,
  type DiscoveryItem,
  type DiscoverySource,
  type PortableTrainingStatus,
  type WorkflowKind,
  type WorkflowUpload,
} from "../../lib/studioParity";

const formatBytes = (value: number) => value ? `${(value / 1024 ** 3).toFixed(value >= 1024 ** 3 ? 1 : 3)} GB` : "size unknown";
const toBase64 = (bytes: Uint8Array) => { let binary = ""; const step = 0x8000; for (let i = 0; i < bytes.length; i += step) binary += String.fromCharCode(...bytes.subarray(i, i + step)); return btoa(binary); };

export function CreatorHubPanel() {
  const [source, setSource] = useState<DiscoverySource>("hugging_face");
  const [kind, setKind] = useState<DiscoveryAssetKind>("model");
  const [query, setQuery] = useState("Qwen Image 2.1 GGUF");
  const [results, setResults] = useState<DiscoveryItem[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [totalRam, setTotalRam] = useState(0);

  const [characterName, setCharacterName] = useState("My character");
  const [triggerWord, setTriggerWord] = useState("mychar");
  const [photos, setPhotos] = useState("");
  const [baseModel, setBaseModel] = useState("");
  const [ditPath, setDitPath] = useState("");
  const [vaePath, setVaePath] = useState("");
  const [textEncoderPath, setTextEncoderPath] = useState("");
  const [trainingSteps, setTrainingSteps] = useState(600);
  const [trainingResolution, setTrainingResolution] = useState(768);
  const [trainingStatus, setTrainingStatus] = useState<string | null>(null);
  const [trainerCapabilities, setTrainerCapabilities] = useState<CharacterTrainerCapabilities | null>(null);
  const [portableTraining, setPortableTraining] = useState<PortableTrainingStatus | null>(null);
  const registeredPortableLora = useRef<string | null>(null);

  const [workflowKind, setWorkflowKind] = useState<WorkflowKind>("music");
  const [workflowUrl, setWorkflowUrl] = useState(() => localStorage.getItem("lm-comfy-workflow-url") ?? "http://127.0.0.1:8188/");
  const [workflow, setWorkflow] = useState<unknown | null>(null);
  const [workflowPrompt, setWorkflowPrompt] = useState("");
  const [workflowScript, setWorkflowScript] = useState("");
  const [inputImage, setInputImage] = useState<WorkflowUpload | null>(null);
  const [inputVideo, setInputVideo] = useState<WorkflowUpload | null>(null);
  const [workflowResults, setWorkflowResults] = useState<GenerationEntry[]>([]);

  useEffect(() => {
    void studioClient.engineStatus().then((status) => setTotalRam(status.totalRamBytes)).catch(() => undefined);
    void studioParityClient.characterCapabilities().then(setTrainerCapabilities).catch(() => undefined);
    void studioParityClient.portableCharacterStatus().then(setPortableTraining).catch(() => undefined);
  }, []);

  useEffect(() => {
    const active = portableTraining?.status === "running" || portableTraining?.status === "setting_up";
    if (!active) return;
    const timer = window.setInterval(() => {
      void studioParityClient.portableCharacterStatus().then(setPortableTraining).catch(() => undefined);
    }, 1000);
    return () => window.clearInterval(timer);
  }, [portableTraining?.status]);

  useEffect(() => {
    if (portableTraining?.status !== "complete" || !portableTraining.loraPath) return;
    if (registeredPortableLora.current === portableTraining.loraPath) return;
    registeredPortableLora.current = portableTraining.loraPath;
    void studioClient.addLora({
      name: portableTraining.loraName || characterName,
      path: portableTraining.loraPath,
    }).then(() => {
      setTrainingStatus("Character LoRA finished and was added to the Studio LoRA library.");
    }).catch((cause) => {
      registeredPortableLora.current = null;
      setError(errorMessage(cause));
    });
  }, [portableTraining, characterName]);

  const preset = WORKFLOW_PRESETS[workflowKind];
  const portableTrainer = trainerCapabilities?.backend === "musubi_zimage";
  const portableBusy = portableTraining?.status === "running" || portableTraining?.status === "setting_up";

  const doSearch = async () => { setBusy("search"); setError(null); try { setResults(await studioParityClient.search(source, query, kind)); } catch (cause) { setError(errorMessage(cause)); } finally { setBusy(null); } };
  const install = async (item: DiscoveryItem) => { if (!item.sha256) return; setBusy(item.id); setError(null); try { const file = await studioParityClient.download(item); if (item.assetKind === "lora") await studioClient.addLora({ name: item.name, path: file.path }); else await studioClient.addModel(modelSpecForDownload(item, file.path, file.sizeBytes)); } catch (cause) { setError(errorMessage(cause)); } finally { setBusy(null); } };
  const pickDir = async (setter: (value: string) => void) => { const picked = await open({ directory: true, multiple: false }); if (typeof picked === "string") setter(picked); };
  const pickSafetensors = async (setter: (value: string) => void) => { const picked = await open({ directory: false, multiple: false, filters: [{ name: "Safetensors model", extensions: ["safetensors"] }] }); if (typeof picked === "string") setter(picked); };

  const trainMflux = async () => {
    setBusy("train"); setError(null); setTrainingStatus(null);
    try {
      const result = await studioParityClient.trainCharacter({ name: characterName, sourceDir: photos, baseModelPath: baseModel, triggerWord, epochs: 24, rank: 16, maxResolution: 768 });
      await studioClient.addLora({ name: characterName, path: result.loraPath });
      setTrainingStatus(`Trained from ${result.imageCount} photos and added to the LoRA library.`);
    } catch (cause) { setError(errorMessage(cause)); }
    finally { setBusy(null); }
  };

  const setupPortableTrainer = async () => {
    setBusy("trainer-setup"); setError(null); setTrainingStatus(null);
    setPortableTraining({ status: "setting_up", phase: "Preparing trainer setup", logTail: "", loraPath: null, loraName: null, currentPid: null });
    try {
      const capabilities = await studioParityClient.setupPortableCharacterTrainer();
      setTrainerCapabilities(capabilities);
      setPortableTraining(await studioParityClient.portableCharacterStatus());
      setTrainingStatus("Portable Character Studio trainer is ready.");
    } catch (cause) {
      setError(errorMessage(cause));
      setPortableTraining(await studioParityClient.portableCharacterStatus().catch(() => null));
    } finally { setBusy(null); }
  };

  const startPortableTraining = async () => {
    setBusy("train"); setError(null); setTrainingStatus(null); registeredPortableLora.current = null;
    try {
      const status = await studioParityClient.startPortableCharacterTraining({
        name: characterName,
        sourceDir: photos,
        ditPath,
        vaePath,
        textEncoderPath,
        triggerWord,
        steps: trainingSteps,
        resolution: trainingResolution,
      });
      setPortableTraining(status);
    } catch (cause) { setError(errorMessage(cause)); }
    finally { setBusy(null); }
  };

  const cancelPortableTraining = async () => {
    setError(null);
    try {
      await studioParityClient.cancelPortableCharacterTraining();
      setPortableTraining(await studioParityClient.portableCharacterStatus());
    } catch (cause) { setError(errorMessage(cause)); }
  };

  const pickWorkflow = async () => { const picked = await open({ directory: false, multiple: false, filters: [{ name: "ComfyUI API workflow", extensions: ["json"] }] }); if (typeof picked === "string") setWorkflow(JSON.parse(await readTextFile(picked))); };
  const pickMedia = async (type: "image" | "video") => { const picked = await open({ directory: false, multiple: false }); if (typeof picked !== "string") return; const upload = { name: picked.split(/[/\\]/).pop() ?? `${type}.bin`, dataBase64: toBase64(await readFile(picked)) }; if (type === "image") setInputImage(upload); else setInputVideo(upload); };
  const runWorkflow = async () => { if (!workflow) return; setBusy("workflow"); setError(null); try { localStorage.setItem("lm-comfy-workflow-url", workflowUrl); const values: Record<string, unknown> = { prompt: workflowPrompt, negative_prompt: "", script: workflowScript, duration_seconds: 10 }; setWorkflowResults(await studioParityClient.runWorkflow({ kind: workflowKind, baseUrl: workflowUrl, workflow, values, inputImage, inputVideo })); } catch (cause) { setError(errorMessage(cause)); } finally { setBusy(null); } };
  const sortedResults = useMemo(() => [...results].sort((a, b) => Number(Boolean(b.sha256)) - Number(Boolean(a.sha256))), [results]);

  return <div className="grid gap-4 border-t border-border pt-4">
    <section className="rounded-lg border border-border bg-surface p-3">
      <div className="flex items-center gap-2"><Sparkles size={15}/><h2 className="text-xs font-medium">Curated Studio catalog</h2></div>
      <p className="mt-1 text-[11px] text-muted">Curated families route into live Hugging Face/CivitAI results instead of freezing stale file URLs. Fit is based on this machine's measured RAM.</p>
      <div className="mt-3 grid gap-2 sm:grid-cols-2">{CURATED_STUDIO_PROFILES.map((profile) => { const fit = hardwareFit(totalRam, profile.minRamBytes); return <button key={profile.id} type="button" className="rounded border border-border p-2 text-left hover:border-accent" onClick={() => { setSource(profile.source); setKind(profile.kind); setQuery(profile.query); }}><span className="flex justify-between gap-2"><span className="text-xs font-medium">{profile.name}</span><StatusPill tone={fit === "fits" ? "success" : fit === "too_large" ? "danger" : "warning"}>{fit.replace("_", " ")}</StatusPill></span><span className="mt-1 block text-[11px] text-muted">{profile.note}</span></button>; })}</div>
    </section>

    <section className="rounded-lg border border-border bg-surface p-3">
      <h2 className="text-xs font-medium">Community model & LoRA browser</h2>
      <div className="mt-2 flex flex-wrap gap-2"><select value={source} onChange={(e) => setSource(e.target.value as DiscoverySource)} className="h-8 rounded border border-border bg-background px-2 text-xs"><option value="hugging_face">Hugging Face</option><option value="civitai">CivitAI</option></select><select value={kind} onChange={(e) => setKind(e.target.value as DiscoveryAssetKind)} className="h-8 rounded border border-border bg-background px-2 text-xs"><option value="model">Models</option><option value="lora">LoRAs</option></select><input value={query} onChange={(e) => setQuery(e.target.value)} className="h-8 min-w-48 flex-1 rounded border border-border bg-background px-2 text-xs"/><Button size="sm" onClick={() => void doSearch()} disabled={busy !== null}><Search size={13}/> Search</Button></div>
      <div className="mt-3 grid gap-2">{sortedResults.map((item) => <div key={item.id} className="flex items-center justify-between gap-3 rounded border border-border p-2"><div className="min-w-0"><p className="truncate text-xs font-medium">{item.name}</p><p className="truncate text-[11px] text-muted">{item.fileName} · {formatBytes(item.sizeBytes)} · {item.family}</p>{!item.sha256 && <p className="text-[10px] text-warning">No published SHA-256: browse only; one-click install is blocked.</p>}</div><Button size="sm" variant="secondary" disabled={!item.sha256 || busy !== null} onClick={() => void install(item)}><Download size={13}/>{busy === item.id ? "Installing…" : "Install"}</Button></div>)}</div>
    </section>

    <section className="rounded-lg border border-border bg-surface p-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h2 className="text-xs font-medium">Character Studio</h2>
        {trainerCapabilities && <StatusPill tone={trainerCapabilities.supported ? "success" : "warning"}>{trainerCapabilities.backend === "mflux" ? "MFLUX · Apple Silicon" : "Musubi · Z-Image"}</StatusPill>}
      </div>

      {!trainerCapabilities && <p className="mt-1 text-[11px] text-muted">Checking the local training backend…</p>}
      {trainerCapabilities?.reason && <p className="mt-2 rounded border border-warning/30 bg-warning/5 p-2 text-[11px] text-warning">{trainerCapabilities.reason}</p>}

      {trainerCapabilities?.backend === "mflux" && <>
        <p className="mt-1 text-[11px] text-muted">Apple Silicon uses the verified MFLUX Image Runtime and a local Krea-2-Raw base model. Training is local and runs with Hugging Face network access disabled.</p>
        <div className="mt-2 grid gap-2 sm:grid-cols-2"><input value={characterName} onChange={(e) => setCharacterName(e.target.value)} className="h-8 rounded border border-border bg-background px-2 text-xs" placeholder="Character name"/><input value={triggerWord} onChange={(e) => setTriggerWord(e.target.value)} className="h-8 rounded border border-border bg-background px-2 text-xs" placeholder="Trigger word"/><Button size="sm" variant="secondary" onClick={() => void pickDir(setPhotos)}>{photos ? "Photos selected" : "Choose photos folder"}</Button><Button size="sm" variant="secondary" onClick={() => void pickDir(setBaseModel)}>{baseModel ? "Base model selected" : "Choose Krea-2-Raw folder"}</Button></div>
        <Button className="mt-2" size="sm" disabled={!photos || !baseModel || busy !== null} onClick={() => void trainMflux()}>{busy === "train" ? "Training…" : "Train character LoRA"}</Button>
      </>}

      {portableTrainer && trainerCapabilities && <>
        <p className="mt-1 text-[11px] text-muted">Windows/Linux training uses pinned {trainerCapabilities.sourceVersion} in a profile-scoped venv. {trainerCapabilities.gpuName ? `${trainerCapabilities.gpuName}${trainerCapabilities.vramMib ? ` · ${(trainerCapabilities.vramMib / 1024).toFixed(1)} GiB VRAM` : ""}${trainerCapabilities.computeCapability ? ` · compute ${trainerCapabilities.computeCapability}` : ""}.` : ""}</p>

        {trainerCapabilities.supported && !trainerCapabilities.setupReady && <div className="mt-2 rounded border border-border p-2"><p className="text-[11px] text-muted">The trainer source and CUDA/Python environment are not installed yet. Setup verifies the pinned Musubi commit and keeps the environment inside this Little Monkey profile.</p><div className="mt-2 flex gap-2"><Button size="sm" disabled={busy !== null || portableBusy} onClick={() => void setupPortableTrainer()}>{busy === "trainer-setup" || portableTraining?.status === "setting_up" ? "Setting up…" : "Set up trainer"}</Button>{portableTraining?.status === "setting_up" && <Button size="sm" variant="secondary" onClick={() => void cancelPortableTraining()}>Cancel</Button>}</div></div>}

        {trainerCapabilities.supported && trainerCapabilities.setupReady && <>
          <div className="mt-2 grid gap-2 sm:grid-cols-2"><input value={characterName} onChange={(e) => setCharacterName(e.target.value)} className="h-8 rounded border border-border bg-background px-2 text-xs" placeholder="Character name"/><input value={triggerWord} onChange={(e) => setTriggerWord(e.target.value)} className="h-8 rounded border border-border bg-background px-2 text-xs" placeholder="Trigger word"/><Button size="sm" variant="secondary" disabled={portableBusy} onClick={() => void pickDir(setPhotos)}>{photos ? "Photos selected" : "Choose photos folder"}</Button><Button size="sm" variant="secondary" disabled={portableBusy} onClick={() => void pickSafetensors(setDitPath)}>{ditPath ? "Z-Image DiT selected" : "Choose Z-Image DiT"}</Button><Button size="sm" variant="secondary" disabled={portableBusy} onClick={() => void pickSafetensors(setVaePath)}>{vaePath ? "VAE selected" : "Choose Z-Image VAE"}</Button><Button size="sm" variant="secondary" disabled={portableBusy} onClick={() => void pickSafetensors(setTextEncoderPath)}>{textEncoderPath ? "Text encoder selected" : "Choose Qwen3 text encoder"}</Button><label className="grid gap-1 text-[11px] text-muted">Training steps<input type="number" min={50} max={10000} value={trainingSteps} disabled={portableBusy} onChange={(e) => setTrainingSteps(Number(e.target.value))} className="h-8 rounded border border-border bg-background px-2 text-xs text-foreground"/></label><label className="grid gap-1 text-[11px] text-muted">Resolution<select value={trainingResolution} disabled={portableBusy} onChange={(e) => setTrainingResolution(Number(e.target.value))} className="h-8 rounded border border-border bg-background px-2 text-xs text-foreground"><option value={512}>512</option><option value={640}>640</option><option value={768}>768</option><option value={896}>896</option><option value={1024}>1024</option></select></label></div>
          <div className="mt-2 flex gap-2"><Button size="sm" disabled={!photos || !ditPath || !vaePath || !textEncoderPath || portableBusy || busy !== null} onClick={() => void startPortableTraining()}>{portableTraining?.status === "running" ? "Training…" : "Train character LoRA"}</Button>{portableBusy && <Button size="sm" variant="secondary" onClick={() => void cancelPortableTraining()}>Cancel</Button>}</div>
        </>}

        {portableTraining && portableTraining.status !== "idle" && <div className="mt-2 rounded border border-border bg-background p-2"><div className="flex items-center justify-between gap-2"><p className="text-[11px] font-medium">{portableTraining.phase || portableTraining.status}</p><StatusPill tone={portableTraining.status === "complete" ? "success" : portableTraining.status === "error" ? "danger" : portableTraining.status === "cancelled" ? "warning" : "neutral"}>{portableTraining.status.replace("_", " ")}</StatusPill></div>{portableTraining.logTail && <pre className="mt-2 max-h-40 overflow-auto whitespace-pre-wrap break-words text-[10px] leading-4 text-muted">{portableTraining.logTail}</pre>}</div>}
      </>}

      {trainingStatus && <p className="mt-2 text-[11px] text-success">{trainingStatus}</p>}
    </section>

    <section className="rounded-lg border border-border bg-surface p-3">
      <h2 className="text-xs font-medium">Media workflow presets</h2><p className="mt-1 text-[11px] text-muted">Music, Talking Character, Motion Control and Extend Video run against your own ComfyUI. Export the workflow in API format once; Little Monkey uploads media, binds typed placeholders, retrieves image/video/audio outputs, and files them in the Studio gallery.</p>
      <div className="mt-2 flex flex-wrap gap-2">{(Object.keys(WORKFLOW_PRESETS) as WorkflowKind[]).map((value) => <Button key={value} size="sm" variant={workflowKind === value ? "primary" : "secondary"} onClick={() => setWorkflowKind(value)}>{WORKFLOW_PRESETS[value].label}</Button>)}</div><p className="mt-2 text-[11px] text-muted">{preset.description} Placeholders: {preset.placeholders.map((p) => `{{${p}}}`).join(", ")}</p>
      <div className="mt-2 grid gap-2"><input value={workflowUrl} onChange={(e) => setWorkflowUrl(e.target.value)} className="h-8 rounded border border-border bg-background px-2 font-mono text-xs"/><textarea value={workflowPrompt} onChange={(e) => setWorkflowPrompt(e.target.value)} className="min-h-16 rounded border border-border bg-background p-2 text-xs" placeholder="Prompt"/>{workflowKind === "talking_character" && <textarea value={workflowScript} onChange={(e) => setWorkflowScript(e.target.value)} className="min-h-16 rounded border border-border bg-background p-2 text-xs" placeholder="Dialogue / script"/>}<div className="flex flex-wrap gap-2"><Button size="sm" variant="secondary" onClick={() => void pickWorkflow()}>{workflow ? "Workflow loaded" : "Load API workflow JSON"}</Button>{(workflowKind === "talking_character" || workflowKind === "motion_control") && <Button size="sm" variant="secondary" onClick={() => void pickMedia("image")}>{inputImage ? "Image loaded" : "Choose input image"}</Button>}{workflowKind === "extend_video" && <Button size="sm" variant="secondary" onClick={() => void pickMedia("video")}>{inputVideo ? "Video loaded" : "Choose input video"}</Button>}<Button size="sm" disabled={!workflow || busy !== null} onClick={() => void runWorkflow()}>{busy === "workflow" ? "Running…" : "Run workflow"}</Button></div></div>{workflowResults.length > 0 && <p className="mt-2 text-[11px] text-success">Completed with {workflowResults.length} artifact{workflowResults.length === 1 ? "" : "s"}; results are in the Studio gallery.</p>}
    </section>
    {error && <p role="alert" className="rounded border border-danger/30 bg-danger/5 p-2 text-xs text-danger">{error}</p>}
  </div>;
}
