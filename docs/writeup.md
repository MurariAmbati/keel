# KEEL — a compendium (§11)

This is the project write-up the design calls for: what KEEL is, how its subsystems
compose, the testing philosophy that is its center of gravity, the bugs that
philosophy caught, the performance it turns in, and — stated plainly — what is built
versus deferred. It is a companion to [`decisions.md`](decisions.md) (the append-only
design ledger) and [`buglog.md`](buglog.md) (the minimized reproducers).

## 1. What was built

KEEL is a relational database engine assembled from an empty file, subsystem by
subsystem, in Rust — a Cargo workspace of 20 small crates with `unsafe` quarantined
to the page layer (D11). It runs SQL over durable storage, and its center of gravity
is the **crash campaign**: a fault-injecting disk that tears and drops writes at
power-loss boundaries, so every durability claim is earned against an adversary
rather than asserted.

Against the design's three victory tiers:

* **V0 — durability & correctness.** The full ARIES recovery ladder (redo-only →
  steal + full-page-writes → undo + CLRs + checkpoints) is proven byte-exact under
  vicious tearing. The SQL layer has its own crash campaign, a logical WAL
  (`open_logged`) with atomic multi-statement transactions (`BEGIN`/`COMMIT`/
  `ROLLBACK`, read-your-writes, crash-atomic), and torn-safe log compaction. Three
  independent SELECT executors are held equal by a permanent differential.
* **V1 — performance.** Cost-based access-path selection (the q-error metric), a
  Selinger left-deep dynamic program for join ordering, a streaming hash join and
  hash aggregation, a vectorized executor, and end-to-end benchmarks including a
  TPC-H query subset.
* **V2 — research.** Serializable snapshot isolation (R1) that forbids the very
  write skew snapshot isolation permits, exhibited as the identical schedule with
  the opposite outcome.

## 2. Architecture — how the crates compose

```
vfs ──────────────► the sole I/O path (D11); OsFile + MemDisk + the fault injector
 │                    (faultfs) that substitutes for the disk in the crash campaign
page ─────────────► 8 KB slotted page, CRC32 from birth (D4); the unsafe quarantine
buffer ───────────► CLOCK pool, RAII pin guards, the WAL-before-data seam (D3, §2.3)
heap ─────────────► tuples by RID, forwarding stubs held to length 1 (D8, §2.2)
btree ────────────► B+-tree, split/merge, range scans, invariant checker (§3)
keys ─────────────► normalized memcmp-comparable key codec (D9)
wal ──────────────► ARIES: log_and_apply, TxnStore, recover_aries (§4, D5)
sql ──────────────► lexer + recursive-descent parser (frozen grammar, D10) +
 │                    the reference engine — the exact 3-valued NULL oracle (§7.1)
stats ────────────► ANALYZE: HyperLogLog NDV + equi-depth histograms, selectivity,
 │                    the q-error metric (§6.3–6.4)
mvcc ─────────────► snapshot-isolation visibility, first-updater-wins, SSI, vacuum
lockmgr ──────────► strict 2PL, multi-granularity modes, waits-for deadlock detect
vexec ────────────► a vectorized (columnar-batch) executor — the third SELECT engine
db ───────────────► the storage-backed engine: self-hosting catalog, indexes, the
 │                    streaming executor (join reordering, hash join, aggregation),
 │                    the logical WAL, transactions, cost-based planning
bench ────────────► the ablation, the end-to-end qbench, the TPC-H timings
dbcheck / pageview ► the offline invariant validator and schema-aware page hexdump
shell ────────────► the `keel` REPL / demo binary
```

The load-bearing seam is `vfs`: because **all** I/O goes through `BlockFile`, the
fault injector can stand in for the disk, which is what makes the crash campaign
valid. The second is the `sql` reference engine: it is the slow, obviously-correct
executor every other path is differenced against.

## 3. The testing philosophy — the actual product

Every subsystem lands with an oracle. The tests are not an afterthought; they are the
reason to trust any claim. Four lenses, each with a distinct blind spot the others
cover:

1. **Fuzz-vs-model.** Heap vs a `Vec`, B-tree vs `BTreeMap`, over thousands of
   adversarial operations. Catches storage-invariant violations.
2. **Differential.** The storage engine vs the reference engine, and the three SELECT
   executors against each other, over random queries and a full random
   CREATE/INSERT/UPDATE/DELETE/DROP lifecycle. Catches semantic errors — NULL logic,
   ORDER BY scope, aggregate reduction, join equivalence.
3. **Crash campaign.** A deterministic, seeded, in-process ALICE disk model tears and
   drops un-synced writes; recovery must reconstruct the committed model byte-for-byte
   (or, before the WAL, must never accept corruption silently). Catches
   recovery-protocol bugs. Runs at both the page level (ARIES) and the SQL level.
4. **Under real threads.** The lock manager and the MVCC store are hammered by 8 OS
   threads with money-conservation as the invariant. Catches concurrency bugs the
   single-threaded scripted tests structurally cannot.

The discipline that ties them together: a claim is not "tested" until an *independent*
implementation disagrees when it is wrong. That is why there are three SELECT engines,
why the reference engine is kept naive, and why the seeds are printed on every failure.

## 4. The bugs — root-cause distribution (§7.5)

Three real bugs were caught and minimized. The distribution over root-cause class is
itself the figure the design asks for:

| Bug        | Subsystem | Root-cause class      | Caught by            |
|------------|-----------|-----------------------|----------------------|
| KEEL-0001  | `heap`    | `storage-invariant`   | crash campaign v0    |
| KEEL-0002  | `sql`     | `null-semantics`      | storage differential |
| KEEL-0003  | `mvcc`    | `visibility` (concurrency) | threaded stress |

```
storage-invariant   █            (1)
null-semantics      █            (1)
visibility          █            (1)
```

The shape is the point: **each bug came from a different lens.** A use-after-free
across the logical-tuple/internal-target boundary (KEEL-0001) is invisible to a
differential but loud under a workload that recycles slots. An ORDER-BY-scope error
(KEEL-0002) is invisible to a fuzzer but immediate against a semantic oracle. An
aborted version poisoning every future update into a livelock (KEEL-0003) is invisible
single-threaded and only surfaces when a real retry loop hits the same row. No single
testing style would have found all three; the campaign's breadth is what did.

## 5. Performance

Honest, KEEL-only numbers (release builds; the tagged-enum `Value` and per-tuple
dispatch cap raw throughput — the caveat is the same one §9 flags):

* **Tuple-vs-vector ablation** (`keel-bench`): the same filter row-at-a-time vs
  batch-at-a-time, on identical data the differential proves equal — **~2.1×**.
* **End-to-end** (`keel-qbench`, 50k orders ⋈ 100k lineitem): filtered scan ~36 ms
  (~1.4 M rows/s), **indexed lookup ~5 ms — ~7× faster than the equivalent scan**
  (the cost model earning its keep), hash join ~210 ms → 100k rows.
* **TPC-H subset** (`keel-tpch`, 100k lineitem): Q6 filtered SUM ~104 ms, Q1 group
  aggregate ~324 ms, Q3 three-way join + aggregate ~755 ms.

The q-error distribution — the headline optimizer number — came out with median ≈1.24
and a heavy p90 tail, the design-predicted shape.

## 6. What is deferred, and why

Everything the design specified as a subsystem is built and tested. What remains is
either a deep architectural refactor or blocked on the environment:

* **Physical prefix-byte reclamation of the log** — the logical WAL's compaction
  bounds *recovery time* (torn-safe, append-only). Reclaiming the dead prefix *bytes*
  needs a file rewrite or page-LSN-gated physical redo under `heap`/`btree` — the
  rung-2/3 machinery. Deliberately deferred.
* **A thread-safe `Database`** — the engine is single-threaded by design (D3). Making
  it `Send` was traced to a single blocker (the buffer's `WalSync` trait object) that
  *cascades*: the `wal` crate's `TxnStore` holds `Rc<RefCell<Log>>`, so real
  thread-safety means refactoring that to `Arc`. The concurrency cores (`lockmgr`,
  `mvcc`) are complete and threaded-proven as standalone services; folding them into
  the engine's execution path is the integration this refactor unlocks.
* **A head-to-head against SQLite / DuckDB** — the correctness of the TPC-H shapes is
  pinned against the oracle and KEEL's own timings are reported, but neither engine is
  installed in this environment, so the comparison itself is deferred.

The through-line of the whole build: correctness is not claimed, it is *earned* — by
an adversary for durability, by an independent oracle for semantics, and by real
threads for concurrency. Where a guarantee is only partial, it is named as partial.
That honesty is the deliverable as much as the code is.
