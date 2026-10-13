//! Operator-controlled portable recovery for frozen read-only resident swarms.
//!
//! A bundle is NOT a peer-signed certificate and never carries authority to
//! trust source success or access a workspace. On import every node is freshly
//! pending, every recipe is rewritten to an explicit local workspace, and the
//! normal daemon submit path revalidates all permissions and cluster grants.
//! A different graph id prevents reuse of local job receipts. This is a safe
//! re-execution recovery bridge, NOT transparent replicated-log failover.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::store::{DaemonPaths, DaemonStore};
use super::swarm_coordinator::{self, ResidentSwarmSpec};

const MAX_BUNDLE_BYTES: u64 = 34 * 1024 * 1024;
const MAX_RECIPE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenRecipe {
    node_id: String,
    primary: String,
    primary_sha256: String,
    fallback: Option<String>,
    fallback_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservedNode {
    node_id: String,
    state: String,
    job_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransferBundle {
    version: u32,
    source_graph_id: String,
    spec: ResidentSwarmSpec,
    recipes: Vec<FrozenRecipe>,
    /// Diagnostic ONLY; unverified remote completions must not become local
    /// completed tasks or authorize downstream dependency reports.
    observed_nodes: Vec<ObservedNode>,
    contents_sha256: String,
}

fn digest(bundle: &TransferBundle) -> Result<String, String> {
    let bytes = serde_json::to_vec(&(
        bundle.version, &bundle.source_graph_id, &bundle.spec,
        &bundle.recipes, &bundle.observed_nodes,
    )).map_err(|e| e.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn check_regular(path: &Path, ceiling: u64) -> Result<(), String> {
    let info = fs::symlink_metadata(path).map_err(|e| format!("File unavailable: {e}"))?;
    if !info.file_type().is_file() || info.len() > ceiling {
        return Err("Transfer requires a regular, bounded, non-symlink local file".into());
    }
    Ok(())
}
fn read_frozen_recipe(path: &str) -> Result<String, String> {
    let path = Path::new(path);
    check_regular(path, MAX_RECIPE_BYTES)?;
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    let parsed: little_monkey_lib::recipes::Recipe =
        serde_json::from_slice(&bytes).map_err(|e|format!("Invalid frozen recipe: {e}"))?;
    if parsed.permission_mode != "plan" {
        return Err("Only plan-mode, read-only recipes may leave a host".into());
    }
    String::from_utf8(bytes).map_err(|e|e.to_string())
}
fn put_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)
        .map_err(|e|e.to_string())?;
    file.write_all(bytes).map_err(|e|e.to_string())?;
    file.sync_all().map_err(|e|e.to_string())?;
    super::store::restrict_file(path)
}
fn atomic_create(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() as u64 > MAX_BUNDLE_BYTES {
        return Err("Transfer bundle exceeds 34 MiB".into());
    }
    let name = format!("{}.{}.tmp",path.file_name().and_then(|x|x.to_str())
        .ok_or("Invalid transfer destination")?,uuid::Uuid::new_v4());
    let temporary = path.with_file_name(name);
    let result = (|| {
        put_private(&temporary,bytes)?;
        // Never silently replace an existing operator-owned evidence bundle.
        fs::hard_link(&temporary,path).map_err(|e|format!(
            "Transfer destination already exists or cannot be linked: {e}"
        ))?;
        Ok(())
    })();
    let _=fs::remove_file(&temporary);
    result
}

/// Export frozen recipes and observational cursor data from the local daemon.
/// No model keys, vector contents, or full agent transcripts are exported.
pub fn export(paths: &DaemonPaths, graph_id: &str, output: &Path) -> Result<serde_json::Value, String> {
    let _=swarm_coordinator::status(paths,Some(graph_id))?;
    let store = DaemonStore::open(paths)?;
    let raw: Option<String> = store.connection.query_row(
        "SELECT spec_json FROM resident_swarm_graphs WHERE graph_id=?1",
        [graph_id], |row|row.get(0),
    ).optional().map_err(|e|e.to_string())?;
    let mut spec: ResidentSwarmSpec = serde_json::from_str(
        &raw.ok_or("Unknown resident swarm graph")?
    ).map_err(|e|e.to_string())?;
    swarm_coordinator::validate(&spec)?;
    let mut recipes=Vec::new();
    for node in &mut spec.nodes {
        let primary=read_frozen_recipe(&node.recipe)?;
        let primary_sha256=hash_bytes(primary.as_bytes());
        let fallback=node.fallback_recipe.as_deref().map(read_frozen_recipe).transpose()?;
        let fallback_sha256=fallback.as_ref().map(|bytes|hash_bytes(bytes.as_bytes()));
        recipes.push(FrozenRecipe{
            node_id:node.id.clone(),primary,primary_sha256,fallback,fallback_sha256
        });
        // Suppress source-machine private filesystem paths from the exported
        // plan. Import reconstructs local paths, and never executes these.
        node.recipe=format!("embedded-primary:{}",node.id);
        node.fallback_recipe=node.fallback_recipe.as_ref().map(|_|format!(
            "embedded-fallback:{}",node.id
        ));
    }
    let status=swarm_coordinator::status(paths,Some(graph_id))?;
    let tasks=status["graphs"][0]["tasks"].as_array()
        .ok_or("Graph status unavailable")?;
    let mut observed_nodes=Vec::new();
    for task in tasks {
        let id=task["id"].as_str().ok_or("Malformed node status")?;
        let state=task["state"].as_str().ok_or("Malformed node status")?;
        let job=task["job_id"].as_str().map(str::to_owned);
        if state.len()>40||job.as_ref().is_some_and(|x|x.len()>120){
            return Err("Oversized node status".into());
        }
        observed_nodes.push(ObservedNode{
            node_id:id.to_owned(),state:state.to_owned(),job_id:job
        });
    }
    // Status returns nodes sorted by name, while the DAG specification may
    // use dependency order. Normalize observations to the spec's order.
    let mut observations=observed_nodes.into_iter().map(|row|
        (row.node_id.clone(),row)
    ).collect::<std::collections::BTreeMap<_,_>>();
    let observed_nodes=spec.nodes.iter().map(|node|
        observations.remove(&node.id).ok_or_else(||
            "Source node state is incomplete".to_string()
        )
    ).collect::<Result<Vec<_>,_>>()?;
    if !observations.is_empty() {
        return Err("Unexpected source node observation".into());
    }
    let mut bundle=TransferBundle{
        version:1,source_graph_id:spec.id.clone(),spec,recipes,observed_nodes,
        contents_sha256:String::new()
    };
    bundle.contents_sha256=digest(&bundle)?;
    let json=serde_json::to_vec_pretty(&bundle).map_err(|e|e.to_string())?;
    atomic_create(output,&json)?;
    Ok(serde_json::json!({
        "source_graph":graph_id,"nodes":bundle.recipes.len(),
        "sha256":bundle.contents_sha256,"output":output,
        "warning":"Not signed. Source states are observations, not reusable successes."
    }))
}
fn verify(bundle: &TransferBundle) -> Result<(), String> {
    if bundle.version!=1||bundle.spec.id!=bundle.source_graph_id
        ||bundle.contents_sha256!=digest(bundle)? {
        return Err("Transfer version, source id or contents checksum invalid".into());
    }
    swarm_coordinator::validate(&bundle.spec)?;
    if bundle.recipes.len()!=bundle.spec.nodes.len()
        ||bundle.observed_nodes.len()!=bundle.spec.nodes.len() {
        return Err("Transfer node evidence incomplete".into());
    }
    for (node,record) in bundle.spec.nodes.iter().zip(&bundle.recipes) {
        let expected_fallback=record.fallback.as_ref()
            .map(|_|format!("embedded-fallback:{}",node.id));
        if node.id!=record.node_id
            ||node.recipe!=format!("embedded-primary:{}",node.id)
            ||node.fallback_recipe.as_deref()!=expected_fallback.as_deref()
            ||hash_bytes(record.primary.as_bytes())!=record.primary_sha256
            ||record.fallback.as_ref().map(|s|hash_bytes(s.as_bytes()))!=record.fallback_sha256
            ||record.primary.len() as u64>MAX_RECIPE_BYTES
            ||record.fallback.as_ref().is_some_and(|s|s.len() as u64>MAX_RECIPE_BYTES) {
            return Err("Embedded frozen recipe and graph manifest disagree".into());
        }
    }
    for (node,observation) in bundle.spec.nodes.iter().zip(&bundle.observed_nodes) {
        if observation.node_id!=node.id
            ||observation.state.len()>40
            ||observation.job_id.as_ref().is_some_and(|id|id.len()>120) {
            return Err("Untrusted source observation is malformed".into());
        }
    }
    Ok(())
}
fn relocated_recipe(raw: &str, workspace: &Path) -> Result<Vec<u8>, String> {
    let mut recipe: little_monkey_lib::recipes::Recipe =
        serde_json::from_str(raw).map_err(|e|format!("Invalid embedded recipe: {e}"))?;
    if recipe.permission_mode!="plan" {
        return Err("Transferred recipe is not read-only plan mode".into());
    }
    recipe.workspace=Some(workspace.to_string_lossy().to_string());
    serde_json::to_vec_pretty(&recipe).map_err(|e|e.to_string())
}
/// Explicit import always creates a DIFFERENT graph with every node pending.
/// It never imports foreign successful jobs, votes, lease certificates,
/// credentials, permissions, or operation approvals.
pub fn import_read_only(
    paths: &DaemonPaths, input: &Path, new_id: &str, workspace: &Path,
) -> Result<serde_json::Value, String> {
    check_regular(input,MAX_BUNDLE_BYTES)?;
    let bundle: TransferBundle=serde_json::from_slice(
        &fs::read(input).map_err(|e|e.to_string())?
    ).map_err(|e|format!("Invalid transfer bundle: {e}"))?;
    verify(&bundle)?;
    if new_id==bundle.source_graph_id {
        return Err("Imported graph needs a new id to prohibit receipt reuse".into());
    }
    let workspace=workspace.canonicalize().map_err(|e|e.to_string())?;
    if !workspace.is_dir(){
        return Err("Destination workspace must exist as a directory".into());
    }
    let mut spec=bundle.spec;
    spec.id=new_id.to_string();
    swarm_coordinator::validate(&spec)?;
    paths.ensure()?;
    let stage=paths.snapshots.join(format!("swarm-transfer-{}",uuid::Uuid::new_v4()));
    fs::create_dir(&stage).map_err(|e|e.to_string())?;
    let result=(||{
        for (node,record) in spec.nodes.iter_mut().zip(&bundle.recipes) {
            let path=stage.join(format!("{}-primary.json",node.id));
            put_private(&path,&relocated_recipe(&record.primary,&workspace)?)?;
            node.recipe=path.to_string_lossy().to_string();
            if let Some(raw)=record.fallback.as_deref(){
                let path=stage.join(format!("{}-fallback.json",node.id));
                put_private(&path,&relocated_recipe(raw,&workspace)?)?;
                node.fallback_recipe=Some(path.to_string_lossy().to_string());
            }
        }
        let spec_path=stage.join("graph.json");
        put_private(&spec_path,&serde_json::to_vec(&spec).map_err(|e|e.to_string())?)?;
        swarm_coordinator::submit(paths,&spec_path)
    })();
    let _=fs::remove_dir_all(stage);
    result.map(|status|serde_json::json!({
        "restored_graph":new_id,"source_graph":bundle.source_graph_id,
        "status":status,
        "warning":"All nodes are newly pending. Source completions are untrusted; plan-only reexecution."
    }))
}
#[cfg(test)]
mod tests {
    use super::*;
    use super::super::swarm_coordinator::ResidentSwarmNode;

    fn setup(root:&Path)->(DaemonPaths,PathBuf){
        let paths=DaemonPaths::under(root);
        paths.ensure().unwrap();
        let workspace=root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        (paths,workspace)
    }
    #[test]
    fn portable_plan_only_graph_restarts_on_distinct_daemon_without_trusting_foreign_completions(){
        let base=std::env::temp_dir().join(format!("swarm-portability-{}",uuid::Uuid::new_v4()));
        let (source,source_ws)=setup(&base.join("source"));
        let (target,target_ws)=setup(&base.join("target"));
        let recipe=base.join("source/recipe.json");
        fs::write(&recipe,serde_json::json!({
            "version":1,"name":"read-only","target":{"ollama":"qwen2.5:7b"},
            "workspace":source_ws,"permission_mode":"plan",
            "prompt":"Inspect this disposable workspace","params":{}
        }).to_string()).unwrap();
        let spec=ResidentSwarmSpec{
            version:1,id:"original".into(),cluster_id:None,max_parallel:1,
            nodes:vec![ResidentSwarmNode{
                id:"survey".into(),recipe:recipe.to_string_lossy().to_string(),
                fallback_recipe:None,depends_on:vec![],
                include_dependency_reports:false,max_runtime_seconds:120,
            }]
        };
        let graph=base.join("source/graph.json");
        fs::write(&graph,serde_json::to_vec(&spec).unwrap()).unwrap();
        swarm_coordinator::submit(&source,&graph).unwrap();
        let bundle=base.join("transfer.json");
        export(&source,"original",&bundle).unwrap();
        assert!(export(&source,"original",&bundle).is_err());
        assert!(import_read_only(&target,&bundle,"original",&target_ws).is_err());
        let restored=import_read_only(&target,&bundle,"new-leader",&target_ws).unwrap();
        assert_eq!(restored["status"]["state"],"running");
        let state=swarm_coordinator::status(&target,Some("new-leader")).unwrap();
        assert_eq!(state["graphs"][0]["tasks"][0]["state"],"pending");
        assert!(state["graphs"][0]["tasks"][0]["job_id"].is_null());
        let frozen=target.snapshots.join("resident-swarm-new-leader-survey.json");
        let installed:little_monkey_lib::recipes::Recipe=
            serde_json::from_slice(&fs::read(frozen).unwrap()).unwrap();
        assert_eq!(installed.workspace.as_deref(),
            Some(target_ws.canonicalize().unwrap().to_string_lossy().as_ref()));
        assert_eq!(installed.permission_mode,"plan");
        let mut damaged:serde_json::Value=
            serde_json::from_slice(&fs::read(&bundle).unwrap()).unwrap();
        damaged["recipes"][0]["primary_sha256"]=serde_json::json!("00");
        let bad=base.join("bad.json");
        fs::write(&bad,serde_json::to_vec(&damaged).unwrap()).unwrap();
        assert!(import_read_only(&target,&bad,"tampered",&target_ws).is_err());
        assert!(swarm_coordinator::status(&target,Some("tampered")).unwrap()["graphs"]
            .as_array().unwrap().is_empty());
        let _=fs::remove_dir_all(&base);
    }
}
