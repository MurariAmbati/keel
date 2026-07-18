# KEEL

A relational database engine, built from an empty file toward a credible TPC-H
fight — a row-store OLTP engine whose center of gravity is the **crash campaign**:
a fault-injecting disk that tears and drops writes at power-loss boundaries, so
every durability claim is earned against an adversary, not asserted.

> Codename KEEL — the spine everything hangs off. (Working alternates: GRANITE,
> STRATA.)

This repository is the living build of the [design brainstorm](docs/). It is
**not** a toy re-implementation of one idea; it is the storage engine assembled
subsystem by subsystem, each landing with its fuzz oracle and its `dbcheck` rule,
under the house laws in [`docs/decisions.md`](docs/decisions.md).

## Status

**P0–P9 (core) are built, tested, and green**: the storage engine (pages, heap,
B+-tree), the full ARIES recovery ladder (redo-only → steal+FPW →
undo+CLRs+checkpoints), a SQL front end over durable storage (`keel sql`), **three
independent SELECT executors** (reference / streaming Volcano / vectorized) that
provably agree, durable secondary indexes (`CREATE INDEX` → B-tree point **and
range** lookups), a statistics/optimizer layer with the **q-error** metric and
cost-based access-path selection, the **MVCC snapshot-isolation** visibility core
with the write-skew exhibit (and its **serializable-SI** counterpart that forbids
it), a **tuple-vs-vector benchmark** (~2.1×), and a strict **2PL lock manager** with
deadlock detection, full `DELETE`/`UPDATE` with index maintenance, and a streaming
**hash join**, `DROP TABLE`/`DROP INDEX`, a **SQL-level crash campaign**, and a
**full-lifecycle differential**, **DML routed through a logical WAL**, and
**multi-statement transactions** (atomic commit/rollback), **3-/4-way joins**,
streaming **hash aggregation**, **cost-based join reordering**, transaction
**read-your-writes**, the lock manager **and MVCC under real threads** with **vacuum**, a **TPC-H
subset** (Q1/Q3/Q6), **torn-safe log compaction**, and a **`Send` engine** (shareable
across threads, with the **full SQL stack proven under real concurrent threads**).
**212 tests** across unit, property, fuzz-vs-model, crash campaigns, a storage
differential, a two-engine executor differential, TLP metamorphic fuzzing, a
q-error calibration test, an exhaustive MVCC-visibility matrix, and a vectorized
parity differential — clean `clippy`, plus `keel demo`, a `keel sql` REPL, and
`keel-bench`.

| Phase | Subsystem | State |
|---|---|---|
| P0 | `vfs` (sole I/O path) + `faultfs` (fault injector) | ✅ done |
| P0 | `page` (8 KB slotted page, CRC32, stable slots) + `pageview` | ✅ done |
| P0 | deterministic `rng`; `campaign` build profile; CI | ✅ done |
| P1 | `buffer` (CLOCK, RAII guards, WAL-before-data seam) | ✅ done |
| P1 | `types` (values + record codec); `keys` (normalized keys, D9) | ✅ done |
| P1 | `heap` (RIDs, forwarding stubs, FSM) + fuzz-vs-model | ✅ done |
| P1 | `dbcheck` v0 + crash campaign v0 | ✅ done |
| P2 | `btree` B+-tree (single-threaded) + range scans + invariant checker | ✅ done |
| P3 rung 1 | `wal` redo-only WAL, no-steal/force, redo recovery + bank crash campaign | ✅ done |
| P3 rung 2 | steal + no-force + FPW (full-page images); torn-write survival | ✅ done |
| P3 rung 3 | full ARIES: undo + CLRs + checkpoints; `recover_aries`, recovery-of-recovery | ✅ done |
| P4 | `sql` lexer/parser + reference engine (3-valued NULL); `db` storage engine + differential | ✅ done |
| P5 | streaming Volcano executor (2-engine differential) + TLP fuzzer; `CREATE INDEX` + B-tree point/range lookups | ✅ core done |
| P5 | `DELETE` / `UPDATE` over the heap with index maintenance; **write-path differential** vs the oracle | ✅ done |
| P5 | streaming **hash join** (inner + left equijoin), left-deep; **join differential** vs the oracle | ✅ done |
| P5 | streaming **hash aggregation** (GROUP BY / HAVING / 5 aggregates); **aggregate differential** vs the oracle | ✅ done |
| P6 | **cost-based join reordering** (Selinger left-deep subset-DP over exact NDVs) + `join_order` mini-EXPLAIN | ✅ done |
| P4 | `DROP TABLE` / `DROP INDEX` — full catalog lifecycle, persists across reopen | ✅ done |
| P7.4 | **full-lifecycle differential**: random CREATE/INSERT/UPDATE/DELETE/DROP vs the oracle, 8-query battery | ✅ done |
| P8 | `keel-qbench` — end-to-end query benchmark (scan / indexed lookup / hash join) over the durable engine | ✅ done |
| P8 | **TPC-H subset** (Q1/Q3/Q6-flavored) correctness vs the oracle; join→aggregate pipeline | ✅ done |
| P8 | `keel-tpch` — Q1/Q3/Q6 timed at scale (Q6 ~104 ms, Q1 ~324 ms, Q3 ~755 ms @ 100k lineitem) | ✅ done |
| P7.3 | **SQL-level crash campaign**: committed statements + index + catalog survive power loss (benign) | ✅ done |
| P3+ | **DML through a logical WAL** (`open_logged`): log-before-data + no-steal; SQL redo crash campaign | ✅ done |
| P3+ | **torn-safe log compaction** (`compact`): append-only snapshot bounds recovery; crash-mid-compact ignored | ✅ done |
| P7+ | **multi-statement transactions** (`BEGIN`/`COMMIT`/`ROLLBACK`): atomic commit, crash-atomic, rollback, read-your-writes | ✅ done |
| P6 | `stats` — ANALYZE (HLL + histograms), selectivity, **q-error metric**, cost-based access-path | ✅ core done |
| P8 | `mvcc` — snapshot-isolation visibility (exhaustively tested), first-updater-wins, **write-skew exhibit** | ✅ core done |
| R1 | `mvcc::SsiStore` — serializable SI: rw-antidependency tracking + dangerous-structure abort (**write-skew forbidden**) | ✅ done |
| P9 | `vexec` vectorized executor (3rd differential engine) + `keel-bench` tuple-vs-vector ablation (~2.1×) | ✅ core done |
| P7 | `lockmgr` — strict 2PL lock table (IS/IX/S/SIX/X) + waits-for deadlock detection | ✅ core done |
| P7 | lock manager **under real threads**: 8-thread transfer stress, money conserved, deadlocks survived | ✅ done |
| P8 | MVCC **under real threads**: 8-thread first-updater-wins stress (caught & fixed KEEL-0003) | ✅ done |
| P8 | MVCC **vacuum** — reclaims aborted + superseded versions, safety-checked against active readers | ✅ done |
| P7 | engine made **`Send`** (`wal` `Rc→Arc/Mutex`); `Database` shareable across threads, 8-thread writer stress | ✅ done |
| P7 | **full SQL stack under real threads**: 6-thread indexed bank transfers (SELECT+UPDATE+index maint.), money conserved | ✅ done |
| P7 | `latch` — the concurrency **protocol**, built & race-proven in isolation: atomic find-or-install directory + per-page RW latches + ordered two-latch acquisition + pin/evict handshake (`Frame`) + a full **concurrent CLOCK page cache** (`ClockPool`) | ✅ done |
| P7 | `cbuffer` — first **disk-backed concurrent page cache** over `vfs`: reserve → **I/O outside the lock** → publish, `loading`-flag find-or-load; **dirty-page eviction under WAL-before-data**, two-copies-safe; two 80–180k-fetch race oracles (one with a WAL-violation-counting disk) + a **24-seed crash campaign** (checkpoint survives vicious power loss byte-exact); **`new_page` concurrent allocation** (3k-allocation race, unique ids, persist across reopen) | ✅ done |
| P7 | `ckv` — a durable, **concurrent hash-bucket KV over `cbuffer`** (the integration reference): per-bucket RW latch, CRC'd pages, all four lenses (HashMap differential + 120k-increment no-lost-update + 24-seed crash campaign with torn-detection) | ✅ done |
| P7 | `cheap` — the **record heap itself on the concurrent buffer**: real `SlottedPage` pages + stable `(page,slot)` RIDs, growing via `new_page`, lock-ordered insert-frontier hint, `seal()` for a deterministic barrier; differential-vs-`HashMap` + 2.4k-insert concurrent race + 24-seed crash campaign (committed records survive on **every** seed) | ✅ done |
| P7 | **adversarial review of `cheap`/`cbuffer`** (4 lenses → 13 findings → refutation → 4 confirmed): found & fixed **KEEL-0004** stale checksum after compact-then-PageFull, **KEEL-0005** failed flush marked a page clean, **KEEL-0006** concurrent-checkpoint dirty-flag race — first two with revert-verified regression tests | ✅ done |
| P7 | **adversarial audit of the concurrent foundation** (`ckv`/`latch`/`cbuffer`, 13 raised → 9 confirmed / 4 refuted, all with deterministic repros): fixed **KEEL-0007** cross-page corruption that defeated the checksum layer (`flush_all` wrote a repurposed frame to the evicted page's offset), **0008** checkpoint skipping dirty mid-eviction pages, **0009** read-failure abort serving a corrupt page as a hit, **0010** `ckv` never consulting its own CRC + re-sealing torn pages, **0011** failed allocation stranding a page id, **0012** a crash test that passed with checksum verification removed → replaced by a falsifiable positive detection test | ✅ done |
| P7 integ 1 | **`PageFormat` — the cache owns the checksum** (D-PAGER-1): `cbuffer` now stamps every write and verifies every read (`CacheError::Corrupt`), so no caller can forget — the structural cure for the KEEL-0004/0010 class, since `keel-buffer` always did this centrally and `cbuffer` did neither. `keel_page()` vs `opaque()` formats; scratch-copy stamping keeps I/O off the read guard | ✅ done |
| P7 integ 2 | **policy + recovery primitives** (D-PAGER-2): `set_no_steal` (dirty frames are not eviction candidates), `invalidate` (abort drops a page unflushed), the **DPT** (`note_dirty`/`dpt_snapshot`, oldest-recLSN-wins, cleared on flush), `fetch_for_redo` (rebuilds a torn/missing page blank + extends the watermark), `flush_page`, `sync`. `fetch`/`fetch_for_redo` share one `fetch_mode`, and `flush_page`/`flush_all` share one `flush_one`, so the skeleton that hosted KEEL-0007/0008 exists in a single copy | ✅ done |
| P7 integ 3 | **`Pager` seam + pool differential** (D-PAGER-3): closure-scoped byte access (a returned guard would be self-referential for `cbuffer`), impl'd for BOTH pools; one generic workload run through each and compared, plus a write-with-`BufferPool`/read-with-`PageCache` test proving the on-disk formats are interchangeable. Caught and closed a sensitivity gap: `compact()` *repairs* a zero-initialised header, so contents-only comparison missed an injected divergence — fixed by sampling each page header at allocation | ✅ done |
| P7 integ 4a | **`btree` generic over `P: Pager`** (D-PAGER-4a), `BufferPool` as the default type param so `db` and every existing test compile **untouched** — zero call-site changes outside the crate. Proven by `on_cbuffer.rs`: the *same* B-tree driven over both pools (3k shuffled inserts → multi-level splits, deletes, scans, ranges, probes, invariant check) must agree exactly, plus checkpoint+reopen on `PageCache` | ✅ done |
| P7 integ 4b | **`heap` generic over `P: Pager`** (D-PAGER-4b) — the riskier half (FSM rebuild scanning every page; the forwarding-stub path that touches two pages for one operation). The guard-scoping check came back clean: `heap` already copies bytes out and drops the guard before every multi-page step, so all 11 sites converted mechanically. `db`/`wal`/`dbcheck` untouched. `on_cbuffer.rs` drives the same heap over both pools straight through those paths (relocations + stubs, `forward_hops > 0`, reopen re-running the FSM rebuild) | ✅ done |
| P7 integ 5a | **`RecoveryPager`** (D-PAGER-5a): the recovery surface (`set_no_steal`, `invalidate`, DPT, redo-fetch, `flush_page`, `sync`) as a **separate trait extending `Pager`**, so `heap`/`btree` stay generic over the small surface and cannot reach for a recovery primitive. Impl'd for both pools and **compared before migration** — `recovery.rs` requires agreement on DPT semantics, invalidate, no-steal refusal, redo-rebuild contents, and the watermark | ✅ done |
| P7 integ 5b | **THE WHOLE SQL ENGINE ON THE CONCURRENT CACHE** (D-PAGER-5b): `Database<P: RecoveryPager = BufferPool>`; `shell`/`bench`/`dbcheck` and all 34 db tests compile **untouched**, with `Database::with_pager` as the seam. `on_cbuffer.rs` runs DDL + 400 inserts + secondary index + UPDATE + DELETE + ANALYZE + indexed lookup + join/aggregate + ordered range + COUNT through **both** pools requiring identical results, then checkpoints and reopens on `PageCache` | ✅ done |
| P7 integ 5c | **`wal` on the concurrent cache — ENGINE SWAP COMPLETE** (D-PAGER-5c): `TxnStore<P: RecoveryPager = BufferPool>`; both crash campaigns and the ARIES ladder unaffected. The delicate `write` (holds the page guard across the log append) now makes the lock order **explicit in the code** — page-buffer → log, matching `cbuffer`'s flush path, so no cycle. `on_cbuffer.rs` compares create/commit/**abort-with-CLRs**/checkpoint on both pools. `heap`, `btree`, `db`, `wal` all now run on either pool, chosen at the type level, with a differential at every layer | ✅ done |
| P7 integ | **crash campaign on the concurrent path**: the engine swap proved equivalence under *benign* conditions, but every existing campaign ran on `BufferPool`. `concurrent_crash.rs` runs the SQL crash campaign against `Database<PageCache>` — 12 seeds, power loss at a sync boundary, catalog + rows + secondary index all survive, `dbcheck` clean | ✅ done |
| P7 integ | **honest boundary of the swap** (D-PAGER-6): `PageCache` is `Send+Sync`, but `Database<PageCache>` is **`Send`, `!Sync`** — its own `RefCell`/`Cell` fields, not the pool, are what still serialize SQL. Verified by compiler error, not asserted. The swap removed the *storage* layer as a blocker; concurrent SQL is separate work | ✅ done |
| bench | TPC-H vs SQLite/DuckDB, the loss-analysis study | ⏳ next |
| P7–P8 | latching, 2PL, MVCC-SI | ⏳ |
| P9–P10 | vectorized executor, benchmarks, write-up | ⏳ |

The [crash campaign](crates/dbcheck/tests/crash_smoke.rs) already establishes the
P1 durability boundary: a checkpoint is a hard barrier (committed data survives a
power loss byte-for-byte), and under vicious tearing **corruption is never
silent** — every torn page is caught by its checksum, and any dangling forward is
only ever a consequence of a detected torn page. Turning "detected" into
"repaired" is the P3 ARIES ladder.

The first real bug the campaign caught is written up in
[`docs/buglog.md`](docs/buglog.md) (KEEL-0001).

## Quickstart

```sh
cargo test --workspace                    # 212 tests: unit + property + fuzz + crash campaigns + threaded
cargo test --workspace --profile campaign # same 212, optimized + assertions + overflow checks ON — all green
cargo clippy --workspace --all-targets    # clean

# SQL end to end over durable storage (statements from stdin, `;`-separated)
printf "CREATE TABLE t (id INT, g VARCHAR(4), v DOUBLE);
INSERT INTO t VALUES (1,'a',9.5),(2,'a',8.0),(3,'b',6.0);
SELECT g, COUNT(*), AVG(v) FROM t GROUP BY g ORDER BY g;" | cargo run -p keel-shell -- sql /tmp/keel.db

# SQL over the logical WAL (durable via redo log), with .compact / .analyze
printf "CREATE TABLE t (id INT, v INT);
INSERT INTO t VALUES (1,10),(2,20);
.compact;
SELECT id, v FROM t ORDER BY id;" | cargo run -p keel-shell -- sqllog /tmp/keel.db /tmp/keel.wal
# reopen the same files -> 'recovered: replayed N log record(s)' -> state intact

# Storage stack: 5000 typed rows -> heap -> checkpoint -> reopen -> scan -> dbcheck
cargo run -p keel-shell -- demo /tmp/keel.db 5000

# Forensics
cargo run -p keel-pageview -- /tmp/keel.db 0   # schema-aware hexdump of page 0
cargo run -p keel-dbcheck  -- /tmp/keel.db     # validate every invariant
```

## Crate map

```
crates/
  vfs/       BlockFile trait + OsFile + MemDisk       (D11: the sole I/O path)
  faultfs/   the crash-campaign adversary             (torn/drop/reorder, seeded)
  rng/       deterministic PRNG                        (Xoshiro256++)
  page/      8 KB slotted page, CRC32, stable slots    (D4; the unsafe quarantine)
  types/     values, schema, tuple record codec        (D10 scalars)
  keys/      normalized memcmp-comparable key codec     (D9)
  buffer/    CLOCK buffer pool, RAII guards, WAL seam   (D3, §2.3)
  heap/      heap file, RIDs, forwarding stubs, FSM     (D8, §2.2)
  btree/     B+-tree: split/merge, range scan, checker  (§3, D8)
  wal/       ARIES WAL: log_and_apply, TxnStore, recover_aries (§4, D5)
  mvcc/      snapshot-isolation visibility + write-skew exhibit (§5, D6/D7)
  lockmgr/   strict 2PL lock table + deadlock detection      (§5.1, D6)
  latch/     concurrency protocol: find-or-install + RW latches + pin/evict + CLOCK pool (§2.3, D-LATCH)
  cbuffer/   concurrent page cache over vfs: I/O outside the lock, dirty+WAL, crash campaign (§2.3, D-LATCH)
  ckv/       durable concurrent hash-bucket KV over cbuffer — the integration reference (D-LATCH-7)
  sql/       lexer + parser + reference engine (3-valued) (§6, §7.1, D10)
  stats/     ANALYZE (HLL + histograms), selectivity, q-error (§6.3-6.4)
  vexec/     vectorized (columnar batch) executor          (§9)
  db/        storage engine: catalog, indexes, cost-based SQL (§6, D12)
  bench/     ablation + qbench + TPC-H (keel-tpch) timings  (§8, §9)
  dbcheck/   offline invariant validator + crash tests  (D12, §7.5)
  pageview/  schema-aware page hexdump                   (D12)
  shell/     the `keel` REPL/demo binary                 (D2)
docs/
  decisions.md   append-only; superseding-only
  semantics.md   values, keys, records, durability, crash model
  buglog.md      every campaign-caught bug, minimized
  writeup.md     the compendium (§11): architecture, testing, bugs, numbers, scope
```

## House laws (database edition)

* Every on-disk structure carries a **version + checksum** from birth.
* **All I/O through `vfs`** — no exceptions; the crash campaign's validity depends
  on it.
* Every mutation will go through `log_and_apply` once the WAL exists (P3).
* Every claimed invariant gets its **`dbcheck` rule the same week**.
* Every subsystem lands with its **fuzz oracle**.
* **Every stats counter before every explanation.**
* Spill/eviction paths tested at tiny budgets.
* `decisions.md` is append-only; the bug log is the project's memory.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
