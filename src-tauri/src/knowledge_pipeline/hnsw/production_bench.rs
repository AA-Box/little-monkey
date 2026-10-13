//! Opt-in real SQLite/HNSW production acceptance.
//! Run in release mode. This is deliberately ignored by ordinary unit tests:
//! it builds a genuine immutable Knowledge index, not an in-memory graph mock.
use super::*;
use super::super::{
    sha256_bytes, Citation, ContentRole, DocumentLocation, EmbeddingSpec,
    HybridIndex, HybridSearchConfig, PipelineLimits, EMBEDDING_CONTRACT_VERSION,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

const K: usize = 10;

fn env_usize(name: &str, default: usize, low: usize, high: usize) -> usize {
    let value = std::env::var(name).ok().map(|v| v.parse::<usize>().expect("invalid unsigned benchmark parameter")).unwrap_or(default);
    assert!((low..=high).contains(&value), "{name} outside [{low},{high}]");
    value
}
fn env_f64(name: &str, default: f64) -> f64 {
    let value = std::env::var(name).ok().map(|v| v.parse::<f64>().expect("invalid numeric gate")).unwrap_or(default);
    assert!(value.is_finite() && value >= 0.0, "invalid {name}");
    value
}
fn rng(state: &mut u64) -> f32 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    (((*state >> 40) as u32) as f32 / 16_777_216.0) * 2.0 - 1.0
}
fn normalized(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| f64::from(*x) * f64::from(*x)).sum::<f64>().sqrt() as f32;
    assert!(norm > 0.0);
    for x in &mut v { *x /= norm; }
    v
}
fn corpus(count: usize, dim: usize) -> (Vec<KnowledgeChunk>,Vec<Vec<f32>>) {
    // 256 independently seeded topic clusters with deterministic per-document
    // variation: neither all-equal vectors nor a 2-dimensional circle.
    let centers: Vec<_> = (0..256_u64).map(|i| {
        let mut state = (i + 1).wrapping_mul(0x9e3779b97f4a7c15);
        normalized((0..dim).map(|_| rng(&mut state)).collect())
    }).collect();
    let mut chunks = Vec::with_capacity(count);
    let mut vectors = Vec::with_capacity(count);
    for i in 0..count {
        let mut state = (i as u64 + 1).wrapping_mul(0xd6e8feb86659fd93);
        let center = &centers[i % centers.len()];
        let vector = normalized(center.iter().map(|x| *x + 0.28 * rng(&mut state)).collect());
        vectors.push(vector);
        let text = format!("benchmark knowledge document number {i} topic {}", i%256);
        let digest = sha256_bytes(text.as_bytes());
        let id = sha256_bytes(format!("hnsw-production-{i}").as_bytes());
        let source = format!("source:benchmark:{}", i % 8);
        let object = format!("object:benchmark:{i}");
        let location = DocumentLocation::Text {
            line_start:1, line_end:1, char_start:0, char_end:text.len() as u64,
        };
        chunks.push(KnowledgeChunk {
            chunk_id:id.clone(), source_id:source.clone(), object_id:object.clone(),
            object_content_sha256:digest.clone(), text_sha256:digest,
            text:text.clone(), heading_path:vec!["production benchmark".to_string()],
            location:location.clone(), block_char_start:0,
            block_char_end:text.len() as u64,
            citation:Citation {
                citation_id:sha256_bytes(format!("citation-hnsw-{i}").as_bytes()),
                source_id:source, object_id:object,
                canonical_uri:format!("fixture://hnsw-production/{i}"),
                location, block_char_start:0, block_char_end:text.len() as u64,
            },
            content_role:ContentRole::RetrievedData, content_type:"text".into(),
            confidence_micros:None, low_confidence:false,
        });
    }
    (chunks,vectors)
}
fn exact(query:&[f32], ids:&[String],vectors:&[Vec<f32>])->Vec<String>{
    let mut scored=vectors.iter().enumerate().map(|(i,v)| (i,similarity(query,v))).collect::<Vec<_>>();
    scored.sort_unstable_by(|a,b|b.1.total_cmp(&a.1).then_with(||ids[a.0].cmp(&ids[b.0])));
    scored.iter().take(K).map(|(i,_)|ids[*i].clone()).collect()
}
fn percentile(durations:&mut [f64], percent:f64)->f64{
    durations.sort_by(|a,b|a.total_cmp(b));
    let idx=((durations.len()-1) as f64 * percent).ceil() as usize;
    durations[idx]
}
fn hwm_mib()->Option<f64>{
    #[cfg(target_os="linux")] {
        let s=fs::read_to_string("/proc/self/status").ok()?;
        let line=s.lines().find(|l|l.starts_with("VmHWM:"))?;
        let kb=line.split_whitespace().nth(1)?.parse::<f64>().ok()?;
        Some(kb/1024.0)
    }
    #[cfg(not(target_os="linux"))] { None }
}
fn bytes_on_disk(path:&Path)->u64 { fs::metadata(path).expect("index metadata").len() }
fn report(json:serde_json::Value) {
    let output=serde_json::to_string_pretty(&json).expect("serialize report");
    println!("HNSW_PRODUCTION_METRICS={output}");
    if let Ok(target)=std::env::var("LM_HNSW_REPORT") {
        let target=PathBuf::from(target);
        if let Some(parent)=target.parent(){fs::create_dir_all(parent).expect("report directory");}
        fs::write(target,output).expect("write report");
    }
}
fn index_path() -> PathBuf {
    std::env::var_os("LM_HNSW_DB").map(PathBuf::from).unwrap_or_else(||
        std::env::temp_dir().join(format!("little-monkey-hnsw-{}.sqlite3",Uuid::new_v4()))
    )
}
fn readonly(path:&Path)->Connection {
    Connection::open_with_flags(path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
    ).expect("open independent SQLite reader")
}
#[test]
#[ignore = "release-only, measured index build/retrieval/reliability on real SQLite"]
fn production_hnsw_scale_gate() {
    let count=env_usize("LM_HNSW_COUNT",10_000,512,100_000);
    let dim=env_usize("LM_HNSW_DIM",128,16,1024);
    let query_count=env_usize("LM_HNSW_QUERIES",48,10,256);
    let path=index_path();
    assert!(!path.exists(),"refuse to overwrite production benchmark path");
    let cancel=CancellationToken::new();
    let (chunks,vectors)=corpus(count,dim);
    let ids=chunks.iter().map(|c|c.chunk_id.clone()).collect::<Vec<_>>();
    let spec=EmbeddingSpec {
        contract_version:EMBEDDING_CONTRACT_VERSION,
        provider_id:"benchmark.hnsw".into(),model_id:"deterministic-clustered-embedding-v1".into(),
        dimension:dim,query_prefix:String::new(),document_prefix:String::new(),normalized:true
    };
    let start=Instant::now();
    let index=HybridIndex::create(&path,"generation:hnsw-production-v1",&chunks,&vectors,&spec,&cancel)
        .expect("create real immutable HNSW index");
    let build_seconds=start.elapsed().as_secs_f64();
    drop(index);
    let reopen=Instant::now();
    let index=HybridIndex::open(&path).expect("verify immutable generation digest and graph");
    let reopen_ms=reopen.elapsed().as_secs_f64()*1000.0;
    let conn=readonly(&path);
    let entries:i64=conn.query_row("SELECT COUNT(*) FROM hnsw_nodes",[],|r|r.get(0)).expect("persisted graph");
    assert_eq!(entries as usize,count,"benchmark must exercise persisted HNSW");
    let mut elapsed=Vec::with_capacity(query_count);
    let mut correct=0;
    for q in 0..query_count {
        let i=(q*7919+53)%count;
        let needle=&vectors[i];
        let ground=exact(needle,&ids,&vectors);
        let now=Instant::now();
        let got=query(&conn,needle,dim,K,&BTreeSet::new(),&cancel)
            .expect("disk ANN query").expect("must use graph");
        elapsed.push(now.elapsed().as_secs_f64()*1000.0);
        assert_eq!(got.len(),K,"ANN must not under-fill");
        let got=got.iter().map(|(id,_)|id.as_str()).collect::<HashSet<_>>();
        correct+=ground.iter().filter(|id|got.contains(id.as_str())).count();
    }
    let recall=correct as f64/(query_count*K) as f64;
    let p50=percentile(&mut elapsed.clone(),0.50);
    let p95=percentile(&mut elapsed,0.95);
    let app=index.search("benchmark", &vectors[0],
        &HybridSearchConfig::default(),&PipelineLimits::default(),None,&cancel);
    assert!(app.is_ok(),"full hybrid search must use the same persisted graph: {app:?}");
    let mut excluded=BTreeSet::new();excluded.insert("source:benchmark:0".into());
    assert!(query(&conn,&vectors[0],dim,K,&excluded,&cancel).unwrap().is_none(),
        "exclusion must force complete exact scan, never leak filtered candidates");
    let filtered=index.search_excluding_sources("benchmark",&vectors[0],
        &HybridSearchConfig::default(),&PipelineLimits::default(),None,
        &excluded,&cancel).expect("filtered full pipeline");
    assert!(filtered.hits.iter().all(|hit|!excluded.contains(&hit.chunk.source_id)),
        "filtered production index must not reveal excluded chunks");
    assert!(query(&conn,&vectors[0],dim,513,&BTreeSet::new(),&cancel).unwrap().is_none(),
        "requests wider than the bounded ANN beam must use exact ranking");
    let cancelled=CancellationToken::new();cancelled.cancel();
    assert!(query(&conn,&vectors[0],dim,K,&BTreeSet::new(),&cancelled).is_err(),
        "cancellation must interrupt persisted traversal");
    // Independent SQLite connections, simultaneous readers, and non-shared caches.
    let path=Arc::new(path.clone());
    let needles=Arc::new((0..8).map(|i|vectors[i*101%count].clone()).collect::<Vec<_>>());
    let started=Instant::now();
    let workers=(0..8).map(|worker|{
        let path=Arc::clone(&path); let needles=Arc::clone(&needles);
        std::thread::spawn(move || {
            let reader=readonly(&path);
            for attempt in 0..8 {
                let found=query(&reader,&needles[(worker+attempt)%needles.len()],
                    dim,K,&BTreeSet::new(),&CancellationToken::new())
                    .expect("concurrent query").expect("concurrent accelerated graph");
                assert_eq!(found.len(),K);
            }
        })
    }).collect::<Vec<_>>();
    for worker in workers{worker.join().expect("concurrent reader panic");}
    let concurrent_qps=64.0/started.elapsed().as_secs_f64();
    let hwm=hwm_mib();
    let disk_mib=bytes_on_disk(&path) as f64/1048576.0;
    let result=serde_json::json!({
        "schema":"little-monkey.hnsw.production-acceptance.v1",
        "index_size":count,"dimensions":dim,"queries":query_count,
        "build_seconds":build_seconds,"reopen_ms":reopen_ms,
        "recall_at_10":recall,"ann_p50_ms":p50,"ann_p95_ms":p95,
        "concurrent_readers":8,"concurrent_queries_per_sec":concurrent_qps,
        "sqlite_file_mib":disk_mib,"process_peak_rss_mib":hwm,
        "integrity":"reopened_and_verified","hybrid_search":"passed",
        "source_exclusion":"end_to_end_exact_fallback","cancellation":"passed",
        "concurrent_independent_sqlite_readers":"passed",
    });
    report(result);
    assert!(recall>=env_f64("LM_HNSW_MIN_RECALL",0.80),
        "measured recall@10={recall:.4} below configured gate");
    assert!(p95<=env_f64("LM_HNSW_MAX_P95_MS",250.0),
        "measured p95={p95:.2}ms above configured gate");
    assert!(build_seconds<=env_f64("LM_HNSW_MAX_BUILD_S",3600.0),
        "measured build {build_seconds:.1}s above configured gate");
    assert!(concurrent_qps>=env_f64("LM_HNSW_MIN_QPS",8.0),
        "measured concurrent QPS={concurrent_qps:.2} below configured gate");
    if let Some(rss)=hwm {
        assert!(rss<=env_f64("LM_HNSW_MAX_RSS_MIB",3500.0),
            "measured peak RSS={rss:.1}MiB above configured gate");
    }
    // A distinct test process opens LM_HNSW_DB again. Preserve only when
    // explicitly configured; ephemeral local runs clean up their database.
    if std::env::var_os("LM_HNSW_DB").is_none(){fs::remove_file(&*path).expect("cleanup index");}
}
#[test]
#[ignore = "run in a separate process after the scale gate with LM_HNSW_DB"]
fn production_hnsw_fresh_process_reopen_gate() {
    let path=PathBuf::from(std::env::var("LM_HNSW_DB").expect("explicit DB path required"));
    let reopened=HybridIndex::open(&path).expect("fresh process must verify source and graph checksums");
    let conn=readonly(&path);
    let count:i64=conn.query_row("SELECT COUNT(*) FROM chunks",[],|r|r.get(0)).unwrap();
    let hnsw_count:i64=conn.query_row("SELECT COUNT(*) FROM hnsw_nodes",[],|r|r.get(0)).unwrap();
    assert_eq!(count,hnsw_count);
    assert!(count>=512);
    let dim=reopened.embedding_spec().dimension;
    let needle:Vec<u8>=conn.query_row("SELECT vector FROM chunks ORDER BY chunk_id LIMIT 1",[],|r|r.get(0)).unwrap();
    let needle=decode_vector(&needle,dim).unwrap();
    let hits=query(&conn,&needle,dim,K,&BTreeSet::new(),&CancellationToken::new())
        .unwrap().expect("persisted graph after process exit");
    assert_eq!(hits.len(),K);
    drop(conn);
    // Corrupt a COPY, never the measured index. A digest mismatch must fail
    // even when SQL remains syntactically valid.
    let corrupt=path.with_extension("corrupt.sqlite3");
    fs::copy(&path,&corrupt).expect("copy immutable index");
    {
        let writer=Connection::open(&corrupt).unwrap();
        writer.execute("UPDATE hnsw_nodes SET links_json='[]' WHERE chunk_id=(SELECT chunk_id FROM hnsw_nodes LIMIT 1)",[])
            .expect("mutate one graph edge record");
    }
    let result=HybridIndex::open(&corrupt);
    assert!(result.is_err(),"corrupted graph MUST fail a fresh index open");
    fs::remove_file(&corrupt).expect("remove corruption fixture");
    println!("HNSW_FRESH_PROCESS_REOPEN_OK nodes={count} dimensions={dim} retrieval=passed tamper=refused");
}
