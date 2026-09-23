import { useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { readFile, readTextFile } from "@tauri-apps/plugin-fs";
import { Download, Search, Sparkles } from "lucide-react";
import { Button, StatusPill } from "../ui";
import { errorMessage } from "../../lib/errors";
import { studioClient, type GenerationEntry } from "../../lib/studioClient";
import { CURATED_STUDIO_PROFILES, WORKFLOW_PRESETS, hardwareFit, modelSpecForDownload, studioParityClient, type DiscoveryAssetKind, type DiscoveryItem, type DiscoverySource, type WorkflowKind, type WorkflowUpload } from "../../lib/studioParity";

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
  const [trainingStatus, setTrainingStatus] = useState<string | null>(null);
  const [workflowKind, setWorkflowKind] = useState<WorkflowKind>("music");
  const [workflowUrl, setWorkflowUrl] = useState(() => localStorage.getItem("lm-comfy-workflow-url") ?? "http://127.0.0.1:8188/");
  const [workflow, setWorkflow] = useState<unknown | null>(null);
  const [workflowPrompt, setWorkflowPrompt] = useState("");
  const [workflowScript, setWorkflowScript] = useState("");
  const [inputImage, setInputImage] = useState<WorkflowUpload | null>(null);
  const [inputVideo, setInputVideo] = useState<WorkflowUpload | null>(null);
  const [workflowResults, setWorkflowResults] = useState<GenerationEntry[]>([]);

  useEffect(() => { void studioClient.engineStatus().then((status) => setTotalRam(status.totalRamBytes)).catch(() => undefined); }, []);
  const preset = WORKFLOW_PRESETS[workflowKind];
  const doSearch = async () => { setBusy("search"); setError(null); try { setResults(await studioParityClient.search(source, query, kind)); } catch (cause) { setError(errorMessage(cause)); } finally { setBusy(null); } };
  const install = async (item: DiscoveryItem) => { if (!item.sha256) return; setBusy(item.id); setError(null); try { const file = await studioParityClient.download(item); if (item.assetKind === "lora") await studioClient.addLora({ name: item.name, path: file.path }); else await studioClient.addModel(modelSpecForDownload(item, file.path, file.sizeBytes)); } catch (cause) { setError(errorMessage(cause)); } finally { setBusy(null); } };
  const pickDir = async (setter: (value: string) => void) => { const picked = await open({ directory: true, multiple: false }); if (typeof picked === "string") setter(picked); };
  const train = async () => { setBusy("train"); setError(null); setTrainingStatus(null); try { const result = await studioParityClient.trainCharacter({ name: characterName, sourceDir: photos, baseModelPath: baseModel, triggerWord, epochs: 24, rank: 16, maxResolution: 768 }); await studioClient.addLora({ name: characterName, path: result.loraPath }); setTrainingStatus(`Trained from ${result.imageCount} photos and added to the LoRA library.`); } catch (cause) { setError(errorMessage(cause)); } finally { setBusy(null); } };
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
      <h2 className="text-xs font-medium">Character Studio</h2><p className="mt-1 text-[11px] text-muted">Train a Krea-2-Raw LoRA locally with the verified MFLUX runtime. The base model must already exist locally; the trainer runs offline and never downloads behind your back.</p>
      <div className="mt-2 grid gap-2 sm:grid-cols-2"><input value={characterName} onChange={(e) => setCharacterName(e.target.value)} className="h-8 rounded border border-border bg-background px-2 text-xs" placeholder="Character name"/><input value={triggerWord} onChange={(e) => setTriggerWord(e.target.value)} className="h-8 rounded border border-border bg-background px-2 text-xs" placeholder="Trigger word"/><Button size="sm" variant="secondary" onClick={() => void pickDir(setPhotos)}>{photos ? "Photos selected" : "Choose photos folder"}</Button><Button size="sm" variant="secondary" onClick={() => void pickDir(setBaseModel)}>{baseModel ? "Base model selected" : "Choose Krea-2-Raw folder"}</Button></div><Button className="mt-2" size="sm" disabled={!photos || !baseModel || busy !== null} onClick={() => void train()}>{busy === "train" ? "Training…" : "Train character LoRA"}</Button>{trainingStatus && <p className="mt-2 text-[11px] text-success">{trainingStatus}</p>}
    </section>

    <section className="rounded-lg border border-border bg-surface p-3">
      <h2 className="text-xs font-medium">Media workflow presets</h2><p className="mt-1 text-[11px] text-muted">Music, Talking Character, Motion Control and Extend Video run against your own ComfyUI. Export the workflow in API format once; Little Monkey uploads media, binds typed placeholders, retrieves image/video/audio outputs, and files them in the Studio gallery.</p>
      <div className="mt-2 flex flex-wrap gap-2">{(Object.keys(WORKFLOW_PRESETS) as WorkflowKind[]).map((value) => <Button key={value} size="sm" variant={workflowKind === value ? "primary" : "secondary"} onClick={() => setWorkflowKind(value)}>{WORKFLOW_PRESETS[value].label}</Button>)}</div><p className="mt-2 text-[11px] text-muted">{preset.description} Placeholders: {preset.placeholders.map((p) => `{{${p}}}`).join(", ")}</p>
      <div className="mt-2 grid gap-2"><input value={workflowUrl} onChange={(e) => setWorkflowUrl(e.target.value)} className="h-8 rounded border border-border bg-background px-2 font-mono text-xs"/><textarea value={workflowPrompt} onChange={(e) => setWorkflowPrompt(e.target.value)} className="min-h-16 rounded border border-border bg-background p-2 text-xs" placeholder="Prompt"/>{workflowKind === "talking_character" && <textarea value={workflowScript} onChange={(e) => setWorkflowScript(e.target.value)} className="min-h-16 rounded border border-border bg-background p-2 text-xs" placeholder="Dialogue / script"/>}<div className="flex flex-wrap gap-2"><Button size="sm" variant="secondary" onClick={() => void pickWorkflow()}>{workflow ? "Workflow loaded" : "Load API workflow JSON"}</Button>{(workflowKind === "talking_character" || workflowKind === "motion_control") && <Button size="sm" variant="secondary" onClick={() => void pickMedia("image")}>{inputImage ? "Image loaded" : "Choose input image"}</Button>}{workflowKind === "extend_video" && <Button size="sm" variant="secondary" onClick={() => void pickMedia("video")}>{inputVideo ? "Video loaded" : "Choose input video"}</Button>}<Button size="sm" disabled={!workflow || busy !== null} onClick={() => void runWorkflow()}>{busy === "workflow" ? "Running…" : "Run workflow"}</Button></div></div>{workflowResults.length > 0 && <p className="mt-2 text-[11px] text-success">Completed with {workflowResults.length} artifact{workflowResults.length === 1 ? "" : "s"}; results are in the Studio gallery.</p>}
    </section>
    {error && <p role="alert" className="rounded border border-danger/30 bg-danger/5 p-2 text-xs text-danger">{error}</p>}
  </div>;
}
