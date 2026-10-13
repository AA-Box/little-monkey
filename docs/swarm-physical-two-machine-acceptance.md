# Physical two-machine swarm acceptance (operator-owned hardware)

This is a **release blocker**, not a simulated CI smoke test. The GitHub
workflow deliberately does not connect to hardware the project does not own.
The actual machines must be independently installed, paired, fingerprint-
verified out of band, on an operator-controlled network, and use a disposable
read-only resident graph already assigned to an enrolled signed quorum group.

The workflow is manual: `.github/workflows/swarm-live-acceptance.yml`.
Use the **same persistent self-hosted runner**, the **same absolute private
checkpoint evidence path**, the **same frozen graph ID**, peer alias, and cluster
roster for all three stage runs. Do not recreate the graph between stages.

## Required three-stage sequence on machine A

1. On machine B, install and start the resident receiver; verify its TLS
   certificate fingerprint out-of-band. On A, verify the pinned pairing,
   `task` grant and cluster roster. Enable a non-production, read-only
   resident graph on A. Configure a strictly-majority current signed lease
   in the enrolled group. Observe the receiver in its own installed CLI.
2. In GitHub Actions, manually run **Native swarm live acceptance** on
   `swarm-node-a`, selecting `network_stage=baseline` and setting
   `replica_graph_id`, `cluster_group`, `peer_alias` and an absolute
   `network_evidence_path`. A successful baseline requires a real signed
   peer hello, sender's current majority lease, POST acknowledgement, and
   fresh pinned-TLS GET of the remote durable SQLite replica.
3. Externally isolate **only** the A–B network path without revoking the
   pairing, stopping the daemon, or changing a model/workspace/cluster grant.
   Independently record the physical cut (switch port, router ACL or other
   operator-controlled infrastructure) with timestamps and the two host
   identities. Run the **same** workflow with `network_stage=partition`.
   Unlike a baseline job, this stage **must not** run the peer task or
   attempt a new cluster election. A fresh signed probe must fail. A CLI
   failure alone cannot distinguish isolation from a service crash: the
   independent operator evidence is mandatory.
4. Restore the same network path and independently observe that both
   installations remain alive and paired. Run the **same** workflow with
   `network_stage=restore` using the original evidence path. It refuses
   a changed sender identity, missing operator partition checkpoint, an
   expired/absent majority lease or a non-advancing durable replica
   sequence. The workflow then runs **30 readback-verified checkpoint
   publications** by default on one preapproved graph, with no synthetic ACKs.
   For **actual parallel stress**, separately prestage three more operator-
   approved read-only graphs assigned to the same cluster and set
   `stress_concurrency=4` and `stress_additional_graphs=graph-b,graph-c,graph-d`.
   Each concurrent worker must have its own graph identity: the receiving
   SQLite store correctly rejects stale out-of-order snapshots on one key.
   The gate requires every POST and exact remote SQLite readback, distinct
   sequences **per graph**, a stable sender roster, a stable receiver identity,
   100% successful publications and p95 at most 30 seconds.
5. Repeat on machine B with `swarm-node-b`, a separate readonly DAG,
   the reverse paired alias and a different private evidence path. Then
   exercise vector writer convergence, cancellation under load, a
   long-running task interrupted mid-result, and observed terminal
   outcomes using the existing dedicated acceptance runners.

### Direct local post-recovery gate

On A **after the baseline/partition/restore evidence is complete**:

```bash
node scripts/swarm-replica-stress-live-acceptance.mjs \
  --group enrolled-test-group --graph preapproved-graph-a --peer node-b \
  --rounds 30 --concurrency 4 \
  --additional-graphs graph-b,graph-c,graph-d \
  --confirm-operator-owned-readonly-cluster \
  --confirm-independent-machines --confirm-post-recovery
```

Before physical tests, contracts may be run without hardware:

```bash
node --test scripts/swarm-replica-stress-acceptance.contract.test.mjs
```

Those contracts use a controlled fake CLI and **do not count** as hardware
acceptance. The real script executes only the operator's installed `monkey`
CLI against a previously enrolled peer. It does not install binaries, create
pairings or new grants, change firewalls, promote models, retry rejected
writes, launch foreign recipes, or modify the repository.

## Required release evidence

Collect both runner hostnames/hardware identifiers, operating systems,
runtime versions, peer certificate fingerprints independently verified by
the operator, graph and roster hashes, the three distinct stage receipts,
physical isolation and restore times/source, 30+ actual stress receipts
summarized by the harness, throughput/p50/p95, accepted/rejected counts,
CLI and daemon errors, and reverse-direction outcomes. Keep the detailed
local operator log and GitHub Actions artifacts. Do not include credentials,
secrets, model prompts, private task output, certificate private keys or
raw user vectors.

A green baseline is not recovery. A green partition stage without an
independent physical cut is not physical partition proof. A green post-restore
stress stage does not establish exactly-once external side effects, live task
migration, malicious-node Byzantine safety, or a durable recovery of an
arbitrary editing workflow. **No live physical run is claimed merely by
merging or reviewing this file.**
