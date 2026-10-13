//! Persisted hierarchical navigable small-world candidate index for Knowledge 2.0.
//!
//! The SQLite generation remains the authority for vectors/source metadata;
//! HNSW is an immutable acceleration structure written IN THE SAME build
//! transaction. Old generations and small fixture indexes continue using the
//! existing exact dot-product scan. A malformed accelerated index fails closed.
//! Never replaces FTS5, reranking, permission checks or source exclusions.
use std::collections::{BTreeSet,HashMap,HashSet};
use rusqlite::{params,Connection,OptionalExtension,Transaction};
use sha2::{Digest,Sha256};
use tokio_util::sync::CancellationToken;
use super::{KnowledgeChunk,PipelineError,PipelineResult,check_cancelled,decode_vector};

const MIN_ACCELERATED:usize=512;
const MAX_LAYER:usize=10;
const MAX_NEIGHBORS:usize=12;
const BUILD_BEAM:usize=64;
const SEARCH_BEAM:usize=128;

#[derive(Clone)]
struct Point{
    id:String,
    level:usize,
    links:Vec<Vec<usize>>,
}
fn level_for(id:&str)->usize{
    let digest=Sha256::digest(id.as_bytes());
    let mut level=0;
    for byte in digest {
        if byte&3==0 {level+=1;}else{break;}
        if level>=MAX_LAYER{break;}
    }
    level
}
fn similarity(a:&[f32],b:&[f32])->f32{
    a.iter().zip(b).map(|(x,y)|x*y).sum()
}
fn best_first(mut scored:Vec<(usize,f32)>,limit:usize)->Vec<(usize,f32)>{
    scored.sort_by(|a,b|b.1.total_cmp(&a.1).then_with(||a.0.cmp(&b.0)));
    scored.truncate(limit);
    scored
}
fn greedy(points:&[Point],vectors:&[Vec<f32>],query:&[f32],
    start:usize,layer:usize)->usize{
    let mut current=start;
    let mut current_score=similarity(query,&vectors[current]);
    loop{
        let mut next=current;
        let mut next_score=current_score;
        for &n in points[current].links.get(layer).into_iter().flatten(){
            let score=similarity(query,&vectors[n]);
            if score>next_score||(score==next_score&&n<next){
                next=n;next_score=score;
            }
        }
        if next==current{break;}
        current=next;current_score=next_score;
    }
    current
}
fn beam(points:&[Point],vectors:&[Vec<f32>],query:&[f32],
    start:usize,layer:usize,ef:usize)->Vec<(usize,f32)>{
    let first=(start,similarity(query,&vectors[start]));
    let mut seen=HashSet::from([start]);
    let mut pending=vec![first];
    let mut found=vec![first];
    let mut expanded=0usize;
    while !pending.is_empty()&&expanded<ef*16{
        pending.sort_by(|a,b|b.1.total_cmp(&a.1).then_with(||a.0.cmp(&b.0)));
        let current=pending.remove(0);
        expanded+=1;
        if found.len()>=ef && current.1<found.last().map(|x|x.1).unwrap_or(f32::NEG_INFINITY){break;}
        for &next in points[current.0].links.get(layer).into_iter().flatten(){
            if !seen.insert(next){continue;}
            let score=similarity(query,&vectors[next]);
            if found.len()<ef||score>=found.last().map(|x|x.1).unwrap_or(f32::NEG_INFINITY){
                pending.push((next,score));
                found.push((next,score));
                found=best_first(found,ef);
            }
        }
    }
    found
}
fn prune(points:&mut [Point],vectors:&[Vec<f32>],node:usize,layer:usize){
    let neighbours=points[node].links[layer].clone();
    let mut scored=Vec::with_capacity(neighbours.len());
    for id in neighbours{
        scored.push((id,similarity(&vectors[node],&vectors[id])));
    }
    points[node].links[layer]=best_first(scored,MAX_NEIGHBORS)
        .into_iter().map(|row|row.0).collect();
}
fn build_graph(chunks:&[KnowledgeChunk],vectors:&[Vec<f32>],
    cancel:&CancellationToken)->PipelineResult<(usize,usize,Vec<Point>)>{
    let mut points=Vec::<Point>::with_capacity(chunks.len());
    let mut top=0;
    let mut top_layer=0;
    for (id,chunk) in chunks.iter().enumerate(){
        if id%32==0{check_cancelled(cancel)?;}
        let level=level_for(&chunk.chunk_id);
        points.push(Point{id:chunk.chunk_id.clone(),level,
            links:vec![Vec::new();level+1]});
        if id==0{top=0;top_layer=level;continue;}
        let mut entry=top;
        if level<top_layer {
            for layer in (level+1..=top_layer).rev(){
                entry=greedy(&points,vectors,&vectors[id],entry,layer);
            }
        }
        for layer in (0..=level.min(top_layer)).rev(){
            let candidates=beam(&points,vectors,&vectors[id],entry,layer,BUILD_BEAM);
            let chosen=candidates.into_iter().filter(|row|row.0!=id)
                .take(MAX_NEIGHBORS).map(|x|x.0).collect::<Vec<_>>();
            if let Some(&candidate)=chosen.first(){entry=candidate;}
            points[id].links[layer]=chosen.clone();
            for other in chosen {
                points[other].links[layer].push(id);
                prune(&mut points,vectors,other,layer);
            }
        }
        if level>top_layer{top=id;top_layer=level;}
    }
    Ok((top,top_layer,points))
}
/// Called after the vectors have been checked and inserted, inside the
/// generation's existing immutable SQLite transaction.
pub(super) fn build(tx:&Transaction<'_>,chunks:&[KnowledgeChunk],
    vectors:&[Vec<f32>],cancel:&CancellationToken)->PipelineResult<()>{
    if chunks.len()<MIN_ACCELERATED{return Ok(());}
    let (top,layer,points)=build_graph(chunks,vectors,cancel)?;
    tx.execute_batch("CREATE TABLE hnsw_nodes(
        chunk_id TEXT PRIMARY KEY NOT NULL,
        layer INTEGER NOT NULL,
        links_json TEXT NOT NULL,
        FOREIGN KEY(chunk_id) REFERENCES chunks(chunk_id)
    ) WITHOUT ROWID;")?;
    let mut digest=Sha256::new();
    // Canonical sorted-ID hash can be independently rechecked from SQLite
    // without needing the original source insertion order.
    let mut order=(0..points.len()).collect::<Vec<_>>();
    order.sort_by(|a,b|points[*a].id.cmp(&points[*b].id));
    for index in order{
        check_cancelled(cancel)?;
        let point=&points[index];
        let links=point.links.iter().map(|level|
            level.iter().map(|id|points[*id].id.clone()).collect::<Vec<_>>()
        ).collect::<Vec<_>>();
        let links_json=serde_json::to_string(&links)?;
        digest.update(point.id.as_bytes());
        digest.update((point.level as u32).to_le_bytes());
        digest.update(links_json.as_bytes());
        tx.execute("INSERT INTO hnsw_nodes(chunk_id,layer,links_json) VALUES(?1,?2,?3)",
            params![point.id,point.level as i64,links_json])?;
    }
    for (key,value) in [
        ("hnsw_version","1".to_string()),
        ("hnsw_entry",points[top].id.clone()),
        ("hnsw_max_layer",layer.to_string()),
        ("hnsw_digest",format!("{:x}",digest.finalize())),
    ]{
        tx.execute("INSERT INTO metadata(key,value) VALUES(?1,?2)",params![key,value])?;
    }
    Ok(())
}
/// The original generation index checksum pins the source embeddings. This
/// additional checksum pins EVERY persisted graph edge, so an on-disk change
/// cannot secretly steer accelerated retrieval past legitimate candidates.
pub(super) fn validate(conn:&Connection)->PipelineResult<()>{
    let declared:Option<String>=conn.query_row(
        "SELECT value FROM metadata WHERE key='hnsw_digest'",[],
        |r|r.get(0)).optional()?;
    let Some(declared)=declared else{return Ok(());};
    let version:String=conn.query_row(
        "SELECT value FROM metadata WHERE key='hnsw_version'",[],
        |r|r.get(0))?;
    if version!="1" {
        return Err(PipelineError::InvalidIndex("Unsupported HNSW generation version".into()));
    }
    let mut digest=Sha256::new();
    let mut stmt=conn.prepare(
        "SELECT chunk_id,layer,links_json FROM hnsw_nodes ORDER BY chunk_id"
    )?;
    let rows=stmt.query_map([],|r|
        Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?))
    )?;
    let mut counted=0usize;
    for row in rows{
        let (id,layer,links)=row?;
        if layer<0||layer as usize>MAX_LAYER{
            return Err(PipelineError::InvalidIndex("Corrupt HNSW layer".into()));
        }
        let links_parsed:Vec<Vec<String>>=serde_json::from_str(&links)?;
        if links_parsed.len()!=layer as usize+1 ||
           links_parsed.iter().any(|level|
               level.len()>MAX_NEIGHBORS||level.iter().any(|id|id.len()>256)
           ){
            return Err(PipelineError::InvalidIndex("Corrupt HNSW links".into()));
        }
        digest.update(id.as_bytes());
        digest.update((layer as u32).to_le_bytes());
        digest.update(links.as_bytes());
        counted+=1;
    }
    let indexed:i64=conn.query_row("SELECT COUNT(*) FROM chunks",[],|r|r.get(0))?;
    if counted<MIN_ACCELERATED||counted as i64!=indexed||
       format!("{:x}",digest.finalize())!=declared{
        return Err(PipelineError::InvalidIndex("HNSW edge integrity digest mismatch".into()));
    }
    Ok(())
}

#[derive(Clone)]
struct DiskPoint{
    links:Vec<Vec<String>>,
    vector:Vec<f32>,
    source_id:String,
}
fn load(conn:&Connection,key:&str,dim:usize,
    cache:&mut HashMap<String,DiskPoint>)->PipelineResult<DiskPoint>{
    if let Some(node)=cache.get(key){return Ok(node.clone());}
    let record:(i64,String,Vec<u8>,String)=conn.query_row(
        "SELECT h.layer,h.links_json,c.vector,c.chunk_json
         FROM hnsw_nodes h JOIN chunks c ON c.chunk_id=h.chunk_id
         WHERE h.chunk_id=?1",
        [key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
    ).map_err(|e|PipelineError::InvalidIndex(format!("HNSW node unavailable: {e}")))?;
    let (layer,links_raw,raw,chunk_raw)=record;
    let links:Vec<Vec<String>>=serde_json::from_str(&links_raw)?;
    if layer<0||layer as usize>MAX_LAYER||
       links.len()!=layer as usize+1||
       links.iter().any(|r|r.len()>MAX_NEIGHBORS||r.iter().any(|id|id.len()>256)){
        return Err(PipelineError::InvalidIndex("HNSW graph adjacency malformed".into()));
    }
    let chunk:KnowledgeChunk=serde_json::from_str(&chunk_raw)?;
    if chunk.chunk_id!=key{
        return Err(PipelineError::InvalidIndex("HNSW chunk ID mismatch".into()));
    }
    let point=DiskPoint{links,vector:decode_vector(&raw,dim)?,source_id:chunk.source_id};
    cache.insert(key.to_string(),point.clone());
    Ok(point)
}
fn greedy_disk(conn:&Connection,query:&[f32],dim:usize,
    mut current:String,layer:usize,cache:&mut HashMap<String,DiskPoint>,
    cancel:&CancellationToken)->PipelineResult<String>{
    let mut best=similarity(query,&load(conn,&current,dim,cache)?.vector);
    for _ in 0..1_000{
        check_cancelled(cancel)?;
        let links=load(conn,&current,dim,cache)?.links.get(layer).cloned()
            .ok_or_else(||PipelineError::InvalidIndex("Missing HNSW upper layer".into()))?;
        let mut next=current.clone();
        let mut score=best;
        for id in links{
            let s=similarity(query,&load(conn,&id,dim,cache)?.vector);
            if s>score||(s==score&&id<next){next=id;score=s;}
        }
        if next==current{return Ok(current);}
        current=next;best=score;
    }
    Err(PipelineError::InvalidIndex("HNSW greedy traversal exceeded bound".into()))
}
fn beam_disk(conn:&Connection,query:&[f32],dim:usize,entry:String,
    ef:usize,cache:&mut HashMap<String,DiskPoint>,
    cancel:&CancellationToken)->PipelineResult<Vec<(String,f32)>>{
    let initial=(entry.clone(),similarity(query,&load(conn,&entry,dim,cache)?.vector));
    let mut seen=HashSet::from([entry]);
    let mut pending=vec![initial.clone()];
    let mut found=vec![initial];
    for _ in 0..ef*16{
        if pending.is_empty(){break;}
        pending.sort_by(|a,b|b.1.total_cmp(&a.1).then_with(||a.0.cmp(&b.0)));
        let current=pending.remove(0);
        if found.len()>=ef&&current.1<found.last().map(|x|x.1).unwrap_or(f32::NEG_INFINITY){break;}
        check_cancelled(cancel)?;
        for next in load(conn,&current.0,dim,cache)?.links[0].clone(){
            if !seen.insert(next.clone()){continue;}
            let score=similarity(query,&load(conn,&next,dim,cache)?.vector);
            if found.len()<ef||score>=found.last().map(|x|x.1).unwrap_or(f32::NEG_INFINITY){
                pending.push((next.clone(),score));found.push((next,score));
                found.sort_by(|a,b|b.1.total_cmp(&a.1).then_with(||a.0.cmp(&b.0)));
                found.truncate(ef);
            }
        }
    }
    Ok(found)
}
/// None means old index/no accelerated graph: retain exact search behavior.
/// Some(result) means this immutable generation's built HNSW was used.
pub(super) fn query(conn:&Connection,needle:&[f32],dim:usize,limit:usize,
    excluded:&BTreeSet<String>,cancel:&CancellationToken)
    ->PipelineResult<Option<Vec<(String,f32)>>>{
    if excluded.len()>32{return Ok(None);}
    let entry:Option<String>=conn.query_row(
        "SELECT value FROM metadata WHERE key='hnsw_entry'",[],
        |r|r.get(0)).optional()?;
    let Some(mut entry)=entry else{return Ok(None)};
    let layer_raw:String=conn.query_row(
        "SELECT value FROM metadata WHERE key='hnsw_max_layer'",[],
        |r|r.get(0))?;
    let layer=layer_raw.parse::<usize>().map_err(|_|
        PipelineError::InvalidIndex("Invalid HNSW max layer".into()))?;
    if layer>MAX_LAYER||limit==0{
        return Err(PipelineError::InvalidIndex("Invalid HNSW search controls".into()));
    }
    let mut cache=HashMap::new();
    for l in (1..=layer).rev(){
        entry=greedy_disk(conn,needle,dim,entry,l,&mut cache,cancel)?;
    }
    let candidates=beam_disk(conn,needle,dim,entry,
        SEARCH_BEAM.max(limit*4).min(512),&mut cache,cancel)?;
    let mut matched=Vec::new();
    for (id,score) in candidates{
        if !excluded.contains(&load(conn,&id,dim,&mut cache)?.source_id){
            if !score.is_finite(){
                return Err(PipelineError::InvalidIndex("Non-finite HNSW score".into()));
            }
            matched.push((id,score));
        }
    }
    matched.sort_by(|a,b|b.1.total_cmp(&a.1).then_with(||a.0.cmp(&b.0)));
    matched.truncate(limit);
    Ok(Some(matched))
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn stable_layers_and_ranked_candidates_preserve_determinism(){
        assert_eq!(level_for("hello"),level_for("hello"));
        assert!(level_for("other")<=MAX_LAYER);
        assert_eq!(best_first(vec![(4,0.5),(2,0.9),(7,0.1)],2),
            vec![(2,0.9),(4,0.5)]);
        assert_eq!(similarity(&[1.0,0.0],&[0.0,1.0]),0.0);
    }
}
