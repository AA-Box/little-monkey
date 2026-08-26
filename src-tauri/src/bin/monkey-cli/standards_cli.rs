//! Headless Standards Studio lifecycle for `monkey standards ...`.
//!
//! The portable repository document remains `.little-monkey/standards/index.json`.
//! This CLI never grants authority from repository content: discovered/imported
//! standards are guidance/verification metadata only and every discovery result
//! starts as an unapproved candidate.

use clap::{Subcommand, ValueEnum};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const RELATIVE_INDEX: &str = ".little-monkey/standards/index.json";
const DEFAULT_EXPORT: &str = ".little-monkey/standards/export.json";
const MAX_SCAN_FILES: usize = 300;
const MAX_SCAN_DEPTH: usize = 4;
const MAX_EVIDENCE_BYTES: u64 = 256 * 1024;
const MAX_SCAN_BYTES: u64 = 3 * 1024 * 1024;
const DEFAULT_PREVIEW_BUDGET: usize = 8_000;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum LifecycleStatus {
    Candidate,
    Approved,
    Rejected,
    Deprecated,
    Conflicting,
    Stale,
}

impl LifecycleStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Deprecated => "deprecated",
            Self::Conflicting => "conflicting",
            Self::Stale => "stale",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum StandardsCmd {
    /// Scan the actual repository and merge evidence-backed candidates.
    Discover {
        #[arg(long)]
        json: bool,
        /// Inspect without writing `.little-monkey/standards/index.json`.
        #[arg(long)]
        dry_run: bool,
    },
    /// List standards, optionally filtered by lifecycle state.
    List {
        #[arg(long, value_enum)]
        status: Option<LifecycleStatus>,
        #[arg(long)]
        json: bool,
    },
    /// Show one standard including evidence, counterexamples and revisions.
    Show {
        standard_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Explicitly approve a candidate or pending revision.
    Approve { standard_id: String, #[arg(long)] json: bool },
    /// Explicitly reject a candidate or pending revision.
    Reject { standard_id: String, #[arg(long)] json: bool },
    /// Re-hash supporting evidence and persist drift state.
    Drift {
        #[arg(long)]
        json: bool,
        /// Report only; do not persist drift/stale state.
        #[arg(long)]
        no_write: bool,
    },
    /// Report unresolved explicit conflicts among active standards.
    Conflicts { #[arg(long)] json: bool },
    /// Preview the bounded standards subset that a task would receive.
    Preview {
        task: String,
        #[arg(long = "file")]
        files: Vec<String>,
        #[arg(long, default_value_t = DEFAULT_PREVIEW_BUDGET)]
        budget_chars: usize,
        #[arg(long)]
        json: bool,
    },
    /// Import a portable Standards Studio JSON document from this repository.
    Import { path: PathBuf, #[arg(long)] json: bool },
    /// Export the current portable document without changing active policy.
    Export {
        #[arg(default_value = DEFAULT_EXPORT)]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn index_path(workspace: &Path) -> PathBuf { workspace.join(RELATIVE_INDEX) }

fn digest_bytes(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn digest_text(text: &str) -> String { digest_bytes(text.as_bytes()) }

fn load_document(workspace: &Path) -> Result<Value, String> {
    let path = index_path(workspace);
    if !path.exists() {
        return Ok(json!({"schema_version":1,"workspace_id":workspace.display().to_string(),"generated_at_ms":now_ms(),"standards":[]}));
    }
    let bytes = fs::read(&path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
    validate_document(&value)?;
    Ok(value)
}

fn validate_document(value: &Value) -> Result<(), String> {
    let object = value.as_object().ok_or_else(|| "standards document must be a JSON object".to_string())?;
    if object.get("schema_version").and_then(Value::as_u64) != Some(1) { return Err("unsupported standards schema version".to_string()); }
    if !object.get("workspace_id").is_some_and(Value::is_string) { return Err("standards document is missing workspace_id".to_string()); }
    let standards = object.get("standards").and_then(Value::as_array).ok_or_else(|| "standards document is missing standards[]".to_string())?;
    for standard in standards {
        let object = standard.as_object().ok_or_else(|| "standard entry must be an object".to_string())?;
        let id = object.get("standard_id").and_then(Value::as_str).ok_or_else(|| "standard is missing standard_id".to_string())?;
        if !object.get("title").is_some_and(Value::is_string) || !object.get("body").is_some_and(Value::is_string) { return Err(format!("standard {id} is missing title/body")); }
        let digest = object.get("content_sha256").and_then(Value::as_str).unwrap_or_default();
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) { return Err(format!("standard {id} has invalid content_sha256")); }
        if !object.get("evidence").is_some_and(Value::is_array) { return Err(format!("standard {id} has malformed evidence")); }
    }
    Ok(())
}

fn save_document(workspace: &Path, document: &mut Value) -> Result<(), String> {
    document["generated_at_ms"] = json!(now_ms());
    validate_document(document)?;
    let path = index_path(workspace);
    let parent = path.parent().ok_or_else(|| "invalid standards path".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let mut bytes = serde_json::to_vec_pretty(document).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes).map_err(|error| format!("failed to write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, &path).map_err(|error| format!("failed to replace {}: {error}", path.display()))
}

fn standards(value: &Value) -> Result<&Vec<Value>, String> {
    value.get("standards").and_then(Value::as_array).ok_or_else(|| "standards[] missing".to_string())
}

fn standards_mut(value: &mut Value) -> Result<&mut Vec<Value>, String> {
    value.get_mut("standards").and_then(Value::as_array_mut).ok_or_else(|| "standards[] missing".to_string())
}

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_EVIDENCE_BYTES { return None; }
    fs::read(path).ok()
}

fn evidence(workspace: &Path, relative: &str, kind: &str, supports: bool) -> Option<Value> {
    let bytes = read_bounded(&workspace.join(relative))?;
    let text = String::from_utf8_lossy(&bytes);
    let excerpt = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim().chars().take(500).collect::<String>();
    Some(json!({"path":relative.replace('\\', "/"),"line":if excerpt.is_empty(){Value::Null}else{json!(1)},"excerpt":excerpt,"sha256":digest_bytes(&bytes),"kind":kind,"supports":supports}))
}

fn candidate(
    id: &str, title: &str, body: &str, evidence: Vec<Value>, confidence: f64,
    tags: &[&str], globs: &[String], languages: &[&str], frameworks: &[&str], task_keywords: &[&str],
) -> Value {
    let applicability = json!({"globs":globs,"languages":languages,"frameworks":frameworks,"task_keywords":task_keywords});
    let policy = json!({"title":title,"body":body,"applicability":applicability,"severity":"recommended","tags":tags});
    json!({
        "standard_id":id,"version":1,"title":title,"body":body,"scope":"repository","scope_path":Value::Null,
        "applicability":policy["applicability"].clone(),"severity":"recommended","status":"candidate","origin":"discovered",
        "confidence":confidence.clamp(0.0,1.0),"tags":tags,"evidence":evidence,"conflicts_with":[],"supersedes":Value::Null,
        "created_at_ms":now_ms(),"approved_at_ms":Value::Null,"last_verified_at_ms":now_ms(),
        "content_sha256":digest_text(&serde_json::to_string(&policy).unwrap_or_default()),"drift":"healthy",
        "revision_history":[],"pending_revision":Value::Null,"checker_command_ids":[]
    })
}

fn scan_files(workspace: &Path) -> Vec<String> {
    let mut result = Vec::new();
    let mut bytes = 0u64;
    for entry in WalkDir::new(workspace).max_depth(MAX_SCAN_DEPTH + 1).follow_links(false).into_iter().filter_map(Result::ok) {
        if result.len() >= MAX_SCAN_FILES || bytes >= MAX_SCAN_BYTES { break; }
        if !entry.file_type().is_file() { continue; }
        let relative = match entry.path().strip_prefix(workspace) { Ok(path) => path.to_string_lossy().replace('\\', "/"), Err(_) => continue };
        if relative.split('/').any(|part| matches!(part, ".git"|"node_modules"|"target"|"dist"|"build"|".next"|".venv")) { continue; }
        let len = entry.metadata().map(|metadata| metadata.len()).unwrap_or(MAX_EVIDENCE_BYTES + 1);
        if len > MAX_EVIDENCE_BYTES { continue; }
        bytes = bytes.saturating_add(len);
        result.push(relative);
    }
    result
}

fn source_extension(path: &str) -> bool {
    matches!(Path::new(path).extension().and_then(|value| value.to_str()).unwrap_or_default().to_ascii_lowercase().as_str(), "ts"|"tsx"|"js"|"jsx"|"rs"|"py"|"go"|"java"|"kt"|"kts"|"swift"|"cs"|"c"|"cc"|"cpp"|"h"|"hpp")
}

fn file_contains(workspace: &Path, relative: &str, needles: &[&str]) -> bool {
    let bytes = match read_bounded(&workspace.join(relative)) { Some(bytes) => bytes, None => return false };
    let text = String::from_utf8_lossy(&bytes);
    needles.iter().any(|needle| text.contains(needle))
}

fn recurring_candidate(workspace: &Path, source: &[String], id: &str, title: &str, body: &str, needles: &[&str], tags: &[&str], task: &[&str]) -> Option<Value> {
    let paths = source.iter().filter(|path| file_contains(workspace, path, needles)).take(8).cloned().collect::<Vec<_>>();
    if paths.len() < 3 { return None; }
    let evidence = paths.iter().filter_map(|path| evidence(workspace, path, "code", true)).collect();
    Some(candidate(id,title,body,evidence,(0.7 + paths.len() as f64 * 0.03).min(0.95),tags,&[],&[],&[],task))
}

fn discover_repository(workspace: &Path) -> Vec<Value> {
    let files = scan_files(workspace);
    let source = files.iter().filter(|path| source_extension(path)).cloned().collect::<Vec<_>>();
    let mut result = Vec::<Value>::new();
    let mut ids = BTreeSet::<String>::new();
    let mut push = |value: Value| {
        let id = value.get("standard_id").and_then(Value::as_str).unwrap_or_default().to_string();
        if !id.is_empty() && ids.insert(id) { result.push(value); }
    };

    for (path,id,title,body,tags,languages) in [
        ("tsconfig.json","compiler-typescript","TypeScript compiler settings are repository authority","TypeScript changes should remain compatible with checked-in compiler/module settings rather than weakening them locally.",vec!["typescript","compiler","architecture"],vec!["typescript"]),
        ("Cargo.toml","rust-cargo-workflow","Rust code follows Cargo project conventions","Rust changes should preserve the existing Cargo workspace/package structure and Cargo-based build/test workflow.",vec!["rust","cargo"],vec!["rust"]),
        (".editorconfig","editorconfig-style","EditorConfig defines repository text conventions","New text files should preserve checked-in EditorConfig conventions.",vec!["formatting","editorconfig"],vec![]),
        ("rustfmt.toml","format-rustfmt","Rust formatting is repository-configured","Rust changes should remain compatible with checked-in rustfmt configuration.",vec!["rust","formatting"],vec!["rust"]),
        ("biome.json","format-biome","Biome owns JavaScript/TypeScript formatting or linting","JavaScript/TypeScript changes should preserve checked-in Biome rules.",vec!["biome","formatting","lint"],vec!["typescript","javascript"]),
        ("eslint.config.js","lint-eslint","ESLint rules are repository-configured","JavaScript/TypeScript changes should satisfy checked-in ESLint rules.",vec!["eslint","lint"],vec!["typescript","javascript"]),
    ] {
        if let Some(item) = evidence(workspace,path,"config",true) {
            push(candidate(id,title,body,vec![item],1.0,&tags,&[],&languages,&[],&tags));
        }
    }

    let tests = files.iter().filter(|path| path.contains("/__tests__/") || path.contains(".test.") || path.contains(".spec.")).cloned().collect::<Vec<_>>();
    if tests.len() >= 3 {
        let mut styles: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for path in &tests {
            let style = if path.contains("/__tests__/") { "__tests__ directory" } else if path.contains(".test.") { ".test file suffix" } else { ".spec file suffix" };
            styles.entry(style).or_default().push(path.clone());
        }
        if let Some((style,supporting)) = styles.iter().max_by_key(|(_,paths)| paths.len()) {
            let mut items = supporting.iter().take(5).filter_map(|path| evidence(workspace,path,"test",true)).collect::<Vec<_>>();
            for path in styles.iter().filter(|(other,_)| other != &style).flat_map(|(_,paths)| paths).take(5) { if let Some(item)=evidence(workspace,path,"test",false){items.push(item);} }
            push(candidate("testing-file-layout",&format!("Existing tests predominantly use {style}"),&format!("New tests should normally follow the repository's predominant {style} convention unless the target module clearly differs."),items,supporting.len() as f64/tests.len() as f64,&["testing","layout"],&["**/*.test.*".into(),"**/*.spec.*".into(),"**/__tests__/**".into()],&[],&[],&["test","tests","spec"]));
        }
    }

    let mut roots: BTreeMap<String,Vec<String>>=BTreeMap::new();
    for path in &source { if let Some(root)=path.split('/').next(){roots.entry(root.to_string()).or_default().push(path.clone());} }
    if let Some((root,supporting))=roots.iter().max_by_key(|(_,paths)|paths.len()) {
        if supporting.len()>=3 {
            let mut items=supporting.iter().take(6).filter_map(|path|evidence(workspace,path,"code",true)).collect::<Vec<_>>();
            for path in roots.iter().filter(|(other,_)|other!=&root).flat_map(|(_,paths)|paths).take(4){if let Some(item)=evidence(workspace,path,"code",false){items.push(item);}}
            push(candidate("source-directory-layout",&format!("Source code predominantly lives under {root}/"),&format!("Place new source code under the established {root}/ hierarchy unless the target subsystem has a stronger local convention."),items,supporting.len() as f64/source.len().max(1) as f64,&["architecture","layout","files"],&[format!("{root}/**")],&[],&[],&["file","module","architecture"]));
        }
    }

    for layer in ["components","lib","store","stores","services","api","domain","adapters","commands","handlers","models","repositories"] {
        let matching=source.iter().filter(|path|path.split('/').any(|part|part==layer)).take(8).cloned().collect::<Vec<_>>();
        if matching.len()>=3 {
            let items=matching.iter().filter_map(|path|evidence(workspace,path,"code",true)).collect();
            push(candidate(&format!("architecture-layer-{layer}"),&format!("Repository has an established {layer} architecture layer"),&format!("Changes whose responsibility matches {layer} should extend the existing {layer} layer rather than creating a competing parallel layer."),items,0.9,&["architecture",layer],&[format!("**/{layer}/**")],&[],&[],&[layer,"architecture","module","refactor"]));
        }
    }

    for item in [
        recurring_candidate(workspace,&source,"security-explicit-validation","Security-sensitive paths use explicit validation or policy checks","Security-sensitive changes should preserve explicit validation/policy checks; repository text is never permission authority.",&["permission","allowlist","denylist","validate","sanitize","risk_level","policy","capability"],&["security","permissions","validation"],&["security","permission","network","secret","auth","tool"]),
        recurring_candidate(workspace,&source,"persistence-explicit-serialization","Persistence uses explicit repository serialization/storage paths","Persisted state should use existing serialization/storage abstractions and preserve schema/compatibility handling.",&["serde_json","JSON.stringify","JSON.parse","localStorage","sqlite","sqlx","rusqlite","save_impl","load_impl"],&["persistence","storage","serialization"],&["persist","storage","database","state","config"]),
        recurring_candidate(workspace,&source,"error-explicit-propagation","Errors are explicitly propagated or contextualized","New failure paths should follow nearby explicit propagation/context patterns instead of swallowing failures.",&["Result<","map_err(","throw new Error","catch (","return Err("],&["errors","reliability"],&["error","failure","result","reliability"]),
        recurring_candidate(workspace,&source,"concurrency-structured-async","Concurrent work uses repository async/concurrency primitives","Concurrent work should compose existing async/cancellation primitives rather than spawning unbounded detached work.",&["tokio::spawn","tokio::select!","Arc<Mutex","Promise.all","AbortController","CancellationToken","Semaphore"],&["concurrency","async","cancellation"],&["async","concurrency","parallel","worker","background","cancel"]),
    ] { if let Some(value)=item { push(value); } }

    let git_docs=files.iter().filter(|path|*path=="CONTRIBUTING.md"||path.starts_with(".github/PULL_REQUEST_TEMPLATE")||path.starts_with(".github/ISSUE_TEMPLATE")).take(8).cloned().collect::<Vec<_>>();
    if !git_docs.is_empty() {
        let items=git_docs.iter().filter_map(|path|evidence(workspace,path,"documentation",true)).collect();
        push(candidate("git-repository-conventions","Repository documents Git/contribution conventions","Git delivery should follow checked-in contribution, pull-request, branch, and release guidance when applicable.",items,if git_docs.len()>1{0.95}else{0.75},&["git","contributing","delivery"],&[],&[],&[],&["git","commit","branch","pr","release"]));
    }
    let docs=files.iter().filter(|path|path==&"README.md"||path.starts_with("docs/")&&(path.ends_with(".md")||path.ends_with(".mdx"))).take(8).cloned().collect::<Vec<_>>();
    if docs.len()>=3 {
        let items=docs.iter().filter_map(|path|evidence(workspace,path,"documentation",true)).collect();
        push(candidate("documentation-checked-in-docs","Repository keeps substantial checked-in documentation","User-visible or architectural behavior changes should update relevant checked-in documentation rather than leaving it knowingly stale.",items,0.9,&["documentation","architecture"],&["docs/**".into(),"README*".into()],&[],&[],&["docs","documentation","architecture","feature","behavior"]));
    }
    result.sort_by(|a,b|a["title"].as_str().unwrap_or_default().cmp(b["title"].as_str().unwrap_or_default()));
    result
}

fn policy_equal(left: &Value, right: &Value) -> bool {
    ["title","body","applicability","severity","tags"].iter().all(|key| left.get(*key)==right.get(*key))
}

fn revision_snapshot(standard: &Value, reason: &str) -> Value {
    json!({"version":standard["version"],"title":standard["title"],"body":standard["body"],"applicability":standard["applicability"],"severity":standard["severity"],"tags":standard["tags"],"evidence":standard["evidence"],"content_sha256":standard["content_sha256"],"recorded_at_ms":now_ms(),"reason":reason})
}

fn pending_from(candidate: &Value, version: u64, source: &str) -> Value {
    json!({"version":version,"title":candidate["title"],"body":candidate["body"],"applicability":candidate["applicability"],"severity":candidate["severity"],"tags":candidate["tags"],"evidence":candidate["evidence"],"content_sha256":candidate["content_sha256"],"recorded_at_ms":now_ms(),"proposed_at_ms":now_ms(),"source":source})
}

fn merge_candidates(document: &mut Value, discovered: Vec<Value>) -> Result<(),String> {
    let entries=standards_mut(document)?;
    for mut candidate in discovered {
        let id=candidate["standard_id"].as_str().unwrap_or_default().to_string();
        if let Some(existing)=entries.iter_mut().find(|entry|entry["standard_id"].as_str()==Some(id.as_str())) {
            if policy_equal(existing,&candidate) {
                if existing["status"]!="approved" { existing["evidence"]=candidate["evidence"].take(); existing["confidence"]=candidate["confidence"].take(); }
                continue;
            }
            let version=existing["version"].as_u64().unwrap_or(1)+1;
            if existing["status"]=="approved" {
                existing["pending_revision"]=pending_from(&candidate,version,"discovered");
                existing["drift"]="weakened".into();
            } else {
                let snapshot=revision_snapshot(existing,"rediscovered");
                let history=existing.get_mut("revision_history").and_then(Value::as_array_mut).ok_or_else(||format!("standard {id} has malformed revision_history"))?;
                history.push(snapshot);
                candidate["version"]=json!(version);
                *existing=candidate;
            }
        } else { entries.push(candidate); }
    }
    Ok(())
}

fn find_standard<'a>(document: &'a Value,id:&str)->Result<&'a Value,String>{standards(document)?.iter().find(|entry|entry["standard_id"].as_str()==Some(id)).ok_or_else(||format!("unknown standard id {id}"))}
fn find_standard_mut<'a>(document: &'a mut Value,id:&str)->Result<&'a mut Value,String>{standards_mut(document)?.iter_mut().find(|entry|entry["standard_id"].as_str()==Some(id)).ok_or_else(||format!("unknown standard id {id}"))}

fn approve(document:&mut Value,id:&str)->Result<(),String>{
    let standard=find_standard_mut(document,id)?;
    if !standard["pending_revision"].is_null() {
        let pending=standard["pending_revision"].take();
        let snapshot=revision_snapshot(standard,"approved_revision");
        if let Some(history)=standard.get_mut("revision_history").and_then(Value::as_array_mut){history.push(snapshot);}
        for key in ["version","title","body","applicability","severity","tags","evidence","content_sha256"] {standard[key]=pending[key].clone();}
    }
    standard["status"]="approved".into(); standard["approved_at_ms"]=json!(now_ms()); standard["last_verified_at_ms"]=json!(now_ms()); standard["drift"]="healthy".into(); standard["pending_revision"]=Value::Null;
    Ok(())
}

fn reject(document:&mut Value,id:&str)->Result<(),String>{
    let standard=find_standard_mut(document,id)?;
    if !standard["pending_revision"].is_null() { standard["pending_revision"]=Value::Null; standard["drift"]="healthy".into(); }
    else { standard["status"]="rejected".into(); }
    Ok(())
}

fn drift_report(workspace:&Path,standard:&Value)->Value{
    let supporting=standard["evidence"].as_array().map(|items|items.iter().filter(|item|item["supports"].as_bool()==Some(true)).collect::<Vec<_>>()).unwrap_or_default();
    let unchanged=supporting.iter().filter(|item|{
        let path=item["path"].as_str().unwrap_or_default(); let expected=item["sha256"].as_str().unwrap_or_default();
        fs::read(workspace.join(path)).map(|bytes|digest_bytes(&bytes)==expected).unwrap_or(false)
    }).count();
    let current=if supporting.is_empty(){"unknown"}else if unchanged==supporting.len(){"healthy"}else if unchanged==0{"contradicted"}else{"weakened"};
    json!({"standard_id":standard["standard_id"],"previous":standard["drift"],"current":current,"unchanged_supporting":unchanged,"supporting_total":supporting.len()})
}

fn conflict_pairs(document:&Value)->Result<Vec<String>,String>{
    let active=standards(document)?.iter().filter(|entry|matches!(entry["status"].as_str(),Some("approved"|"candidate"|"conflicting"))).filter_map(|entry|entry["standard_id"].as_str()).collect::<BTreeSet<_>>();
    let mut pairs=BTreeSet::new();
    for entry in standards(document)? { let Some(id)=entry["standard_id"].as_str() else{continue}; for other in entry["conflicts_with"].as_array().into_iter().flatten().filter_map(Value::as_str){if active.contains(id)&&active.contains(other){let (a,b)=if id<=other{(id,other)}else{(other,id)};pairs.insert(format!("{a} <-> {b}"));}} }
    Ok(pairs.into_iter().collect())
}

fn tokens(text:&str)->BTreeSet<String>{text.to_ascii_lowercase().split(|c:char|!c.is_ascii_alphanumeric()&&!matches!(c,'_'|'+'|'#'|'.'|'-')).filter(|part|part.len()>=2).map(str::to_string).collect()}

fn preview(document:&Value,task:&str,file_hints:&[String],budget:usize)->Result<Value,String>{
    let query=tokens(&format!("{} {}",task,file_hints.join(" ")));
    let conflicts=conflict_pairs(document)?;
    let conflicted=conflicts.iter().flat_map(|pair|pair.split(" <-> ")).collect::<BTreeSet<_>>();
    let mut ranked=Vec::<(i64,usize,Value)>::new();
    for standard in standards(document)? {
        let id=standard["standard_id"].as_str().unwrap_or_default();
        if standard["status"]!="approved"||matches!(standard["drift"].as_str(),Some("contradicted"|"unknown"))||conflicted.contains(id){continue;}
        let mut score=if standard["severity"]=="required"{100}else{0}; let mut reasons=Vec::new();
        for field in ["tags","task_keywords","languages","frameworks"] {
            let values=if field=="task_keywords"||field=="languages"||field=="frameworks" { standard["applicability"].get(field) } else { standard.get(field) };
            for value in values.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str){if query.contains(&value.to_ascii_lowercase()){score+=20;reasons.push(format!("{field}:{value}"));}}
        }
        for glob in standard["applicability"]["globs"].as_array().into_iter().flatten().filter_map(Value::as_str){let hint=glob.replace("**/","").replace('*',"");if hint.len()>1&&file_hints.iter().any(|path|path.to_ascii_lowercase().contains(&hint.to_ascii_lowercase())){score+=25;reasons.push(format!("files:{glob}"));}}
        if score<=0 {continue;}
        let chars=standard["body"].as_str().unwrap_or_default().chars().count()+standard["title"].as_str().unwrap_or_default().chars().count()+120;
        ranked.push((score,chars,json!({"standard_id":id,"version":standard["version"],"content_sha256":standard["content_sha256"],"severity":standard["severity"],"drift":standard["drift"],"score":score,"reasons":reasons,"chars":chars,"title":standard["title"],"body":standard["body"]})));
    }
    ranked.sort_by(|a,b|b.0.cmp(&a.0).then_with(||a.2["standard_id"].as_str().cmp(&b.2["standard_id"].as_str())));
    let mut used=0usize; let mut selected=Vec::new(); let total=ranked.len();
    for (_,chars,item) in ranked {if used+chars>budget{continue;}used+=chars;selected.push(item);}
    Ok(json!({"schema_version":1,"selected":selected,"omitted":total-selected.len(),"total_chars":used,"budget_chars":budget}))
}

fn import_document(workspace:&Path,path:&Path,document:&mut Value)->Result<(),String>{
    let absolute=if path.is_absolute(){path.to_path_buf()}else{workspace.join(path)};
    let incoming:Value=serde_json::from_slice(&fs::read(&absolute).map_err(|error|format!("failed to read {}: {error}",absolute.display()))?).map_err(|error|format!("failed to parse {}: {error}",absolute.display()))?;
    validate_document(&incoming)?;
    let imported=standards(&incoming)?.clone();
    merge_candidates(document,imported)?;
    Ok(())
}

fn export_document(workspace:&Path,path:&Path,document:&Value)->Result<PathBuf,String>{
    let absolute=if path.is_absolute(){path.to_path_buf()}else{workspace.join(path)};
    if let Some(parent)=absolute.parent(){fs::create_dir_all(parent).map_err(|error|format!("failed to create {}: {error}",parent.display()))?;}
    let mut bytes=serde_json::to_vec_pretty(document).map_err(|error|error.to_string())?; bytes.push(b'\n');
    fs::write(&absolute,bytes).map_err(|error|format!("failed to write {}: {error}",absolute.display()))?; Ok(absolute)
}

fn print_standard(standard:&Value,json_output:bool)->Result<(),String>{
    if json_output {println!("{}",serde_json::to_string_pretty(standard).map_err(|error|error.to_string())?);}
    else {
        println!("{}@v{}  {}  {}",standard["standard_id"].as_str().unwrap_or_default(),standard["version"].as_u64().unwrap_or(1),standard["status"].as_str().unwrap_or("unknown"),standard["title"].as_str().unwrap_or_default());
        println!("{}",standard["body"].as_str().unwrap_or_default());
        for item in standard["evidence"].as_array().into_iter().flatten(){println!("  {} {}:{}  {}",if item["supports"].as_bool()==Some(false){"counterexample"}else{"evidence"},item["path"].as_str().unwrap_or_default(),item["line"].as_u64().map(|v|v.to_string()).unwrap_or_else(||"?".into()),item["excerpt"].as_str().unwrap_or_default());}
    }
    Ok(())
}

pub fn run(action:&StandardsCmd,workspace:&Path)->Result<(),String>{
    let workspace=workspace.canonicalize().map_err(|error|format!("invalid workspace {}: {error}",workspace.display()))?;
    match action {
        StandardsCmd::Discover{json,dry_run}=>{let discovered=discover_repository(&workspace);let mut document=load_document(&workspace)?;merge_candidates(&mut document,discovered.clone())?;if !dry_run{save_document(&workspace,&mut document)?;}if *json{println!("{}",serde_json::to_string_pretty(&discovered).map_err(|error|error.to_string())?);}else{println!("Discovered {} evidence-backed standard candidates{}.",discovered.len(),if *dry_run{" (dry run)"}else{""});for item in discovered{println!("{}  {:.0}%  {}",item["standard_id"].as_str().unwrap_or_default(),item["confidence"].as_f64().unwrap_or(0.0)*100.0,item["title"].as_str().unwrap_or_default());}}Ok(())},
        StandardsCmd::List{status,json}=>{let document=load_document(&workspace)?;let filtered=standards(&document)?.iter().filter(|entry|status.map(|wanted|entry["status"].as_str()==Some(wanted.as_str())).unwrap_or(true)).collect::<Vec<_>>();if *json{println!("{}",serde_json::to_string_pretty(&filtered).map_err(|error|error.to_string())?);}else{for item in filtered{println!("{}@v{}\t{}\t{}\t{}",item["standard_id"].as_str().unwrap_or_default(),item["version"].as_u64().unwrap_or(1),item["status"].as_str().unwrap_or("unknown"),item["drift"].as_str().unwrap_or("unknown"),item["title"].as_str().unwrap_or_default());}}Ok(())},
        StandardsCmd::Show{standard_id,json}=>{let document=load_document(&workspace)?;print_standard(find_standard(&document,standard_id)?,*json)},
        StandardsCmd::Approve{standard_id,json}=>{let mut document=load_document(&workspace)?;approve(&mut document,standard_id)?;save_document(&workspace,&mut document)?;print_standard(find_standard(&document,standard_id)?,*json)},
        StandardsCmd::Reject{standard_id,json}=>{let mut document=load_document(&workspace)?;reject(&mut document,standard_id)?;save_document(&workspace,&mut document)?;print_standard(find_standard(&document,standard_id)?,*json)},
        StandardsCmd::Drift{json,no_write}=>{let mut document=load_document(&workspace)?;let reports=standards(&document)?.iter().map(|entry|drift_report(&workspace,entry)).collect::<Vec<_>>();if !no_write{let timestamp=now_ms();for (entry,report) in standards_mut(&mut document)?.iter_mut().zip(&reports){entry["drift"]=report["current"].clone();entry["last_verified_at_ms"]=json!(timestamp);if entry["status"]=="approved"&&report["current"]=="contradicted"{entry["status"]="stale".into();}}save_document(&workspace,&mut document)?;}if *json{println!("{}",serde_json::to_string_pretty(&reports).map_err(|error|error.to_string())?);}else{for report in reports{println!("{}\t{} -> {}\t{}/{} supporting evidence unchanged",report["standard_id"].as_str().unwrap_or_default(),report["previous"].as_str().unwrap_or("unknown"),report["current"].as_str().unwrap_or("unknown"),report["unchanged_supporting"],report["supporting_total"]);}}Ok(())},
        StandardsCmd::Conflicts{json}=>{let document=load_document(&workspace)?;let pairs=conflict_pairs(&document)?;if *json{println!("{}",serde_json::to_string_pretty(&json!({"conflicts":pairs})).map_err(|error|error.to_string())?);}else if pairs.is_empty(){println!("No unresolved standards conflicts.");}else{for pair in pairs{println!("{pair}");}}Ok(())},
        StandardsCmd::Preview{task,files,budget_chars,json}=>{let document=load_document(&workspace)?;let report=preview(&document,task,files,*budget_chars)?;if *json{println!("{}",serde_json::to_string_pretty(&report).map_err(|error|error.to_string())?);}else{for item in report["selected"].as_array().into_iter().flatten(){println!("{}@v{} score={} {}",item["standard_id"].as_str().unwrap_or_default(),item["version"],item["score"],item["title"].as_str().unwrap_or_default());}println!("{} chars; {} omitted",report["total_chars"],report["omitted"]);}Ok(())},
        StandardsCmd::Import{path,json}=>{let mut document=load_document(&workspace)?;import_document(&workspace,path,&mut document)?;save_document(&workspace,&mut document)?;if *json{println!("{}",serde_json::to_string_pretty(&document).map_err(|error|error.to_string())?);}else{println!("Imported standards from {}.",path.display());}Ok(())},
        StandardsCmd::Export{path,json}=>{let document=load_document(&workspace)?;let output=export_document(&workspace,path,&document)?;if *json{println!("{}",serde_json::to_string_pretty(&json!({"path":output})).map_err(|error|error.to_string())?);}else{println!("Exported standards to {}.",output.display());}Ok(())},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approved(id:&str,tags:&[&str],body:&str)->Value{let mut value=candidate(id,id,body,vec![],1.0,tags,&[],&[],&[],tags);value["status"]="approved".into();value}

    #[test]
    fn preview_is_bounded_and_relevant(){let document=json!({"schema_version":1,"workspace_id":"x","generated_at_ms":1,"standards":[approved("react",&["react","component"],"A"),approved("rust",&["rust","cargo"],"B")]});let result=preview(&document,"Add React component",&[],DEFAULT_PREVIEW_BUDGET).unwrap();assert_eq!(result["selected"].as_array().unwrap().len(),1);assert_eq!(result["selected"][0]["standard_id"],"react");}

    #[test]
    fn merge_never_silently_replaces_approved_policy(){let mut current=approved("x",&["x"],"old");current["version"]=json!(2);let mut document=json!({"schema_version":1,"workspace_id":"x","generated_at_ms":1,"standards":[current]});let changed=candidate("x","x","new",vec![],1.0,&["x"],&[],&[],&[],&["x"]);merge_candidates(&mut document,vec![changed]).unwrap();assert_eq!(document["standards"][0]["body"],"old");assert_eq!(document["standards"][0]["pending_revision"]["body"],"new");}

    #[test]
    fn repository_text_cannot_auto_approve(){let temp=std::env::temp_dir().join(format!("lm-standards-{}",now_ms()));fs::create_dir_all(temp.join("src/security")).unwrap();for name in ["a.ts","b.ts","c.ts"]{fs::write(temp.join("src/security").join(name),"permission validate policy").unwrap();}let discovered=discover_repository(&temp);assert!(discovered.iter().all(|entry|entry["status"]=="candidate"));let _=fs::remove_dir_all(temp);}
}
