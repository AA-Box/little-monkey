import { useEffect, useMemo, useState } from 'react';
import {composeDistributedSwarmTrace} from '../../lib/swarmDistributedTrace';
import { open } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { readTextFile, writeTextFile } from '@tauri-apps/plugin-fs';
import { prepareResidentSwarm } from '../../lib/residentSwarmBridge';
import {parseValidatedMlxFusionReceipt,verifyFusedModelConfig} from '../../lib/swarmMlxFusionImport';
import {useModelStore,type ModelInfo} from '../../store/modelStore';
import {promoteVerifiedMlxFusion,recoverInterruptedMlxPromotion,type PromotionRuntime} from '../../lib/swarmMlxPromotion';
import { parseCommandHooksManifest } from '../../lib/commandHookManifest';
import { useUserHooksStore } from '../../store/userHooksStore';
import { daemonSwarmCancel, daemonSwarmStatus, daemonSwarmSubmit,
 daemonClusterSetup,daemonClusterStatus,daemonClusterElect,daemonSwarmReplicaStatus,
 type DaemonClusterStatus,type DaemonSwarmReplica,type ResidentSwarmGraphStatus } from '../../lib/daemonClient';
import { Activity, CheckCircle2, Copy, RefreshCw, ShieldCheck } from 'lucide-react';
import { useShallow } from 'zustand/react/shallow';
import { buildGoalSwarm } from '../../lib/swarmPlanning';
import { SWARM_TOPOLOGIES, type SwarmTopology } from '../../lib/swarmCoordination';
import { SWARM_VOTE_POLICIES,type SwarmVotePolicy } from '../../lib/swarmConsensus';
import { swarmMissionOverview, projectSwarmBudget, estimateSwarmMissionSpend, estimateSwarmRatedSpend } from '../../lib/swarmMissionDashboard';
import {useCostControlStore} from '../../store/costControlStore';
import {prepareRufloMcpBridge} from '../../lib/rufloMcpBridge';
import {
  distillLearningCandidate, listLearningPatterns, decideLearningCandidate,
  forgetLearningPattern,
} from '../../lib/swarmLearningCycle';
import { buildSwarmPreset, SWARM_PRESET_NAMES, type SwarmPresetName } from '../../lib/swarmPresets';
import { listReviewedSwarmPacks, installReviewedSwarmPack, removeReviewedSwarmPack,
  compiledReviewedSwarmPack, type ReviewedSwarmPack } from '../../lib/swarmPackRegistry';
import { prepareSwarmResumeDraft } from '../../lib/swarmRecoveryDraft';
import {
  peersList, peersMemoryShare, peersMemoryPut, peersMemorySync, peersMemorySearch, peersMemoryWatch,
  peersMemoryAddText, peersMemoryQueryText,
  type OutboundPeer,
} from '../../lib/peersClient';
import { loadPeerObservations } from '../../lib/swarmPeerMetrics';
import {
  importVerifiedSpecialists, listVerifiedSpecialists, clearVerifiedSpecialists,
  setVerifiedSpecialistRouting, verifiedSpecialistRoutingEnabled,
  SPECIALIST_DOMAINS, type VerifiedSpecialistDomain,
} from '../../lib/swarmVerifiedSpecialists';
import {
  clearSwarmBenchmark, importSwarmBenchmark, loadSwarmBenchmark,
  setSwarmBenchmarkRouting, swarmBenchmarkRoutingEnabled,
} from '../../lib/swarmVerifiedBenchmarks';
import { skillLearningClient } from '../../lib/skillLearningClient';
import { setSwarmCostOptimization,swarmCostOptimizationEnabled } from '../../lib/swarmCostOptimizer';
import { loadSwarmLiveOutcomes } from '../../lib/swarmLiveOptimizer';
import { decideSwarmNote, listSwarmNotebook, revertSwarmNote } from '../../lib/swarmNotebook';
import { approvedNotebookVectorClaim } from '../../lib/swarmNotebookVector';
import { useSessionStore } from '../../store/sessionStore';
import { useMcpStore } from '../../store/mcpStore';
import { selectWorkflowRunList, useWorkflowStore } from '../../store/workflowStore';
import { Button } from '../ui';

/**
 * Operator-facing bounded native swarm setup and monitoring. A draft is
 * prepared in the active chat, not run invisibly with the Settings dialog's
 * own privileges; Send in the chat is the execution approval boundary.
 */
export function SwarmPanel() {
  const [goal, setGoal] = useState('');
  const [capabilityPack, setCapabilityPack] = useState<SwarmPresetName>('frontend-audit');
  const [reviewedPacks, setReviewedPacks] = useState(() => listReviewedSwarmPacks());
  const [packBusy, setPackBusy] = useState(false);
  const [rufloVersion,setRufloVersion]=useState('');
  const [mode, setMode] = useState<'analyze' | 'implement'>('analyze');
  const [topology, setTopology] = useState<SwarmTopology>('phased');
  const [learningHints, setLearningHints] = useState(false);
  const [tokenBudget, setTokenBudget] = useState('200000');
  const [estimatedUsdPerMillion, setEstimatedUsdPerMillion] = useState('');
  const [learningRevision, setLearningRevision] = useState(0);
  const [fusionImportBusy,setFusionImportBusy]=useState(false);
  const [fusionActivationBusy,setFusionActivationBusy]=useState(false);
  const [importedFusion,setImportedFusion]=useState<ModelInfo|null>(null);
  const [scope, setScope] = useState('');
  const [consensusPolicy, setConsensusPolicy] = useState<SwarmVotePolicy|'none'>('none');
  const [notebook, setNotebook] = useState(false);
  const [notebookRefresh, setNotebookRefresh] = useState(0);
  const [learningBusy, setLearningBusy] = useState<string | null>(null);
  const [benchmark, setBenchmark] = useState(() => loadSwarmBenchmark());
  const [benchmarkRouting, setBenchmarkRouting] = useState(() => swarmBenchmarkRoutingEnabled());
  const [costRouting, setCostRouting] = useState(() => swarmCostOptimizationEnabled());
  const recentOperationalOutcomes=loadSwarmLiveOutcomes();
  const [specialistQuality, setSpecialistQuality] = useState(() => listVerifiedSpecialists());
  const [specialistQualityDomain, setSpecialistQualityDomain] = useState<VerifiedSpecialistDomain>('security');
  const [specialistQualityRouting, setSpecialistQualityRouting] = useState(() => verifiedSpecialistRoutingEnabled('security'));
  const selectedSpecialistQuality = specialistQuality.find(item => item.domain === specialistQualityDomain);
  const learningCandidates = useMemo(() => listLearningPatterns(),
    [learningRevision, specialistQuality]);
  const [benchmarkBusy, setBenchmarkBusy] = useState(false);
  const [pluginBusy, setPluginBusy] = useState<string | null>(null);
  const [resident, setResident] = useState<ResidentSwarmGraphStatus[]>([]);
  const [residentBusy, setResidentBusy] = useState(false);
  const [clusterGroup,setClusterGroup]=useState('');
  const [clusterStatus,setClusterStatus]=useState<DaemonClusterStatus|null>(null);
  const [clusterBusy,setClusterBusy]=useState(false);
  const [replicaView,setReplicaView]=useState<{graph:string;records:DaemonSwarmReplica[]}|null>(null);
  const distributedTrace=useMemo(()=>{
    const selected=resident.find(graph=>graph.id===replicaView?.graph);
    return selected && replicaView
      ?composeDistributedSwarmTrace(selected,replicaView.records):null;
  },[resident,replicaView]);
  const [residentFallbackEnabled,setResidentFallbackEnabled]=useState(false);
  const [hookBusy, setHookBusy] = useState(false);
  const [memoryDevice, setMemoryDevice] = useState('');
  const [memoryAlias, setMemoryAlias] = useState('');
  const [memoryNamespace, setMemoryNamespace] = useState('shared-project');
  const [memoryModel, setMemoryModel] = useState('local-embedding-v1');
  const [memoryStack, setMemoryStack] = useState('');
  const [memoryRecord, setMemoryRecord] = useState('claim-1');
  const [memoryRevision, setMemoryRevision] = useState('1');
  const [memoryText, setMemoryText] = useState('');
  const [memoryVector, setMemoryVector] = useState('');
  const [memoryRead, setMemoryRead] = useState(true);
  const [memoryWrite, setMemoryWrite] = useState(false);
  const [memoryBusy, setMemoryBusy] = useState(false);
  const [memoryMatches, setMemoryMatches] = useState<{ score: number; entry: { author: string; id: string; text: string } }[]>([]);

  const [peerPool, setPeerPool] = useState<string[]>([]);
  const [peers, setPeers] = useState<OutboundPeer[]>([]);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const runs = useWorkflowStore(useShallow(selectWorkflowRunList));
  const sessions = useSessionStore(state => state.sessions);
  const sessionId = useSessionStore(state => state.activeSessionId);
  const metrics = loadPeerObservations();
  const mission = useMemo(() => swarmMissionOverview(sessions, resident), [sessions, resident]);
  const modelCostRates = useCostControlStore(state => state.rates);
  const ratedSpend = useMemo(() => estimateSwarmRatedSpend(sessions,modelCostRates),
    [sessions,modelCostRates]);
  const forecast = useMemo(() => {
    const budget=Number(tokenBudget);
    const unitPrice=estimatedUsdPerMillion.trim()?Number(estimatedUsdPerMillion):null;
    try{
      return projectSwarmBudget(mission,4,
        Number.isSafeInteger(budget)&&budget>0?budget:null,unitPrice);
    }catch{return null;}
  },[mission,tokenBudget,estimatedUsdPerMillion]);
  const observedCost = useMemo(() => {
    try{return estimateSwarmMissionSpend(mission,
      estimatedUsdPerMillion.trim()?Number(estimatedUsdPerMillion):null);}
    catch{return null;}
  },[mission,estimatedUsdPerMillion]);
  const mcpServers = useMcpStore(state => state.servers);
  const refreshPlugins = useMcpStore(state => state.refresh);
  const addMcpServer = useMcpStore(state => state.addServer);
  const notebookProject = sessions.find(session => session.id === sessionId)?.workspacePath?.trim() || 'session:' + sessionId;
  const notes = useMemo(() => listSwarmNotebook(notebookProject), [notebookProject, notebookRefresh, sessions]);

  function parseMemoryVector(): number[] {
    const vector = memoryVector.split(',').map(token => Number(token.trim()));
    if (!memoryVector.trim() || vector.length > 2048 ||
        vector.some(value => !Number.isFinite(value)) || vector.every(value => value === 0)) {
      throw new Error('Supply 1–2048 comma-separated finite, nonzero vector coordinates.');
    }
    return vector;
  }
  async function memoryAction(action: 'share' | 'write' | 'sync' | 'search' | 'watch' | 'unwatch' | 'embed-write' | 'embed-search') {
    setMemoryBusy(true); setError(''); setNotice('');
    try {
      if (action === 'share') {
        if (!window.confirm('Change the per-namespace memory ACL of this inbound paired device? Read grants expose text and embeddings; write grants allow durable peer records.')) return;
        await peersMemoryShare(memoryDevice.trim(), memoryNamespace.trim(), memoryRead, memoryWrite);
        setNotice('Namespace ACL saved on this installation.');
      } else if (action === 'embed-write') {
        const revision = Number(memoryRevision);
        if (!Number.isSafeInteger(revision) || revision < 1) throw new Error('Revision must be a positive integer.');
        const result = await peersMemoryAddText(memoryNamespace.trim(), memoryRecord.trim(), memoryStack.trim(), memoryText, revision);
        setNotice(result.updated ? 'Text embedded with the configured model and saved. Sync to replicate.' : 'Same revision already saved.');
      } else if (action === 'embed-search') {
        const result = await peersMemoryQueryText(memoryNamespace.trim(), memoryStack.trim(), memoryText, 5);
        setMemoryMatches(result.matches);
        setNotice('Text embedded and semantically searched using the configured Knowledge 2.0 model.');
      } else if (action === 'watch' || action === 'unwatch') {
        const enabled = action === 'watch';
        if (enabled && !window.confirm('Enable automatic vector synchronization with this paired peer every 60 seconds while the installed daemon is running? Both sides must explicitly authorize this namespace.')) return;
        await peersMemoryWatch(memoryAlias.trim(), memoryNamespace.trim(), enabled);
        setNotice(enabled ? 'Resident vector autosync enabled. An offline peer is retried on subsequent ticks.' :
          'Resident vector autosync disabled for this peer and namespace.');
      } else if (action === 'write') {
        const revision = Number(memoryRevision);
        if (!Number.isSafeInteger(revision) || revision < 1) throw new Error('Revision must be a positive integer.');
        const updated = await peersMemoryPut(memoryNamespace.trim(), {
          id: memoryRecord.trim(), model: memoryModel.trim(), text: memoryText,
          revision, vector: parseMemoryVector(),
        });
        setNotice(updated.updated ? 'Local vector revision stored; use Sync to replicate.' : 'Identical revision already stored.');
      } else if (action === 'sync') {
        if (!window.confirm('Replicate locally authored vector records with this explicitly paired node? Both sides must grant memory-read/write and the remote namespace ACL.')) return;
        const result = await peersMemorySync(memoryAlias.trim(), memoryNamespace.trim());
        setNotice('Vector sync complete: received ' + result.pulled + ', sent ' + result.pushed + ' records.');
      } else {
        const result = await peersMemorySearch(memoryNamespace.trim(), memoryModel.trim(), parseMemoryVector(), 5);
        setMemoryMatches(result.matches);
        setNotice('Vector search completed.');
      }
    } catch (cause) { setError('Shared vector memory: ' + String(cause)); }
    finally { setMemoryBusy(false); }
  }

  async function refreshPeers() {
    try { setPeers((await peersList()).outbound); setError(''); }
    catch (e) { setError('Peer list unavailable: ' + String(e)); }
  }
  useEffect(() => { void refreshPeers(); }, []);
  useEffect(() => { void refreshPlugins().catch(() => undefined); }, [refreshPlugins]);
  async function signedClusterAction(action:'setup'|'status'|'elect') {
    setClusterBusy(true);setError('');setNotice('');
    try {
      if(action==='setup'){
        const selected=await open({multiple:false,filters:[
          {name:'Signed peer quorum roster',extensions:['json']},
        ]});
        if(typeof selected!=='string')return;
        if(!window.confirm('Enroll this explicit roster of already paired device identities and TLS certificate fingerprints on THIS machine? This grants no task permissions. Your other installations must be enrolled separately.'))return;
        const result=await daemonClusterSetup(selected);
        setClusterGroup(result.cluster);
        setClusterStatus(await daemonClusterStatus(result.cluster));
        setNotice('Local signed peer roster enrolled: '+result.members+
          ' members. No election has been implied by enrollment.');
      }else{
        const group=clusterGroup.trim();
        if(!group)throw Error('Enter an operator-enrolled cluster group.');
        if(action==='elect'){
          if(!window.confirm('Request actual signed votes from the pinned paired nodes for '+
            group+'? If a strict majority grants a lease, pre-approved local read-only resident graph tasks may start automatically. No permission grants or remote edits will be created.'))return;
          const answer=await daemonClusterElect(group);
          if(answer.status!=='elected'||answer.quorum<=Math.floor(answer.members/2))
            throw Error('The native election did not return a valid strict majority.');
          setNotice('Actual signed quorum elected term '+answer.term+
            ' with '+answer.quorum+'/'+answer.members+' voters. Re-read lease status for current authorization.');
        }
        setClusterStatus(await daemonClusterStatus(group));
      }
    }catch(error){setError('Signed cluster: '+String(error));}
    finally{setClusterBusy(false);}
  }
  async function refreshResident() {
    try { setResident((await daemonSwarmStatus()).graphs); }
    catch (cause) { setError('Resident service unavailable: ' + String(cause)); }
  }
  useEffect(() => { void refreshResident(); }, []);
  function mlxPromotionRuntime():PromotionRuntime {
    return {
      installed:()=>useModelStore.getState().installed,
      active:()=>useModelStore.getState().active,
      activeProvider:()=>useModelStore.getState().activeProvider,
      otherLocalRunning:()=>{
        const st=useModelStore.getState();
        return st.llamaStatus==='ready' && !st.mlxChat?.running;
      },
      runningPath:()=>{
        const state=useModelStore.getState().mlxChat;
        return state?.running?state.modelPath:null;
      },
      start:model=>useModelStore.getState().start(model),
      stop:()=>useModelStore.getState().stop(),
      clearInactive:previous=>useModelStore.setState({
        active:previous,activeProvider:'local',llamaStatus:'stopped',
      }),
      probe:path=>invoke('mlx_chat_canary',{expectedModelPath:path}),
    };
  }
  useEffect(()=>{
    if(!localStorage.getItem('little-monkey-mlx-fusion-promotion-v1'))return;
    let alive=true;
    void (async()=>{
      try{
        await useModelStore.getState().refresh();
        const recovered=await recoverInterruptedMlxPromotion(mlxPromotionRuntime(),localStorage);
        if(alive&&recovered)setNotice('Recovered the previous model from an interrupted MLX activation. Candidate was NOT accepted.');
      }catch(error){
        if(alive)setError('Interrupted MLX model promotion requires manual recovery: '+String(error));
      }
    })();
    return ()=>{alive=false;};
  },[]);
  async function activateImportedMlxFusion(){
    if(!importedFusion)return;
    setFusionActivationBusy(true);setError('');setNotice('');
    try{
      if(!window.confirm('Switch to the evaluated fused model, execute a REAL local MLX chat completion, and restore the previous local model automatically if inference fails? Other AI provider modes are refused.'))return;
      const result=await promoteVerifiedMlxFusion(importedFusion,mlxPromotionRuntime(),localStorage);
      setNotice('New fused MLX model passed a real local inference canary ('+result.nonemptyOutputChars+' output characters) and is now active. Previous weights remain installed for rollback.');
    }catch(e){setError('MLX activation failed: '+String(e));}
    finally{setFusionActivationBusy(false);}
  }

  async function importValidatedMlxFusion() {
    setError('');setNotice('');
    const selected=await open({multiple:false,filters:[
      {name:'Evaluated MLX training receipt',extensions:['json']},
    ]});
    if(typeof selected!=='string')return;
    setFusionImportBusy(true);
    try{
      const raw:unknown=JSON.parse(await readTextFile(selected));
      const receipt=parseValidatedMlxFusionReceipt(raw);
      // The selected receipt only authorizes reading that file. Explicitly
      // ask the operator to select the model folder too; Tauri FS scopes
      // must not be silently widened by a JSON-controlled path.
      const selectedFolder=await open({directory:true,multiple:false});
      if(typeof selectedFolder!=='string')return;
      const normalize=(path:string)=>path.replace(/\\/g,'/').replace(/\/+$/,'');
      if(normalize(selectedFolder)!==normalize(receipt.fusedModelPath))
        throw Error('Selected MLX model folder does not match the evaluated receipt.');
      const text=await readTextFile(selectedFolder.replace(/[\\/]$/,'')+'/config.json');
      await verifyFusedModelConfig(text,receipt.fusedConfigSha256);
      if(!window.confirm('Verify and register this fused MLX model? Native verification streams all '+
        receipt.fusedWeights.length+' weight shards (potentially many gigabytes) and checks the config too. It does NOT activate the model.'))return;
      if(!window.confirm('Import this locally trained MLX model directory WITHOUT activating it?\n'+
        receipt.fusedModelPath+'\n\n'+
        'Measured fused held-out perplexity: '+receipt.fusedTestPerplexity+
        '. The receipt and hash are local evidence, NOT a trusted signature. '+
        'Review the folder source and tests, then select/test the model manually after import.'))return;
      const models=await invoke<ModelInfo[]>('models_import_verified_fusion', {
        path:selectedFolder,configSha256:receipt.fusedConfigSha256,files:receipt.fusedWeights,
      });
      await useModelStore.getState().refresh();
      const registered=models.find(model=>model.runtime==='mlx'&&
        model.path?.replace(/\\/g,'/').replace(/\/+$/,'')===
          selectedFolder.replace(/\\/g,'/').replace(/\/+$/,''));
      if(!registered)throw Error('Native importer did not register the exact operator-selected MLX directory.');
      const real=useModelStore.getState().installed.find(model=>model.id===registered.id);
      if(!real)throw Error('Registered fused model is not present in refreshed native catalog.');
      setImportedFusion(real);
      setNotice('Fused MLX model is registered and verified. It has NOT been activated; use the real-inference canary action to promote safely.');
    }catch(cause){setError('Fused model not imported: '+String(cause));}
    finally{setFusionImportBusy(false);}
  }
  async function submitResident() {
    setError(''); setNotice('');
    const selected = await open({ multiple: false, filters: [{ name: 'Swarm JSON graph', extensions: ['json'] }] });
    if (typeof selected !== 'string') return;
    if (!window.confirm('Register this JSON graph with the resident OS daemon? Every node must reference an already configured local recipe with manual/plan permissions. The daemon will run it even after this app closes.')) return;
    setResidentBusy(true);
    try {
      const result = await daemonSwarmSubmit(selected);
      setNotice('Registered durable swarm ' + result.id + ' with ' + result.nodes + ' nodes.');
      await refreshResident();
    } catch (cause) { setError('Swarm registration refused: ' + String(cause)); }
    finally { setResidentBusy(false); }
  }
  async function submitGoalToResident() {
    if (!config.spec) return;
    setError(''); setNotice('');
    setResidentBusy(true);
    try {
      // Every privilege-bearing field comes from a user-selected, local
      // immutable model recipe, never from model-generated agent arguments.
      const recipePath = await open({ multiple: false, filters: [
        { name: 'Operator-approved JSON task recipe', extensions: ['json'] },
      ] });
      if (typeof recipePath !== 'string') return;
      const directory = await open({ directory: true, multiple: false });
      if (typeof directory !== 'string') return;
      const base = JSON.parse(await readTextFile(recipePath)) as unknown;
      let alternate:unknown=undefined;
      if(mode==='analyze' && residentFallbackEnabled){
        const backupPath=await open({multiple:false,filters:[
          {name:'Read-only PLAN fallback model recipe',extensions:['json']},
        ]});
        if(typeof backupPath!=='string')return;
        alternate=JSON.parse(await readTextFile(backupPath)) as unknown;
      }
      const graphId = 'swarm-' + crypto.randomUUID().replace(/-/g, '').slice(0, 16);
      const prepared = prepareResidentSwarm(config.spec, base, directory, graphId, mode, alternate);
      const approved = window.confirm(
        'Queue ' + prepared.graph.nodes.length + ' agents as frozen resident tasks?\n\n' +
        'Model and workspace: the recipe you selected.\n' +
        (mode === 'implement'
          ? 'Permission: write-capable agents require MANUAL approval for every mutation. No auto-push or deploy.\n'
          : 'Permission: enforced PLAN mode, no writes or remote peer grants.\n') +
        'Output: ' + prepared.graphPath + '\n\n' +
        prepared.caveat + '\n\n' +
        'The daemon can continue after the app closes. Review the model/workspace before approving.',
      );
      if (!approved) return;
      for (const recipe of prepared.recipes) {
        await writeTextFile(recipe.path, recipe.contents, { createNew: true });
      }
      await writeTextFile(prepared.graphPath, JSON.stringify(prepared.graph, null, 2), { createNew: true });
      const result = await daemonSwarmSubmit(prepared.graphPath);
      setNotice('Resident ' + mode + ' workflow ' + result.id + ' registered (' + result.nodes +
        ' frozen jobs). It continues under the daemon after app exit. ' + prepared.caveat);
      await refreshResident();
    } catch (cause) {
      setError('Resident handoff refused or failed: ' + String(cause) +
        '. Any already written recipe files are inert; no job is submitted until registration succeeds.');
    } finally { setResidentBusy(false); }
  }

  async function cancelResident(id: string) {
    if (!window.confirm('Request cancellation of every still-running job in this resident swarm? Execution may already have side effects.')) return;
    setResidentBusy(true); setError('');
    try { await daemonSwarmCancel(id); setNotice('Cancellation requested for ' + id + '. Completion is not yet confirmed.'); await refreshResident(); }
    catch (cause) { setError('Cancellation not confirmed: ' + String(cause)); }
    finally { setResidentBusy(false); }
  }

  const config = useMemo(() => {
    try {
      const args: Record<string, unknown> = { goal, mode };
      if (mode === 'analyze' && topology !== 'phased') args.topology = topology;
      if (mode === 'analyze' && learningHints) args.learning = true;
      if (scope.trim()) args.scope = scope.trim();
      if (mode === 'analyze' && consensusPolicy!=='none') args.consensus = consensusPolicy;
      if (notebook) args.notebook = true;
      if (mode === 'analyze' && peerPool.length) args.auto_peer_pool = peerPool;
      const spec = buildGoalSwarm(args);
      return { args, spec, problem: '' };
    } catch (cause) {
      return { args: null, spec: null, problem: String(cause).replace(/^Error: /, '') };
    }
  }, [goal, mode, topology, learningHints, scope, peerPool, consensusPolicy, notebook]);

  const restored = sessions.flatMap(session => Object.entries(session.workflowRunMeta ?? {}).map(([id, run]) => ({
    id, name: run.name, status: run.status, startedAt: run.startedAt,
    completed: Object.values(run.agentResults ?? {}).filter(item => item.status === 'done').length,
    total: run.phases.reduce((sum, phase) => sum + phase.agents.length, 0),
    sessionId: session.id, sourceRunId: run.sourceRunId ?? null, description: run.description,
    recoverable: Boolean(run.spec),
    phases: run.phases,
    resultJournal: run.agentResults ?? {},
    elapsedMs: Math.max(0, run.finishedAt - run.startedAt),
  }))).sort((a, b) => b.startedAt - a.startedAt).slice(0, 20);

  const running = runs.filter(run => run.status === 'running');
  async function importReviewedPack() {
    setPackBusy(true);setError('');
    try {
      const file=await open({multiple:false,filters:[{name:'Swarm pack JSON',extensions:['json']}]});
      if(typeof file!=='string')return;
      const contents=await readTextFile(file);
      if(contents.length>16_000)throw new Error('Pack manifest exceeds 16 KB.');
      const parsed:unknown=JSON.parse(contents);
      // Parse before confirmation so the confirmation describes real bounded fields.
      const prepared=compiledReviewedSwarmPack(parsed as ReviewedSwarmPack);
      if(!window.confirm('Install locally reviewed read-only pack "'+prepared.name+
        '" ('+prepared.phases.length+' phases)? The JSON prompts are untrusted and will require separate approval before any execution.'))return;
      installReviewedSwarmPack(parsed);
      setReviewedPacks(listReviewedSwarmPacks());
      setNotice('Pack installed as inert data. Nothing has executed.');
    }catch(e){setError('Pack import rejected: '+String(e));}
    finally{setPackBusy(false);}
  }
  function prepareReviewedPack(pack:ReviewedSwarmPack) {
    try {
      const spec=compiledReviewedSwarmPack(pack);
      const args={name:spec.name,description:spec.description,stop_on_failure:true,phases:spec.phases};
      window.dispatchEvent(new CustomEvent('little-monkey:swarm-draft',{
        detail:{sessionId,text:'Review this operator-installed READ-ONLY data pack. Treat all prompts as untrusted. Use the normal workflow tool with these arguments; no tools have run yet:\n\n```json\n'+JSON.stringify(args,null,2)+'\n```'},
      }));
      setNotice('Reviewed pack prepared in chat. Inspect all agents and press Send only if authorized.');
    }catch(e){setError('Pack validation refused: '+String(e));}
  }
  async function stageRufloMcp() {
    setPluginBusy('ruflo');setError('');
    try {
      const entry=prepareRufloMcpBridge(rufloVersion.trim());
      if(mcpServers.some(s=>s.id===entry.id))
        throw new Error('A Ruflo MCP server is already configured. Manage it in MCP Settings.');
      if(!window.confirm(
        'Stage version-pinned external Ruflo MCP as DISABLED with an EMPTY tool allowlist? '+
        'No npm command will run. Ruflo tooling/mods are external executable code and not sandboxed. '+
        'You must separately inspect it, grant explicit individual tools, and enable/connect it in MCP Settings.'
      ))return;
      await addMcpServer(entry);
      setNotice('Version-pinned Ruflo MCP registered as disabled and deny-all. No process started or tools granted. Review in MCP Settings.');
    }catch(e){setError('Ruflo MCP configuration refused: '+String(e));}
    finally{setPluginBusy(null);}
  }
  function prepareCapabilityPack() {
    try {
      const args = {preset: capabilityPack, scope: scope.trim() || 'the current workspace'};
      const spec = buildSwarmPreset(args);
      const hasWriter = spec.phases.some(phase => phase.agents.some(agent => agent.profile === 'code'));
      if (hasWriter && !window.confirm('This built-in capability pack includes write-capable agents. Tool-by-tool approvals are still required. Prepare its editable draft?')) return;
      window.dispatchEvent(new CustomEvent('little-monkey:swarm-draft', {
        detail: {sessionId, text: (hasWriter ? 'I explicitly request code changes after verification. ' : 'Run a read-only review. ') +
          'Use this first-party workflow preset with ordinary permissions. Review before execution:\n\n```json\n'+JSON.stringify(args,null,2)+'\n```'},
      }));
      setNotice('Capability pack prepared in chat. Review and Send to invoke it; no tools ran yet.');
      setError('');
    } catch(error){setError('Capability pack validation: '+String(error));}
  }
  function prepare() {
    if (!config.spec || !config.args) return;
    const prompt = mode === 'implement'
      ? 'I explicitly request the following code implementation. Use the workflow tool with these exact arguments, obey every permission prompt, and report real test results:'
      : 'Run a native, read-only agent swarm using the workflow tool with these exact arguments. Independently verify findings and report supporting paths:';
    window.dispatchEvent(new CustomEvent('little-monkey:swarm-draft', {
      detail: {
        sessionId,
        text: prompt + '\n\n```json\n' + JSON.stringify(config.args, null, 2) + '\n```',
      },
    }));
    setNotice('Prepared in your active chat composer. Close Settings, review the draft and press Send to start.');
  }
  function prepareResume(session: string, id: string) {
    const targetSession = useSessionStore.getState().sessions.find(item => item.id === session);
    const run = targetSession?.workflowRunMeta?.[id];
    if (!run) { setError('The saved workflow is no longer present in its original conversation.'); return; }
    let draft: ReturnType<typeof prepareSwarmResumeDraft>;
    try {
      draft = prepareSwarmResumeDraft(id, run);
    } catch (cause) { setError('Cannot prepare a safe workflow resume: ' + String(cause)); return; }
    if (draft.requiresMutationReview && !window.confirm(
      'This interrupted workflow includes write-capable or isolated agents. Check the repository diff and existing side effects before authorizing any repeat execution. Prepare an editable resume request in the original chat?'
    )) return;
    // Switching conversations re-renders ChatWindow. Deliver the draft only
    // after that render, to the listener scoped to this exact session.
    if (useSessionStore.getState().activeSessionId !== session) {
      useSessionStore.getState().switchSession(session);
    }
    window.requestAnimationFrame(() => {
      window.dispatchEvent(new CustomEvent('little-monkey:swarm-draft', {
        detail: { sessionId: session, text: draft.text },
      }));
    });
    setNotice('Resume request prepared in the original chat. Review it and press Send; no agent was launched by this button.');
    setError('');
  }
  function decide(id: string, version: number, decision: 'approved' | 'rejected') {
    if (decision === 'approved' && !window.confirm('Approve this model-proposed claim for use as untrusted reference in future project swarms?')) return;
    try { decideSwarmNote(notebookProject, id, version, decision); setNotebookRefresh(current => current + 1); setError(''); }
    catch (cause) { setError(String(cause)); }
  }
  async function publishApprovedVector(id: string) {
    setMemoryBusy(true); setError(''); setNotice('');
    try {
      const note=notes.find(item=>item.id===id);
      if (!note) throw new Error('Approved notebook claim no longer exists.');
      const exportable=approvedNotebookVectorClaim(note);
      const revision=Number(memoryRevision);
      if (!Number.isSafeInteger(revision) || revision<1 || revision>1_000_000) {
        throw new Error('Supply a positive monotonically increasing vector revision (1–1,000,000).');
      }
      if (!memoryStack.trim() || !memoryNamespace.trim()) {
        throw new Error('Choose an existing Knowledge embedding stack and memory namespace first.');
      }
      if (!window.confirm(
        'Embed the approved notebook evidence with the configured Knowledge 2.0 stack and store it in "'+
        memoryNamespace+'"? If peer autosync is enabled, the receiver may receive these exact evidence locations. Review their sensitivity and the selected vector revision before continuing.'
      )) return;
      const result=await peersMemoryAddText(memoryNamespace.trim(),exportable.id,memoryStack.trim(),exportable.text,revision);
      setNotice(result.updated
        ? 'Approved notebook revision '+exportable.approvedVersion+' embedded as local vector '+exportable.id+'. Sync only to explicitly authorized peers.'
        : 'The same vector revision already exists; no duplicate was written.');
    } catch (cause) {setError('Approved claim export failed: '+String(cause));}
    finally {setMemoryBusy(false);}
  }
  function rollback(id: string, version: number) {
    if (!window.confirm('Restore this earlier approved revision for future swarm context?')) return;
    try { revertSwarmNote(notebookProject, id, version); setNotebookRefresh(current => current + 1); setError(''); }
    catch (cause) { setError(String(cause)); }
  }
  async function importHooks() {
    setError(''); setNotice('');
    const selected = await open({ multiple: false, filters: [{ name: 'Command-hook JSON', extensions: ['json'] }] });
    if (typeof selected !== 'string') return;
    setHookBusy(true);
    try {
      const source = await readTextFile(selected);
      const preview = parseCommandHooksManifest(source);
      if (!preview.supported.length) {
        setNotice('No supported command hooks found. Unsupported groups: ' + preview.unsupported.slice(0, 5).join('; '));
        return;
      }
      const examples = preview.supported.slice(0, 12).map(hook => hook.event + ': ' + hook.command.slice(0, 90)).join('\\n');
      if (!window.confirm('Enable ' + preview.supported.length +
          ' executable hook commands? These run on your computer outside the model tool sandbox. Review every command before approving.\\n\\n' +
          examples + (preview.supported.length > 12 ? '\\n...' : '') +
          '\\n\\nUnsupported groups (not installed): ' + preview.unsupported.length)) return;
      const store = useUserHooksStore.getState();
      const existing = new Set(store.hooks.map(hook => [hook.event, hook.command, hook.matcher ?? ''].join('\\u0000')));
      let installed = 0;
      for (const hook of preview.supported) {
        const key = [hook.event, hook.command, hook.matcher ?? ''].join('\\u0000');
        if (existing.has(key)) continue;
        store.add(hook);
        existing.add(key);
        installed += 1;
      }
      setNotice(installed + ' explicitly approved lifecycle hooks configured. Unsupported groups skipped: ' +
        preview.unsupported.length + '. Manage or remove individual commands under User Hooks settings.');
    } catch (cause) { setError('Hook import refused: ' + String(cause)); }
    finally { setHookBusy(false); }
  }
  async function operatePlugin(id: string, action: 'connect' | 'disconnect' | 'enable' | 'disable') {
    setPluginBusy(id); setError(''); setNotice('');
    try {
      const store = useMcpStore.getState();
      if (action === 'connect') {
        if (!window.confirm('Launch/connect this operator-configured MCP server? External stdio servers run outside Little Monkey\'s workspace sandbox.')) return;
        await store.connect(id);
      } else if (action === 'disconnect') await store.disconnect(id);
      else await store.setEnabled(id, action === 'enable');
      await store.refresh();
      setNotice('MCP server state updated. Review its advertised tool allowlist in MCP settings.');
    } catch (cause) { setError('MCP connection action failed: ' + String(cause)); }
    finally { setPluginBusy(null); }
  }
  async function proposeLearningCandidate(sourceRunId: string, goalText: string) {
    setLearningBusy(sourceRunId); setError(''); setNotice('');
    try {
      const eligible = await skillLearningClient.captureEligibility(sourceRunId, goalText);
      if (!eligible) {
        setNotice('The native run ledger has no verified learning-eligible evidence for this swarm. No skill was staged.');
        return;
      }
      const result = await skillLearningClient.capture(sourceRunId, goalText);
      setNotice(result.kind === 'created'
        ? 'Learning candidate staged for evaluation and operator review. This does not promote a skill.'
        : 'A candidate or learned skill already exists. Review it under Skill Learning settings.');
    } catch (cause) { setError('Could not capture verified learning evidence: ' + String(cause)); }
    finally { setLearningBusy(null); }
  }
  async function importLiveBenchmark() {
    setError(''); setNotice('');
    const selected = await open({ multiple: false, filters: [{
      name: 'Little Monkey real-model benchmark JSON', extensions: ['json'],
    }] });
    if (typeof selected !== 'string') return;
    setBenchmarkBusy(true);
    try {
      const raw = await readTextFile(selected);
      if (raw.length > 64_000) throw new Error('Benchmark JSON exceeds the 64 KiB import limit.');
      const summary = importSwarmBenchmark(JSON.parse(raw));
      setBenchmark(summary);
      setSwarmBenchmarkRouting(false);
      setBenchmarkRouting(false);
      setNotice('Imported operator-provided fixture measurements. Review every target before opting in. Only existing policy-eligible subagent targets may be reordered.');
    } catch (cause) { setError('Benchmark import refused: ' + String(cause)); }
    finally { setBenchmarkBusy(false); }
  }
  function changeBenchmarkRouting(enabled: boolean) {
    if (enabled && !benchmark) return;
    if (enabled && !window.confirm('Use these operator-provided accuracy measurements to rank subagent model candidates already permitted by a routing policy? Explicit model pins still win.')) return;
    setSwarmBenchmarkRouting(enabled);
    setBenchmarkRouting(enabled);
  }
  function deleteBenchmark() {
    if (!window.confirm('Clear the locally stored benchmark data and turn off quality routing?')) return;
    clearSwarmBenchmark();
    setBenchmark(null);
    setBenchmarkRouting(false);
    setNotice('Stored benchmark removed and accuracy-based routing disabled.');
  }
  async function importSpecialistQuality() {
    setError(''); setNotice('');
    const selected = await open({multiple:false,filters:[{
      name:'Verified real-model specialist benchmark',extensions:['json'],
    }]});
    if (typeof selected !== 'string') return;
    setBenchmarkBusy(true);
    try {
      const raw=await readTextFile(selected);
      if (raw.length > 100_000) throw new Error('Specialist evaluation exceeds the 100 KiB import limit.');
      const summary = importVerifiedSpecialists(JSON.parse(raw));
      setSpecialistQuality(listVerifiedSpecialists());
      setSpecialistQualityDomain(summary.domain);
      setSpecialistQualityRouting(false);
      setNotice('Imported independent ' + summary.domain + ' benchmark. Its quality routing is OFF until reviewed. Other domains keep their existing settings.');
    } catch(error) {setError('Specialist quality import refused: '+String(error));}
    finally {setBenchmarkBusy(false);}
  }
  function toggleSpecialistQuality(enabled: boolean) {
    if (enabled && !selectedSpecialistQuality) return;
    if (enabled && !window.confirm(
      'Enable independently checked real-model accuracy for the supplemental ' + specialistQualityDomain +
      ' reviewer only? No additional agents, tool permissions or peer capabilities are granted.'
    )) return;
    setVerifiedSpecialistRouting(enabled, specialistQualityDomain);
    setSpecialistQualityRouting(verifiedSpecialistRoutingEnabled(specialistQualityDomain));
  }
  function removeSpecialistQuality() {
    if (!window.confirm('Clear verified ' + specialistQualityDomain + ' specialist accuracy and disable its ranking only?')) return;
    clearVerifiedSpecialists(specialistQualityDomain);
    setSpecialistQuality(listVerifiedSpecialists());
    setSpecialistQualityRouting(false);
    setNotice('Verified ' + specialistQualityDomain + ' measurements removed; other domains retained.');
  }
  async function copyConfig() {
    if (!config.args) return;
    try { await navigator.clipboard.writeText(JSON.stringify(config.args, null, 2)); setNotice('Configuration copied.'); }
    catch (cause) { setError('Clipboard unavailable: ' + String(cause)); }
  }
  return (
    <div className="space-y-4">
      <div className="rounded-lg border border-border bg-surface p-3">
        <h4 className="text-sm font-semibold text-foreground">Native agent swarm</h4>
        <p className="mt-1 text-xs leading-5 text-muted">Compose an evidence-led, permission-bounded workflow. No model runs until you review and submit its draft in chat. Read-only peers always use the recipient installation's own task recipe and permissions.</p>
        <label className="mt-3 block text-xs text-muted">Goal
          <textarea aria-label="Swarm goal" rows={3} maxLength={1200} value={goal} onChange={event => setGoal(event.target.value)}
            className="mt-1 w-full rounded-md border border-border bg-background p-2 text-foreground"
            placeholder="Analyze API authorization correctness and provide source evidence" />
        </label>
        <div className="mt-3 grid gap-3 sm:grid-cols-2">
          <label className="text-xs text-muted">Execution mode
            <select aria-label="Swarm execution mode" className="mt-1 w-full rounded-md border border-border bg-background p-2 text-foreground"
              value={mode} onChange={event => { setMode(event.target.value as 'analyze' | 'implement'); setPeerPool([]); }}>
              <option value="analyze">Read-only analysis</option>
              <option value="implement">Permission-gated implementation</option>
            </select>
          </label>
          <label className="text-xs text-muted">Optional scope
            <input aria-label="Swarm scope" maxLength={300} className="mt-1 w-full rounded-md border border-border bg-background p-2 text-foreground"
              value={scope} onChange={event => setScope(event.target.value)} placeholder="Selected repository or module" />
          </label>
        </div>
        <label className="mt-3 flex gap-2 text-xs text-foreground">
          <input type="checkbox" checked={notebook} onChange={event => setNotebook(event.target.checked)} />
          Use approved versioned project notebook claims (local-only; no remote peer forwarding)
        </label>
        {mode === 'analyze' && <label className="mt-3 block text-xs text-foreground">
          Independent source-claim vote policy (non-authoritative; dissent always disputed)
          <select aria-label="Swarm review vote policy" className="ml-2 rounded border border-border bg-background p-1"
            value={consensusPolicy} onChange={e=>setConsensusPolicy(e.target.value as SwarmVotePolicy|'none')}>
            <option value="none">No structured votes</option>
            {SWARM_VOTE_POLICIES.map(value=><option key={value} value={value}>{value}</option>)}
          </select>
        </label>}
        {mode === 'analyze' && <div className="mt-3 flex flex-wrap items-center gap-3 text-xs text-foreground">
          <label className="flex items-center gap-2">Team topology
            <select aria-label="Swarm team topology" value={topology}
              onChange={event => setTopology(event.target.value as SwarmTopology)}
              className="rounded border border-border bg-background px-2 py-1">
              {SWARM_TOPOLOGIES.map(value=><option key={value} value={value}>{value}</option>)}
            </select>
          </label>
          <label className="flex items-center gap-2">
            <input type="checkbox" checked={learningHints} onChange={e=>setLearningHints(e.target.checked)}/>
            Retrieve operator-approved historical learning (local only)
          </label>
        </div>}
        {mode === 'analyze' && <div className="mt-3 rounded-md border border-border p-3">
          <div className="flex items-center justify-between">
            <span className="text-xs font-semibold text-foreground">Approved peer pool (optional, maximum 3)</span>
            <Button size="sm" onClick={() => void refreshPeers()}><RefreshCw size={13}/> Refresh</Button>
          </div>
          <p className="mt-1 text-xs text-muted">Only task-granted peers can receive a read-only discovery job. Actual permissions remain under each receiver's task recipe.</p>
          {peers.length === 0 && <p className="mt-2 text-xs text-faint">No paired peers available. The swarm will run locally.</p>}
          {peers.map(peer => {
            const allowed = peer.grants.includes('task');
            const selected = peerPool.includes(peer.alias);
            const metric = metrics[peer.alias];
            return <label key={peer.alias} className="mt-2 flex items-center gap-2 text-xs text-foreground">
              <input type="checkbox" disabled={!allowed || (!selected && peerPool.length >= 3)}
                checked={selected} onChange={event => setPeerPool(old =>
                  event.target.checked ? [...old, peer.alias] : old.filter(alias => alias !== peer.alias))} />
              <span className="font-medium">{peer.alias}</span>
              <span className="text-faint">{peer.presence} · {allowed ? 'Task grant' : 'No task grant'} · {metric ? `${metric.successes} completed / ${metric.failures} failed` : 'No observations'}</span>
            </label>;
          })}
        </div>}
        {config.problem && goal.trim() && <p className="mt-3 text-xs text-warning" role="alert">{config.problem}</p>}
        {config.spec && <p className="mt-3 text-xs text-muted">
          <ShieldCheck className="mr-1 inline" size={14}/> {config.spec.phases.length} ordered phases · {config.spec.phases.reduce((count, phase) => count + phase.agents.length, 0)} agents · {mode === 'analyze' ? 'No code writes permitted by local agent profiles' : 'Code tools require existing approvals'}
        </p>}
        <div className="mt-3 flex flex-wrap gap-2">
          <Button variant="primary" disabled={!config.spec} onClick={prepare}><Activity size={14}/> Prepare in chat</Button>
          <Button disabled={!config.spec} onClick={() => void copyConfig()}><Copy size={14}/> Copy tool arguments</Button>
        </div>
        {notice && <p role="status" className="mt-2 text-xs text-muted">{notice}</p>}
        {error && <p role="alert" className="mt-2 text-xs text-warning">{error}</p>}
      </div>
      <div className="rounded-lg border border-border bg-surface p-3">
        <h4 className="text-sm font-semibold text-foreground">Verified-fixture model accuracy</h4>
        <p className="mt-1 text-xs text-muted">Import measurements produced by <code>pnpm test:swarm-models:live</code> on real configured models. Results are operator-provided and not cryptographic proof. Little Monkey independently validates fixture completeness and scores. Opt-in routing affects subagents only, after eligibility and routing-policy restrictions; explicit preferred models always win.</p>
        <div className="mt-3 flex flex-wrap gap-2">
          <Button size="sm" disabled={benchmarkBusy} onClick={() => void importLiveBenchmark()}>Import real-model benchmark JSON</Button>
          {benchmark && <Button size="sm" disabled={benchmarkBusy} onClick={deleteBenchmark}>Clear measurements</Button>}
        </div>
        {benchmark && <>
          <p className="mt-2 text-xs text-muted">Generated {benchmark.generatedAt} · {benchmark.ranking.length} measured targets</p>
          {benchmark.ranking.map(row => <p key={row.target} className="mt-1 break-all text-xs text-foreground">
            {row.target} — {row.passes}/{row.attempts} verified fixtures ({Math.round(100*row.accuracy)}%) · {row.averageVerifiedMs === null ? 'no verified latency' : row.averageVerifiedMs + ' ms average'}
          </p>)}
          <label className="mt-3 flex items-start gap-2 text-xs text-foreground">
            <input type="checkbox" checked={benchmarkRouting}
              onChange={event => changeBenchmarkRouting(event.target.checked)} />
            Opt in to quality-ranked candidate order for subagents when a routing policy has no explicit preferred models.
          </label>
          <label className="mt-2 flex items-start gap-2 text-xs text-foreground">
            <input type="checkbox" checked={costRouting}
              onChange={event => {
                setSwarmCostOptimization(event.target.checked);
                setCostRouting(swarmCostOptimizationEnabled());
              }}/>
            Additionally optimize price among independently graded, quality-equivalent, already eligible subagent targets. Requires six fixture observations per model, configured input/output rates, and at least three historical usage samples. Minimum 10% estimated rate savings; never overrides model pins, policy restrictions or privacy. After six real baseline completions, an eligible cheaper candidate starts at 10% canary, ramps to 30% with six recorded candidate outcomes, then to 100% with 12, unless the real-run failure/latency circuit breaker trips. Model completion is not answer correctness. This estimates token rates, not bills.
          </label>
          <p className="mt-2 text-xs text-muted">Operational feedback (local and durable): {recentOperationalOutcomes.length} terminal model runs in the last 14 days. Continuous cost-routing safety decisions are made on each subagent dispatch; only an actual run can feed this count. This is not a verified answer-quality score.</p>
          <p className="mt-1 text-xs text-muted">
            Real usage attached to {recentOperationalOutcomes.filter(row=>row.usage).length}/{recentOperationalOutcomes.length} terminal runs ·
            observed tokens {recentOperationalOutcomes.reduce((n,row)=>n+(row.usage?.promptTokens??0)+(row.usage?.completionTokens??0),0).toLocaleString()} ·
            configured-rate estimate {recentOperationalOutcomes.some(row=>row.meteredCostUsd!==undefined)?
              'USD '+recentOperationalOutcomes.reduce((n,row)=>n+(row.meteredCostUsd??0),0).toFixed(6)+' (partial when rates unknown)':
              'unavailable'} · NOT audited provider billing.
          </p>
          {benchmark.ranking.map(row=>{
            const recent=recentOperationalOutcomes.filter(item=>item.target===row.target);
            return <p key={row.target} className="mt-1 text-xs text-muted">{row.target}: {recent.length} observed · {recent.filter(item=>item.status==='error').length} failed · {recent.filter(item=>item.status==='done').length} finished</p>;
          })}
        </>}
      </div>
      <div className="rounded-lg border border-border bg-surface p-3">
        <h4 className="text-sm font-semibold text-foreground">Swarm missions · measured execution</h4>
        <p className="mt-1 text-xs text-muted">Read-only aggregation of persisted run journals. No invented token costs or estimated model invoices; missing measurements remain unknown. Review the separate process resource ledger for CPU/GPU and scheduler decisions.</p>
        <div className="mt-2 grid grid-cols-2 gap-2 text-xs text-foreground sm:grid-cols-4">
          <span>Runs: {mission.runs} ({mission.running} active)</span>
          <span>Success: {mission.successful} · failed: {mission.failed}</span>
          <span>Agents: {mission.agentRuns} · measured: {mission.measuredAgentRuns}</span>
          <span>Tokens: {mission.measuredTokens === null ? 'Unavailable' : mission.measuredTokens.toLocaleString()} (partial if not all measured)</span>
          <span>Provider-billed cost: unavailable</span>
          <span>Configured per-model rate estimate: {ratedSpend.estimatedUsd === null ? 'Unavailable' : 'USD '+ratedSpend.estimatedUsd.toFixed(6)}{ratedSpend.incomplete ? ' (partial)' : ''}</span>
          <span>Model-rate coverage: {ratedSpend.coveredAgents}/{ratedSpend.dispatchedAgents} new agents · {ratedSpend.measuredUnpricedAgents} measured without a price · {ratedSpend.unmeasuredAgents} without usage</span>
          <span>Observed token spend estimate: {observedCost?.observedUsd == null
            ? 'Unavailable' : '$'+observedCost.observedUsd.toFixed(6)}
            {observedCost?.incomplete ? ' (partial)' : ''}</span>
          <span>Usage coverage: {observedCost?.coverage == null
            ? 'Unavailable' : Math.round(observedCost.coverage*100)+'% of dispatched'}</span>
          <span>Failed agents: {mission.failedAgentRuns} · cancelled: {mission.cancelledAgentRuns}</span>
          <span>Checkpoint replays: {mission.reusedAgentRuns} (no fresh inference)</span>
          <span>Mean completed duration: {mission.meanDurationMs === null ? 'Unavailable' : Math.round(mission.meanDurationMs/1000)+' s'}</span>
          <span>Resident running: {mission.resident.running}</span>
          <span>Resident failed: {mission.resident.failed}</span>
        </div>
        <div className="mt-2 grid gap-2 text-xs sm:grid-cols-2">
          <label className="text-muted">Advisory token budget
            <input type="number" min={1} value={tokenBudget} onChange={e=>setTokenBudget(e.target.value)}
              className="mt-1 w-full rounded border border-border bg-background p-2 text-foreground"/>
          </label>
          <label className="text-muted">Estimated USD per million tokens (not billed cost)
            <input type="number" min={0} step="0.01" value={estimatedUsdPerMillion}
              onChange={e=>setEstimatedUsdPerMillion(e.target.value)}
              className="mt-1 w-full rounded border border-border bg-background p-2 text-foreground"/>
          </label>
        </div>
        <p className="mt-1 text-xs text-muted">
          Four-agent token forecast:
          {forecast?.projectedTokens == null ? 'Insufficient measured usage (minimum three agent samples)' :
            ' '+forecast.projectedTokens.toLocaleString()+' tokens'}
          {forecast?.projectedUsd != null ? ' · estimated $'+forecast.projectedUsd.toFixed(4) : ''}
          {forecast?.overTokenBudget ? ' · Above advisory token budget' : ''}
          {' '}· Observations: {forecast?.sampleAgents ?? 0}. Forecast only; not an enforced hard limit.
        </p>
        {mission.missions.slice(0,8).map(row=><details key={row.id} className="mt-2 rounded border border-border p-2 text-xs text-muted">
          <summary className="cursor-pointer">{row.name} · {row.status} · {row.topology} · {row.done}/{row.agents} succeeded · {row.failed} failed · {row.cancelled} cancelled · {Math.round(row.elapsedMs/1000)}s · {row.measuredAgents}/{row.agents-row.reused} dispatched usage samples · {row.tokens === null ? 'tokens unavailable' : row.tokens.toLocaleString()+' measured tokens'} · reused {row.reused}</summary>
          <p className="mt-1 text-xs text-faint">Initial model dispatch routing (not a provider invoice or verified completion)</p>
          {(() => {
            const cost = ratedSpend.missions.find(m => m.id === row.id);
            return <p>{cost === undefined || cost.estimatedUsd === null
              ? 'Per-model cost unavailable' : 'Configured-rate estimate USD '+cost.estimatedUsd.toFixed(6)}
              {' '}· priced {cost?.covered ?? 0}/{cost?.dispatched ?? 0} newly dispatched agents</p>;
          })()}
          {row.routing.length===0 ? <p>Model routing unavailable for this mission (older record, peer placement or checkpoint reuse).</p> :
            row.routing.map((entry,index)=><p key={index} className="mt-1 break-words">{entry.agent} → {entry.targetLabel} · {entry.policyName ?? 'default / pinned'} · {entry.reason}</p>)}
        </details>)}
      </div>
      <div className="rounded-lg border border-border bg-surface p-3">
        <h4 className="text-sm font-semibold text-foreground">Learning cycle · retrieve / judge / distill / consolidate</h4>
        <div className="mt-2 rounded-md border border-border p-2">
          <p className="text-xs font-medium text-foreground">Local MLX LoRA / QLoRA model import</p>
          <p className="mt-1 text-xs text-muted">After locally training, evaluating, and fusing a new adapter using the documented Apple Silicon runner, select its training-receipt.json. The configuration hash and held-out/regression quality gates are checked before native model registration. Registration does not switch or execute the model.</p>
          <Button size="sm" className="mt-2" disabled={fusionImportBusy} onClick={()=>void importValidatedMlxFusion()}>
            {fusionImportBusy?'Checking fused model…':'Review and import evaluated MLX fusion…'}
          </Button>
          {importedFusion&&<div className="mt-2">
            <p className="mb-1 text-xs text-muted">Verified candidate: {importedFusion.name}</p>
            <Button size="sm" disabled={fusionImportBusy||fusionActivationBusy}
              onClick={()=>void activateImportedMlxFusion()}>
              {fusionActivationBusy?'Running real model canary and rollback protection…':'Activate verified fusion · real inference + rollback'}
            </Button>
          </div>}
        </div>
        <p className="mt-1 text-xs text-muted">Only real benchmark outcomes with repeated independently graded source fixtures can stage a learning candidate. A candidate is not reusable until manually approved. Approved guidance enters future LOCAL read-only swarms only if you select historical learning. Nothing self-modifies model weights or bypasses permissions.</p>
        <div className="mt-2 flex flex-wrap gap-2">
          <Button size="sm" disabled={!selectedSpecialistQuality}
            onClick={()=>{
              if(!selectedSpecialistQuality)return;
              try{
                const candidate=distillLearningCandidate(selectedSpecialistQuality);
                setLearningRevision(n=>n+1);
                setNotice(candidate ? 'New candidate staged as pending: '+candidate.id : 'At least six independently correct, source-checked observations and >=80% accuracy are needed.');
              }catch(e){setError('Learning distillation refused: '+String(e));}
            }}>
            Distill candidate from selected verified domain
          </Button>
        </div>
        {learningCandidates.slice(-12).reverse().map(candidate=><div key={candidate.id+':'+candidate.revision} className="mt-2 border-t border-border pt-2 text-xs">
          <p className="text-foreground">{candidate.domain} / {candidate.specialist} · {candidate.status} v{candidate.revision}</p>
          <p className="text-faint">{candidate.observations} graded fixtures · {Math.round(candidate.accuracy*100)}% · {candidate.target}</p>
          {candidate.status==='pending' && <div className="mt-1 flex flex-wrap gap-2">
            <Button size="sm" onClick={()=>{
              if(!window.confirm('Consolidate this previously independently graded reviewer choice into approved historical guidance? Source correctness still needs checking on each new repository.'))return;
              try{decideLearningCandidate(candidate.id,candidate.revision,'approved');setLearningRevision(n=>n+1);}catch(e){setError(String(e));}
            }}>Approve learning</Button>
            <Button size="sm" onClick={()=>{
              try{decideLearningCandidate(candidate.id,candidate.revision,'rejected');setLearningRevision(n=>n+1);}catch(e){setError(String(e));}
            }}>Reject</Button>
          </div>}
          <Button size="sm" className="mt-1" onClick={()=>{
            if(!window.confirm('Forget all locally retained revisions of this learning?'))return;
            try{forgetLearningPattern(candidate.id);setLearningRevision(n=>n+1);}catch(e){setError(String(e));}
          }}>Forget pattern</Button>
        </div>)}
      </div>
      <div className="rounded-lg border border-border bg-surface p-3">
        <h4 className="text-sm font-semibold text-foreground">Verified specialist quality · all five domains</h4>
        <p className="mt-1 text-xs text-muted">Independent real-model source tests cover security, frontend, backend, performance and general reviewers. Each domain uses two defective executable source fixtures and a correct control, tests unchanged Git source, checks real run receipts and recomputes scores. Ordinary task completion is NOT an accuracy measure. Import a domain-specific result from <code>pnpm test:swarm-specialists:live -- --domain DOMAIN --target ollama:MODEL</code>, review it, and opt in separately.</p>
        <label className="mt-3 flex items-center gap-2 text-xs text-foreground">
          Domain
          <select className="rounded border border-border bg-surface px-2 py-1" value={specialistQualityDomain}
            onChange={event => {
              const domain = event.target.value as VerifiedSpecialistDomain;
              setSpecialistQualityDomain(domain);
              setSpecialistQualityRouting(verifiedSpecialistRoutingEnabled(domain));
            }}>
            {SPECIALIST_DOMAINS.map(domain => <option key={domain} value={domain}>{domain}</option>)}
          </select>
        </label>
        <div className="mt-3 flex flex-wrap gap-2">
          <Button size="sm" disabled={benchmarkBusy} onClick={() => void importSpecialistQuality()}>Import independent fixture results…</Button>
          {selectedSpecialistQuality && <Button size="sm" disabled={benchmarkBusy} onClick={removeSpecialistQuality}>Clear this domain</Button>}
        </div>
        <p className="mt-2 text-xs text-muted">Imported: {specialistQuality.length ? specialistQuality.map(row => row.domain).join(', ') : 'none'} · Each imported domain expires after 90 days.</p>
        {selectedSpecialistQuality ? <>
          <p className="mt-2 text-xs text-muted">
            {selectedSpecialistQuality.domain} · {selectedSpecialistQuality.target} · Generated {selectedSpecialistQuality.generatedAt}
          </p>
          {selectedSpecialistQuality.ranking.map(row => <p key={row.specialist} className="mt-1 text-xs text-foreground">
            {row.specialist}: {row.passes}/{row.attempts} independently checked fixtures ({Math.round(100*row.accuracy)}%)
          </p>)}
          <label className="mt-3 flex items-start gap-2 text-xs text-foreground">
            <input type="checkbox" checked={specialistQualityRouting}
              onChange={event => toggleSpecialistQuality(event.target.checked)} />
            Opt in to accuracy-based supplemental reviewer selection for {specialistQualityDomain} only. Existing specialist pools and all tool permissions remain unchanged.
          </label>
        </> : <p className="mt-2 text-xs text-faint">No measured accuracy for {specialistQualityDomain}. Execution reliability remains separate.</p>}
      </div>
      <div className="rounded-lg border border-border bg-surface p-3">
        <h4 className="text-sm font-semibold text-foreground">Versioned notebook · {notes.length} claims</h4>
        <p className="mt-1 text-xs text-muted">Only explicitly approved revisions enter future local swarm prompts. Model suggestions remain pending. Their source paths still need verification against the current repository. To publish one, select a Knowledge embedding stack and monotonic vector revision in the Replicated vector memory section, then explicitly approve its export.</p>
        {notes.length === 0 && <p className="mt-2 text-xs text-faint">No approved or pending notebook claims in this project.</p>}
        {notes.slice(0, 30).map(note => {
          const pending = note.revisions.filter(revision => revision.status === 'pending');
          return <div key={note.id} className="mt-3 border-t border-border pt-2 text-xs text-foreground">
            <div className="font-medium break-all">{note.id}</div>
            <p className="text-faint">Active version: {note.activeVersion ?? 'none'} · {note.revisions.length} retained revisions</p>
            {note.activeVersion !== null && <Button size="sm" className="mt-2" disabled={memoryBusy}
              onClick={() => void publishApprovedVector(note.id)}>
              Embed approved revision into shared vector namespace…
            </Button>}

            {pending.map(revision => <div key={revision.version} className="mt-1 rounded-md border border-border p-2">
              <p>Pending revision {revision.version} · {revision.reviewers.length} independent reviewers</p>
              <p className="mt-1 break-all text-faint">{revision.evidence.join(' · ')}</p>
              <div className="mt-2 flex gap-2">
                <Button size="sm" onClick={() => decide(note.id, revision.version, 'approved')}>Approve</Button>
                <Button size="sm" onClick={() => decide(note.id, revision.version, 'rejected')}>Reject</Button>
              </div>
            </div>)}
            {note.revisions.filter(revision => revision.status === 'approved' && revision.version !== note.activeVersion).map(revision =>
              <Button key={revision.version} className="mt-2" size="sm" onClick={() => rollback(note.id, revision.version)}>Restore approved v{revision.version}</Button>)}
          </div>;
        })}
      </div>
      <div className="rounded-lg border border-border bg-surface p-3">
        <h4 className="text-sm font-semibold text-foreground">Native capability packs</h4>
        <p className="mt-1 text-xs text-muted">Versioned first-party workflow packs for security, frontend, API resilience, performance, knowledge evidence, integrations and release readiness. No external installation, shell script, MCP grant or hook is enabled by choosing a pack; review the draft in chat before sending.</p>
        <label className="mt-2 flex items-center gap-2 text-xs text-foreground">
          Pack
          <select aria-label="Native swarm capability pack" value={capabilityPack}
            className="rounded border border-border bg-background px-2 py-1"
            onChange={event=>setCapabilityPack(event.target.value as SwarmPresetName)}>
            {SWARM_PRESET_NAMES.map(name=><option key={name} value={name}>{name}</option>)}
          </select>
        </label>
        <Button size="sm" className="mt-2" onClick={prepareCapabilityPack}>Prepare reviewed pack in chat</Button>
      </div>
      <div className="rounded-lg border border-border bg-surface p-3">
        <h4 className="text-sm font-semibold text-foreground">Reviewed external read-only packs</h4>
        <p className="mt-1 text-xs text-muted">Import versioned JSON-only swarm recipes (up to 8 packs, 12 agents each). Arbitrary code, remote peers, write agents, hooks and installers are not accepted. Imported prompts remain untrusted, and preparing a pack only drafts a normal permission-checked workflow.</p>
        <Button size="sm" className="mt-2" disabled={packBusy} onClick={() => void importReviewedPack()}>
          {packBusy?'Validating pack…':'Review and import pack JSON'}
        </Button>
        {reviewedPacks.map(pack=><div key={pack.id} className="mt-2 border-t border-border pt-2">
          <p className="text-xs font-medium text-foreground">{pack.title} · {pack.phases.length} phases</p>
          <p className="text-xs text-muted">{pack.description}</p>
          <div className="mt-1 flex gap-2">
            <Button size="sm" onClick={()=>prepareReviewedPack(pack)}>Prepare in chat</Button>
            <Button size="sm" onClick={()=>{
              if(!window.confirm('Remove this local read-only pack?'))return;
              try{removeReviewedSwarmPack(pack.id);setReviewedPacks(listReviewedSwarmPacks());}
              catch(e){setError(String(e));}
            }}>Remove</Button>
          </div>
        </div>)}
      </div>
      <div className="rounded-lg border border-border bg-surface p-3">
        <h4 className="text-sm font-semibold text-foreground">MCP plugin interoperability</h4>
        <div className="mt-3 rounded-md border border-border p-3">
          <p className="text-xs font-medium text-foreground">External Ruflo MCP compatibility (operator reviewed)</p>
          <p className="mt-1 text-xs text-muted">Registers a pinned npx Ruflo MCP stdio server but does NOT launch it, install host mods, or grant any tools. You must review its package, configure a narrow explicit tool allowlist, enable and connect from MCP Settings. Not a claim of native Ruflo mod/hook parity.</p>
          <div className="mt-2 flex flex-wrap gap-2">
            <input aria-label="Pinned Ruflo version" value={rufloVersion} onChange={event=>setRufloVersion(event.target.value)}
              placeholder="Exact release, e.g. 3.6.0"
              className="min-w-40 flex-1 rounded border border-border bg-background px-2 py-1 text-xs text-foreground" />
            <Button size="sm" disabled={pluginBusy!==null || mcpServers.some(server=>server.id==='ruflo-operator-mcp')}
              onClick={()=>void stageRufloMcp()}>
              {mcpServers.some(server=>server.id==='ruflo-operator-mcp') ? 'Registered in MCP Settings' : 'Stage disabled Ruflo MCP'}
            </Button>
          </div>
        </div>
        <p className="mt-1 text-xs text-muted">Lifecycle controls for operator-configured MCP servers. External servers expose tools only through explicit approvals and do not expand agent permissions automatically. Review each server's allowlist under MCP settings; external stdio processes are outside the native sandbox.</p>
        <div className="mt-2 flex flex-wrap justify-end gap-2">
          <Button size="sm" onClick={() => void refreshPlugins()}><RefreshCw size={13}/> Refresh connections</Button>
        </div>
        <div className="mt-3 border-t border-border pt-2">
          <p className="text-xs text-muted">Import compatible command-hook manifests manually. Only four supported Little Monkey lifecycle events are mapped; unknown events and hook kinds are refused. A confirmation displays executable commands before enabling them.</p>
          <Button size="sm" className="mt-2" disabled={hookBusy} onClick={() => void importHooks()}>
            {hookBusy ? 'Reading hook manifest…' : 'Review and import hooks.json…'}
          </Button>
        </div>
        {mcpServers.length === 0 && <p className="mt-2 text-xs text-faint">No MCP servers are configured. Add one under Settings → Connectors → MCP.</p>}
        {mcpServers.map(server => <div key={server.id} className="mt-3 flex flex-wrap items-center justify-between gap-2 border-t border-border pt-2">
          <div className="text-xs">
            <p className="font-medium text-foreground">{server.label} <span className="text-faint">({server.status})</span></p>
            <p className="text-faint">{server.tools.length} discovered tools · {server.toolAllowlist?.length ?? 'no'} allowlist entries · {server.transport.type}</p>
          </div>
          <div className="flex flex-wrap gap-2">
            <Button size="sm" disabled={pluginBusy !== null} onClick={() => void operatePlugin(server.id, server.enabled ? 'disable' : 'enable')}>{server.enabled ? 'Disable' : 'Enable'}</Button>
            {server.status === 'connected'
              ? <Button size="sm" disabled={pluginBusy !== null} onClick={() => void operatePlugin(server.id, 'disconnect')}>Disconnect</Button>
              : <Button size="sm" disabled={pluginBusy !== null || !server.enabled} onClick={() => void operatePlugin(server.id, 'connect')}>Connect</Button>}
          </div>
        </div>)}
      </div>
      <div className="rounded-lg border border-border bg-surface p-3">
        <h4 className="text-sm font-semibold text-foreground">Replicated vector memory</h4>
        <p className="mt-1 text-xs leading-5 text-muted">Opt-in, signed cross-installation vector records with per-namespace access control, embedding-model pinning, independent author revisions and tombstones. The two installations must already be paired. Sync manually or opt in to daemon-owned 60-second replication; vectors must be produced by the same embedding model on each node.</p>
        <div className="mt-2 grid gap-2 sm:grid-cols-2">
          <label className="text-xs text-muted">Namespace<input className="mt-1 w-full rounded border border-border bg-background p-2 text-foreground" value={memoryNamespace} onChange={e => setMemoryNamespace(e.target.value)} /></label>
          <label className="text-xs text-muted">Knowledge 2.0 stack ID (for automatic embeddings)<input className="mt-1 w-full rounded border border-border bg-background p-2 text-foreground" value={memoryStack} onChange={e => setMemoryStack(e.target.value)} placeholder="Existing configured indexed stack ID" /></label>
          <label className="text-xs text-muted">Embedding model identity<input className="mt-1 w-full rounded border border-border bg-background p-2 text-foreground" value={memoryModel} onChange={e => setMemoryModel(e.target.value)} /></label>
          <label className="text-xs text-muted">Inbound peer device ID<input className="mt-1 w-full rounded border border-border bg-background p-2 text-foreground" value={memoryDevice} onChange={e => setMemoryDevice(e.target.value)} /></label>
          <label className="text-xs text-muted">Outbound paired alias<input className="mt-1 w-full rounded border border-border bg-background p-2 text-foreground" value={memoryAlias} onChange={e => setMemoryAlias(e.target.value)} /></label>
        </div>
        <div className="mt-2 flex flex-wrap items-center gap-3 text-xs text-muted">
          <label><input type="checkbox" checked={memoryRead} onChange={e => setMemoryRead(e.target.checked)} /> Allow peer read</label>
          <label><input type="checkbox" checked={memoryWrite} onChange={e => setMemoryWrite(e.target.checked)} /> Allow peer write</label>
          <Button size="sm" disabled={memoryBusy || !memoryDevice.trim()} onClick={() => void memoryAction('share')}>Apply namespace ACL</Button>
          <Button size="sm" disabled={memoryBusy || !memoryAlias.trim()} onClick={() => void memoryAction('sync')}>Sync paired vectors</Button>
          <Button size="sm" disabled={memoryBusy || !memoryAlias.trim()} onClick={() => void memoryAction('watch')}>Enable auto-sync</Button>
          <Button size="sm" disabled={memoryBusy || !memoryAlias.trim()} onClick={() => void memoryAction('unwatch')}>Disable auto-sync</Button>
        </div>
        <div className="mt-3 grid gap-2 sm:grid-cols-2">
          <label className="text-xs text-muted">Document ID<input className="mt-1 w-full rounded border border-border bg-background p-2 text-foreground" value={memoryRecord} onChange={e => setMemoryRecord(e.target.value)} /></label>
          <label className="text-xs text-muted">Monotonic revision<input className="mt-1 w-full rounded border border-border bg-background p-2 text-foreground" type="number" min={1} value={memoryRevision} onChange={e => setMemoryRevision(e.target.value)} /></label>
          <label className="text-xs text-muted sm:col-span-2">Embedding vector (comma-separated floats)<input className="mt-1 w-full rounded border border-border bg-background p-2 text-foreground" value={memoryVector} onChange={e => setMemoryVector(e.target.value)} placeholder="0.25,0.5,0.75" /></label>
          <label className="text-xs text-muted sm:col-span-2">Document text<textarea rows={2} maxLength={2048} className="mt-1 w-full rounded border border-border bg-background p-2 text-foreground" value={memoryText} onChange={e => setMemoryText(e.target.value)} /></label>
        </div>
        <div className="mt-2 flex flex-wrap gap-2">
          <Button size="sm" disabled={memoryBusy || !memoryStack.trim() || !memoryText.trim()} onClick={() => void memoryAction('embed-write')}>Embed & save text</Button>
          <Button size="sm" disabled={memoryBusy || !memoryStack.trim() || !memoryText.trim()} onClick={() => void memoryAction('embed-search')}>Semantic search text</Button>
          <Button size="sm" disabled={memoryBusy || !memoryText.trim()} onClick={() => void memoryAction('write')}>Save local vector revision</Button>
          <Button size="sm" disabled={memoryBusy} onClick={() => void memoryAction('search')}>Search vectors</Button>
        </div>
        {memoryMatches.map((result,index) => <p key={index} className="mt-1 break-words text-xs text-muted">
          {result.entry.author}/{result.entry.id}: {result.score.toFixed(3)} — {result.entry.text.slice(0,200)}
        </p>)}
      </div>
      <div className="rounded-lg border border-border bg-surface p-3">
        <h4 className="text-sm font-semibold text-foreground">Resident autonomous recipe graphs</h4>
        <div className="mt-3 rounded border border-border p-2">
          <p className="text-xs font-medium text-foreground">Signed multi-node leader quorum</p>
          <p className="mt-1 text-xs text-muted">Requires independently installed, TLS-pinned, task-granted peers. Enroll a reviewed roster on each machine. Only a current local signed strict-majority lease authorizes new clustered read-only DAG dispatch. Network failure and expired votes do not grant leadership.</p>
          <label className="mt-2 block text-xs text-muted">Cluster group
            <input aria-label="Signed cluster group" value={clusterGroup}
              onChange={event=>{setClusterGroup(event.target.value);setClusterStatus(null);}}
              maxLength={80} placeholder="research-cluster"
              className="mt-1 w-full rounded border border-border bg-background p-2 text-foreground"/>
          </label>
          <div className="mt-2 flex flex-wrap gap-2">
            <Button size="sm" disabled={clusterBusy} onClick={()=>void signedClusterAction('setup')}>Enroll reviewed roster JSON…</Button>
            <Button size="sm" disabled={clusterBusy||!clusterGroup.trim()} onClick={()=>void signedClusterAction('status')}>Refresh native lease status</Button>
            <Button size="sm" disabled={clusterBusy||!clusterGroup.trim()} onClick={()=>void signedClusterAction('elect')}>Request signed leader election…</Button>
          </div>
          {clusterStatus&&<div className="mt-2 space-y-1 text-xs text-foreground">
            <p>Group: {clusterStatus.group} · Members: {clusterStatus.configured_members}</p>
            <p className="break-all">Roster digest: {clusterStatus.roster_sha256}</p>
            <p>Local dispatch authorized by a CURRENT quorum: {clusterStatus.may_schedule?'yes':'no'}</p>
            <p>Term: {clusterStatus.lease?.term??'none'} · Leader: {clusterStatus.lease?.leader?.slice(0,16)??'none'} · Expires: {clusterStatus.lease?new Date(clusterStatus.lease.expires_at_ms).toLocaleString():'no lease'}</p>
          </div>}
        </div>
        <p className="mt-1 text-xs leading-5 text-muted">Durable DAGs run under the installed OS daemon and survive app restarts. Each graph node uses a frozen, operator-selected local recipe with manual/plan tool permissions. This does not automatically resume write-capable chat agents.</p>
        {mode==='analyze' && <label className="mt-2 flex gap-2 text-xs text-foreground">
          <input type="checkbox" checked={residentFallbackEnabled} onChange={event=>setResidentFallbackEnabled(event.target.checked)}/>
          Select a separate, locally installed PLAN-mode model recipe as durable read-only root-agent fallback. Only a failed job can trigger it once; cancellations and write-capable jobs do not retry.
        </label>}
        <div className="mt-3 flex flex-wrap gap-2">
          <Button size="sm" variant="primary" disabled={residentBusy}
            onClick={() => void submitResident()}>Import and register JSON graph…</Button>
          <Button size="sm" disabled={residentBusy || !config.spec}
            onClick={() => void submitGoalToResident()}>
            Run {mode === 'implement' ? 'manually approved implementation' : 'read-only goal'} in resident daemon…
          </Button>
          <Button size="sm" disabled={residentBusy} onClick={() => void refreshResident()}><RefreshCw size={13}/> Refresh graphs</Button>
        </div>
        {resident.length === 0 && <p className="mt-3 text-xs text-faint">No resident swarm graphs recorded.</p>}
        {resident.map(graph => <div key={graph.id} className="mt-3 border-t border-border pt-2">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <p className="break-all text-xs font-medium">{graph.id} · {graph.state}</p>
            {(graph.state === 'running' || graph.state === 'failing') &&
              <Button size="sm" disabled={residentBusy} onClick={() => void cancelResident(graph.id)}>Request cancellation</Button>}
          </div>
          {!!clusterGroup.trim() && <Button size="sm" disabled={residentBusy || clusterBusy}
            onClick={() => void (async () => {
              try {
                const result=await daemonSwarmReplicaStatus(clusterGroup.trim(),graph.id);
                setReplicaView({graph:graph.id,records:result.replicas});
              } catch (failure) {setError('DAG peer replicas: '+String(failure));}
            })()}>
            View authenticated peer checkpoints
          </Button>}
          {replicaView?.graph===graph.id && <div className="mt-2 text-xs text-muted">
            <p>Received peer observations: {replicaView.records.length} · NOT authorized local jobs</p>
            {replicaView.records.map(peer => <p key={peer.source_fingerprint} className="break-all">
              Peer certificate {peer.source_fingerprint.slice(0,16)}… · term {peer.term} · revision {peer.sequence} · {peer.graph_state} · {peer.nodes.length} nodes
            </p>)}
            {distributedTrace && <details className="mt-2">
              <summary className="cursor-pointer">Combined local and peer trace · {distributedTrace.events.length} events</summary>
              <p>Fresh peer checkpoints: {distributedTrace.freshPeers} · Stale: {distributedTrace.stalePeers} · Invalid: {distributedTrace.invalidPeerRecords} · Original remote journal events: {distributedTrace.historicalPeerEvents} · Legacy snapshot-only peers: {distributedTrace.legacySnapshotPeers}</p>
              <p>Verified source-run metering: {distributedTrace.meteredPeerRuns} completed peer runs; token counts are from the original host's intact run ledger, and reported amounts are NOT provider invoices or independent answer-quality grades.</p>
              {distributedTrace.meteredUsage.map(row=><p key={row.sourceFingerprint+':'+row.node} className="break-all">
                Peer {row.sourceFingerprint.slice(0,12)}… · {row.node} · {row.status} · target {row.targetId} · input {row.inputTokens.toLocaleString()} · output {row.outputTokens.toLocaleString()} · cache {row.cachedInputTokens.toLocaleString()} · calls {row.modelCalls} · elapsed {row.elapsedMs} ms · {row.meteredCostMicros===null ? 'cost unavailable' : 'metered micro-USD '+row.meteredCostMicros.toLocaleString()}
              </p>)}
              <div className="max-h-60 space-y-1 overflow-auto" aria-label="Authenticated distributed DAG observations">
                {distributedTrace.events.slice(-40).map((entry,index)=><p key={entry.source+':'+entry.sequence+':'+entry.node+':'+index}>
                  {entry.source==='local-ledger'?'Local journal':'Signed peer report'} · {entry.node} · {entry.kind} · {entry.state} · #{entry.sequence} · {new Date(entry.occurredAt).toLocaleString()}
                  {entry.term!==null?' · term '+entry.term:''}
                </p>)}
              </div>
              <p>Remote completion reports are observations, never independent model-output verification or execution authority.</p>
            </details>}
          </div>}
          <p className="mt-1 text-xs text-faint">{graph.tasks.filter(t => t.state === 'succeeded').length}/{graph.tasks.length} nodes succeeded</p>
          {graph.tasks.map(task => <p key={task.id} className="mt-1 text-xs text-muted">
            {task.id} · {task.state} {task.error ? '— ' + task.error.slice(0, 160) : ''}
          </p>)}
          {!!graph.events?.length && <details className="mt-2 text-xs text-muted">
            <summary className="cursor-pointer">Durable transition trace · {graph.events.length} recent events</summary>
            <div className="mt-1 max-h-56 space-y-1 overflow-auto" aria-label="Resident swarm transition trace">
              {graph.events.slice(-30).map(event => <p key={event.sequence} className="break-all">
                #{event.sequence} · {event.node_id ?? 'graph'} · {event.previous_state ?? 'new'} → {event.state}
                {event.job_id ? ' · ' + event.job_id.slice(0, 48) : ''}
              </p>)}
            </div>
          </details>}
        </div>)}
      </div>
      <div className="rounded-lg border border-border bg-surface p-3">
        <h4 className="text-sm font-semibold text-foreground">Swarm runs</h4>
        <p className="mt-1 text-xs text-muted">Live activity and durable session checkpoints. Interrupted runs require an explicit resume request in the same conversation.</p>
        {running.length === 0 && restored.length === 0 && <p className="mt-2 text-xs text-faint">No swarm runs recorded.</p>}
        {running.map(run => <p key={run.runId} className="mt-2 text-xs text-foreground">
          <Activity className="mr-1 inline" size={13}/> {run.name} — running phase {run.activePhaseIndex + 1}/{run.phases.length}
        </p>)}
        {restored.map(run => <div key={run.sessionId + run.id} className="mt-2 border-t border-border pt-2 text-xs text-foreground">
          <CheckCircle2 className="mr-1 inline" size={13}/> {run.name} — {run.status} · {run.completed}/{run.total} agents
          {(run.status === 'error' || run.status === 'cancelled') && run.recoverable &&
            <Button size="sm" onClick={() => prepareResume(run.sessionId, run.id)}>
              Prepare safe resume in chat
            </Button>}
          {(run.status === 'error' || run.status === 'cancelled') && !run.recoverable &&
            <p className="text-faint">This older run has no saved recovery specification; it cannot be automatically reconstructed.</p>}
          <details className="mt-2 rounded-md border border-border p-2">
            <summary className="cursor-pointer text-xs font-medium text-foreground">
              Execution trace · {run.phases.length} phases · {Math.round(run.elapsedMs / 1000)} s recorded
            </summary>
            <div className="mt-2 space-y-2">
              {run.phases.map((phase, phaseIndex) => <section key={phaseIndex}
                  aria-label={'Phase ' + (phaseIndex + 1) + ': ' + phase.title}>
                <p className="text-xs font-medium text-foreground">{phaseIndex + 1}. {phase.title}</p>
                {phase.agents.map(agent => {
                  const recorded = run.resultJournal[agent.taskId];
                  const agentState = recorded?.status ?? 'not completed';
                  return <div key={agent.taskId} className="mt-1 ml-3 text-xs text-muted">
                    <span className="font-medium">{agent.description}</span>
                    {' — '}{agentState}{recorded?.reused ? ' (reused verified prompt journal)' : ''}
                    {recorded?.report && <details className="mt-1">
                      <summary className="cursor-pointer">Show saved report (untrusted)</summary>
                      <pre className="mt-1 max-h-48 overflow-auto whitespace-pre-wrap break-words rounded bg-background p-2 text-xs">{recorded.report.slice(0, 1_200)}</pre>
                    </details>}
                  </div>;
                })}
              </section>)}
            </div>
          </details>
          {run.status === 'done' && run.sourceRunId && <Button size="sm"
            disabled={learningBusy !== null}
            onClick={() => void proposeLearningCandidate(run.sourceRunId!, run.description)}>
            {learningBusy === run.sourceRunId ? 'Checking verification…' : 'Check verified learning eligibility'}
          </Button>}
        </div>)}
      </div>
    </div>
  );
}
