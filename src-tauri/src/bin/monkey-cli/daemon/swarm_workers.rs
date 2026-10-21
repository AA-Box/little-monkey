//! Operator-installed, durable read-only swarm worker presets.
//!
//! Reuses the existing native cron/filesystem trigger delivery pipeline,
//! deterministic job IDs, kill switch, backpressure and supervision.
//! No worker is enabled by default, no model generates its own permissions,
//! and no worker may silently edit or deploy repository contents.
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use super::store::DaemonPaths;
use super::ledger::SharedLedger;
use super::trigger::{TriggerConfig, TriggerTarget};

pub const WORKERS: &[(&str,&str)] = &[
    ("ambient-discovery","Act as an opt-in read-only maintenance scout. Inspect only current workspace source and documentation. Propose at most five distinct next tasks with source paths, evidence, impact, uncertainty, and a safe verification plan. Explicitly say when no warranted work is found. Never edit, execute shell commands, expose secrets, enqueue work, use external messaging, or claim unrun tests passed."),
    ("security-review","Review source for concrete exploitable security flaws. Give file paths, evidence and suggested tests, never modify files."),
    ("test-gap-review","Identify high-impact untested behavior, missing negative tests and brittle assertions. Report exact source locations."),
    ("dependency-review","Examine dependency manifests and locks for maintainability, licensing and known-risk indicators. Do not fetch or install packages."),
    ("api-contract-review","Compare API interfaces with their callers and tests for incompatible changes, data validation and error semantics."),
    ("performance-review","Look for demonstrable hot paths, unnecessary allocations and latency risks, identifying measurements needed before optimization."),
    ("accessibility-review","Inspect UI source for keyboard navigation, semantic labels, focus and accessibility regressions. Propose but do not perform edits."),
    ("documentation-review","Compare documentation with actual source behavior and flag stale claims or missing operational instructions."),
    ("architecture-review","Inspect module boundaries, circular dependencies and unsafe ownership assumptions; report actionable engineering debt."),
    ("data-integrity-review","Inspect state persistence, migration, transactional boundaries and recovery paths for data loss or corruption risks."),
    ("configuration-review","Audit configuration defaults, authorization scopes, secret handling and insecure environment assumptions."),
    ("release-readiness","Identify missing release gates, migration considerations, rollback procedures and failed verification evidence."),
    ("code-quality-review","Locate correctness risks, duplication and unclear contracts that justify bounded refactoring and targeted tests."),
];

fn role_instruction(role:&str)->Result<&'static str,String>{
    WORKERS.iter().find(|(name,_)|*name==role).map(|(_,prompt)|*prompt)
        .ok_or_else(||"Unknown built-in swarm worker role".to_string())
}
fn valid_id(id:&str)->bool{
    (2..=40).contains(&id.len())
        && id.bytes().next().is_some_and(|c|c.is_ascii_lowercase())
        && id.bytes().all(|c|c.is_ascii_lowercase()||c.is_ascii_digit()||c==b'-')
}
fn build_recipe(
    source:&[u8], extension:&str, role:&str, id:&str, workspace:&Path,
)->Result<little_monkey_lib::recipes::Recipe,String>{
    let instruction=role_instruction(role)?;
    if !valid_id(id)||source.len()>1024*1024 {
        return Err("Worker id or source size outside allowed bounds".into());
    }
    let mut recipe=little_monkey_lib::recipes::parse_recipe(
        std::str::from_utf8(source).map_err(|_|"Recipe is not UTF-8")?,extension,
    )?;
    if recipe.permission_mode!="plan"
        ||recipe.params.values().any(Option::is_none)
        ||recipe.channel_send.is_some()
        ||recipe.desktop_turn.is_some()
        ||recipe.placed_run.is_some()
        ||recipe.autonomous_task.is_some() {
        return Err("Background workers require read-only plan recipes without elevated execution or channel grants".into());
    }
    if recipe.prompt.len()>64*1024||recipe.system.as_ref().is_some_and(|s|s.len()>64*1024) {
        return Err("Unbounded worker prompt or system instructions".into());
    }
    let local=workspace.canonicalize().map_err(|e|e.to_string())?;
    if !local.is_dir(){return Err("Worker workspace must be an existing directory".into());}
    recipe.name=format!("swarm-worker-{id}");
    recipe.workspace=Some(local.to_string_lossy().to_string());
    recipe.output=Default::default();
    // A scheduled job has no operator at the keyboard. Bound both wall time
    // and inference/tool turns independent of permissive source defaults.
    recipe.timeout_seconds=Some(recipe.timeout_seconds.unwrap_or(900).clamp(1,900));
    recipe.max_iterations=Some(recipe.max_iterations.unwrap_or(12).clamp(1,12));
    recipe.prompt=format!(
        "This is an operator-scheduled READ-ONLY evidence task. No file modifications, \
         network writes, deployment, external messaging or permission escalation.\n\
         Worker role ({role}): {instruction}\n\nOperator-selected task:\n{}",
        recipe.prompt,
    );
    // Keep all target selection, runtime and model credentials under the
    // already configured source recipe; do not invent a provider or grant.
    Ok(recipe)
}
pub fn install(
    paths:&DaemonPaths, id:&str, role:&str, source:&Path,
    workspace:&Path, cron:Option<&str>, watch:Option<&Path>,
)->Result<serde_json::Value,String>{
    if cron.is_some()==watch.is_some() {
        return Err("Choose exactly one cron schedule OR watched filesystem path".into());
    }
    if !valid_id(id){return Err("Invalid worker id".into());}
    let info=fs::symlink_metadata(source).map_err(|e|e.to_string())?;
    if !info.file_type().is_file()||info.len()>1024*1024 {
        return Err("Worker source recipe must be a regular local file under 1 MiB".into());
    }
    let ext=source.extension().and_then(|s|s.to_str()).unwrap_or("json");
    let recipe=build_recipe(&fs::read(source).map_err(|e|e.to_string())?,
        ext,role,id,workspace)?;
    paths.ensure()?;
    let target=paths.snapshots.join(format!("swarm-worker-{id}.json"));
    let trigger_id=format!("swarm-worker-{id}");
    let mut shared=SharedLedger::open(&paths.ledger_db)?;
    if shared.trigger(&trigger_id)?.is_some()||target.exists() {
        return Err("Worker id already registered; remove the old trigger and review the recipe before reinstalling".into());
    }
    let config=if let Some(schedule)=cron {
        TriggerConfig::Cron {
            target:TriggerTarget::Recipe {
                recipe:target.to_string_lossy().to_string(),
                params:BTreeMap::new(),payload_param:None,
            },
            workflow:None,schedule:schedule.to_string(),
        }
    }else{
        let watch=watch.expect("exactly one trigger kind");
        TriggerConfig::Filesystem {
            target:TriggerTarget::Recipe {
                recipe:target.to_string_lossy().to_string(),
                params:BTreeMap::new(),payload_param:None,
            },
            workflow:None,
            path:super::trigger::canonicalize_trigger_path(watch.to_path_buf())?,
            recursive:false,pattern:None,last_fingerprint:None,
        }
    };
    config.validate()?;
    let mut file=OpenOptions::new().write(true).create_new(true).open(&target)
        .map_err(|e|e.to_string())?;
    super::store::restrict_file(&target)?;
    let contents=serde_json::to_vec_pretty(&recipe).map_err(|e|e.to_string())?;
    if contents.len()>1024*1024 {
        drop(file);
        let _=fs::remove_file(&target);
        return Err("Frozen worker exceeds the 1 MiB recipe limit".into());
    }
    let persisted=(||{
        file.write_all(&contents).map_err(|e|e.to_string())?;
        file.sync_all().map_err(|e|e.to_string())?;
        super::add_trigger(paths,&mut shared,&trigger_id,config,None)
    })();
    if let Err(e)=persisted {
        drop(file);
        let _=fs::remove_file(&target);
        return Err(e);
    }
    Ok(serde_json::json!({
        "worker":trigger_id,"role":role,"recipe":target,
        "status":"scheduled","note":"Read-only plan recipe; normal daemon trigger supervision and approvals apply"
    }))
}
pub fn catalog()->serde_json::Value{
    serde_json::json!({"roles":WORKERS.iter().map(|(id,description)|
        serde_json::json!({"id":id,"description":description})).collect::<Vec<_>>()})
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn every_builtin_role_has_a_unique_bounded_read_only_instruction(){
        assert_eq!(WORKERS.len(),13);
        let mut seen=std::collections::BTreeSet::new();
        for (role,instruction) in WORKERS {
            assert!(seen.insert(*role));
            assert!(valid_id(role));
            assert!(!instruction.is_empty()&&instruction.len()<600);
        }
        assert!(role_instruction("run-shell").is_err());
        assert!(role_instruction("ambient-discovery").unwrap().contains("read-only"));
        assert!(!valid_id("../evil"));
    }
    #[test]
    fn scheduled_worker_reuses_durable_native_trigger_instead_of_a_side_loop(){
        let root=std::env::temp_dir().join(format!("swarm-worker-{}",uuid::Uuid::new_v4()));
        let paths=DaemonPaths::under(&root);
        paths.ensure().unwrap();
        let workspace=root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let input=root.join("source.json");
        fs::write(&input,serde_json::json!({
            "version":1,"name":"readonly","target":{"ollama":"qwen2.5:7b"},
            "workspace":workspace,"permission_mode":"plan",
            "prompt":"Inspect this test repository","params":{}
        }).to_string()).unwrap();
        let result=install(&paths,"scheduled-security","security-review",&input,
            &workspace,Some("*/5 * * * *"),None).unwrap();
        assert_eq!(result["status"],"scheduled");
        let shared=SharedLedger::open(&paths.ledger_db).unwrap();
        assert!(shared.trigger("swarm-worker-scheduled-security").unwrap().is_some());
        let stored=paths.snapshots.join("swarm-worker-scheduled-security.json");
        let loaded:little_monkey_lib::recipes::Recipe=
            serde_json::from_slice(&fs::read(&stored).unwrap()).unwrap();
        assert_eq!(loaded.permission_mode,"plan");
        // Discovery is a real native daemon worker role and uses the same
        // frozen recipe, trigger, bounded turn count and permission gate.
        let discovery = install(&paths,"ambient-review","ambient-discovery",&input,
            &workspace,Some("0 */6 * * *"),None).unwrap();
        assert_eq!(discovery["status"],"scheduled");
        let discovery_recipe: little_monkey_lib::recipes::Recipe =
            serde_json::from_slice(&fs::read(paths.snapshots.join("swarm-worker-ambient-review.json")).unwrap()).unwrap();
        assert_eq!(discovery_recipe.permission_mode,"plan");
        assert!(discovery_recipe.prompt.contains("ambient-discovery"));
        assert_eq!(loaded.max_iterations,Some(12));
        assert_eq!(loaded.timeout_seconds,Some(900));
        assert!(loaded.prompt.contains("security-review"));
        assert!(install(&paths,"scheduled-security","security-review",&input,
            &workspace,Some("*/5 * * * *"),None).is_err());
        let mut invalid:serde_json::Value=
            serde_json::from_slice(&fs::read(&input).unwrap()).unwrap();
        invalid["permission_mode"]=serde_json::json!("acceptEdits");
        let bad=root.join("bad.json");
        fs::write(&bad,serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(install(&paths,"unsafe-worker","security-review",&bad,
            &workspace,Some("*/5 * * * *"),None).is_err());
        let _=fs::remove_dir_all(root);
    }
}
