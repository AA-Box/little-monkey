# HNSW production acceptance

The persisted HNSW graph belongs to Knowledge Stacks 2.0. Its existing 2D
fixture and the older 50k test with **identical vectors** do not demonstrate
production-scale ANN retrieval. The 50k test intentionally uses exact search.

## Real measured gates

`production_hnsw_scale_gate` creates a real immutable SQLite generation via
`HybridIndex::create` from deterministic, seeded, non-identical clustered
embeddings, drops it, and reopens via `HybridIndex::open`. It refuses any
run without the expected persisted `hnsw_nodes` row count.

- Recall@10 against independent full-vector exact cosine ranking.
- Build wall time; disk ANN p50/p95; 8-reader concurrent query throughput.
- Linux process peak resident memory (including corpus/build) and SQLite bytes.
- Full hybrid retrieval and excluded-source correctness, cancellation, and
  exact fallback for candidate requests wider than the bounded ANN beam.
- In `production_hnsw_fresh_process_reopen_gate`, a separate process opens
  the exact same SQLite file and retrieves results; a corrupted COPY is
  independently rejected by immutable index/graph checksum validation.

The workflow `.github/workflows/hnsw-production-acceptance.yml` exercises
100,000 × 384-dimensional vectors on affected PRs and the weekly
schedule. Manual dispatch also supports smaller diagnostic profiles. It saves measured JSON,
the source commit, and host resource details as GitHub Actions artifacts.
Assertions fail CI rather than creating a fictitious success result.

## Reproduce on a fully configured Rust/Tauri build host

~~~sh
export LM_HNSW_DB="$(pwd)/artifacts/hnsw.sqlite3"
export LM_HNSW_REPORT="$(pwd)/artifacts/metrics.json"
export LM_HNSW_COUNT=100000
export LM_HNSW_DIM=384
export LM_HNSW_QUERIES=64
export LM_HNSW_MIN_RECALL=0.80
export LM_HNSW_MAX_P95_MS=250
export LM_HNSW_MAX_BUILD_S=3600
export LM_HNSW_MIN_QPS=8
export LM_HNSW_MAX_RSS_MIB=3500
mkdir -p artifacts
cd src-tauri
cargo test --locked --release --lib production_hnsw_scale_gate -- --ignored --nocapture --test-threads=1
cargo test --locked --release --lib production_hnsw_fresh_process_reopen_gate -- --ignored --nocapture --test-threads=1
~~~

The build gate refuses to overwrite any existing database. To rerun the
measurement remove only that disposable benchmark database first; keep it
between the build and cross-process reopen tests.

## Evidence and release criteria

Passing the deterministic benchmark demonstrates measured scale and
reliability only on the recorded runner hardware. Before approving
production readiness, require: (1) both tests passing on the release
commit, with raw JSON and runner provenance retained; (2) real-model
embeddings and representative real-query relevance judgements confirming
recall targets; (3) repeated sustained workloads on the deployment hardware,
including Apple Silicon if shipped there; and (4) monitored latency, memory,
failure rates, and recovery/rollback to a prior immutable generation.

A scheduled job that has not actually run is not evidence of validation.
A synthetic corpus is not evidence of real-world semantic relevance.
