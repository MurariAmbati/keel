# KEEL — Decisions Log (append-only)

A decision changes only by a superseding entry, never by editing an old one. The
twelve headline decisions (D1–D12) are the shape of the whole project; the
`D-*` entries below them record the concrete P1 implementation choices and the
one deviation the machine forced.

---

## D1 — Row store, not column store
The n-ary slotted-page row store is the design that makes ARIES, MVCC, and index
interplay the curriculum. Columnar execution is a satellite, deliberately out of
scope. Consequence accepted openly: the TPC-H fight is asymmetric, and that
asymmetry is the OLAP loss-analysis methodology, not an embarrassment.

## D2 — Embedded library first; no network
KEEL is a linkable set of crates plus a REPL shell (`keel`), SQLite's model. No
auth, sessions, or wire protocol. The decisive payoff: every test calls the
engine as a function — the reference engine, the fault injector, and the fuzzer
are all in-process. The Postgres wire protocol is preserved as extension R6.

## D3 — Single-threaded until phase 7, then real concurrency
Every subsystem is built and campaigned single-threaded first; latching and the
transaction layer arrive together in a dedicated phase over an already-trusted
storage engine. Concurrent B-tree bugs on top of unproven recovery is the
classic project-killer; the phase gate prevents it structurally.

## D4 — Pages: 8 KB, checksummed always, torn-write-safe via full-page images
Every page carries `{checksum, pageLSN, version, flags, type}`. Torn writes are
handled the PostgreSQL way: the first modification of a page after each
checkpoint logs a full-page image, and redo restores from it. FPW is a flag,
default on, its WAL-volume cost a measured number. (FPI logging lands with the
WAL at P3; the page header already carries the flags bit.)

## D5 — ARIES in full — reached by a three-rung ladder
Destination: steal/no-force, physiological redo, logical undo with CLRs, fuzzy
checkpoints. Route: rung 1 redo-only no-steal/force → rung 2 steal+FPW → rung 3
undo+CLRs+fuzzy checkpoints. Each rung passes the full crash campaign before the
next begins. (P1 predates the ladder: there is no WAL yet, so P1 offers
detection, not recovery — see D-BUF-1 and buglog KEEL-0001's context.)

## D6 — Concurrency control: strict 2PL first, then MVCC-SI
Lane T1 (lock manager + strict 2PL + deadlock detection) exercises ARIES undo
for real. Lane T2 (MVCC snapshot isolation) makes reads non-blocking and aborts
cheap. 2PL first is deliberate: it is the only way the undo machinery gets honest
exercise before MVCC makes undo nearly vestigial.

## D7 — Version storage: in-heap chains, newest-first
Postgres-style in-heap versioning over InnoDB undo-log reconstruction or
time-travel stores: every version is a visible tuple a page inspector can show
you. The InnoDB unification is the elegant road not taken.

## D8 — Heap tables + secondary B+-tree indexes, not index-organized tables
Tuples live in heap pages addressed by RID `(page, slot)`; every index — the
primary key included — maps normalized keys to RIDs. Simpler MVCC (versions don't
migrate the tree), simpler ARIES story, one tree implementation for every index.

## D9 — Normalized keys everywhere
All index keys are `memcmp`-comparable byte strings. One encoding decision
deletes a whole family of comparator bugs and makes the B-tree type-oblivious.
See `semantics.md` for the exact codec; the order-preservation property test is
the P2 gate and already passes for all scalar types.

## D10 — The SQL freeze set
`CREATE TABLE/INDEX`, `INSERT`, `SELECT` with `WHERE`, inner/left joins,
`GROUP BY`/`HAVING` with the five aggregates, `ORDER BY`, `LIMIT`, `CASE`,
uncorrelated scalar/`IN` subqueries, `BEGIN/COMMIT/ROLLBACK`. Three-valued NULL
logic implemented exactly. Correlated subqueries, window functions, outer joins
beyond LEFT, ALTER/triggers/views deferred with reasons. (Types and the record
codec exist; the parser/binder/executor land at P4–P5.)

## D11 — Rust; unsafe quarantined
Reinterpreting buffer-pool frames as typed page structures is the only genuinely
`unsafe` operation, and it lives in exactly one crate (`page`) behind a checked
API. All I/O goes through the `vfs` `BlockFile` trait from day one — because the
fault injector is just a second implementation of that trait, and that single
abstraction is what makes the crash campaign possible at all.

## D12 — Self-hosting catalog + first-class forensic tooling
The catalog will live in KEEL's own tables (P4). Two tools ship as part of the
engine, not afterthoughts: `dbcheck`, the offline validator of every invariant
the system claims; and `pageview`, a schema-aware hexdump. Both exist now.

---

## D-VFS-1 — The crash campaign is a deterministic simulation, not a process kill
The design says "kill the process at write/fsync boundaries". We implement that
as an in-process deterministic simulation (the FoundationDB/TigerBeetle ethos):
`FaultDisk` models the disk (durable image survives; volatile un-synced writes
may tear/drop/reorder on `crash()`), and the *harness* owns the crash schedule.
A failure is fully described by `(disk seed, crash schedule)` and replays
byte-for-byte. `OsFile` is the production durability path (real `fsync`) but does
**not** participate in the in-process crash loop. Rationale: real `abort()`-based
crashing is non-deterministic, un-replayable, and can't crash *recovery itself*
at a chosen depth — which is the whole point of §7.3.

## D-PAGE-1 — `page` is the unsafe quarantine, implemented safely for P1
The P1 slotted page uses bounds-checked little-endian byte accessors and
`copy_from_slice` — zero `unsafe`. The crate keeps `unsafe_code = "allow"` to
reserve the boundary for a future measured zero-copy frame reinterpretation, per
D11. Until a benchmark shows the copy matters, safety wins.

## D-PAGE-2 — Page header and slot format frozen for format version 1
32-byte header (`checksum u32 @0`, `pageLSN u64 @4`, `flags u16 @12`,
`type u8 @14`, `version u8 @15`, `slotCount u16 @16`, `freeStart u16 @18`,
`freeEnd u16 @20`, `extra u64 @24`). CRC32/IEEE over bytes `[4, PAGE_SIZE)`. Slot
= `{offset u16, len u16}`; a slot with `offset == 0` is a tombstone (offset 0 is
inside the header and can never be a real tuple offset). Slot indices are stable
across compaction — this is what makes a RID a permanent address.

## D-HEAP-1 — Forwarding chains are held to length one; three record tags
Heap records are tagged `Tuple` / `Forward` (stub → target RID) / `ForwardTarget`.
A stub always points at a real tuple, never at another stub: when a forwarded
tuple must move again, the stub is repointed and the intermediate deleted, so
reads follow at most one hop. A client RID legitimately resolves only to `Tuple`
or `Forward`; if it resolves to `ForwardTarget` the RID is stale (its slot was
recycled internally) and every operation treats it as absent — never touching the
target, which belongs to another stub. See buglog **KEEL-0001**.

## D-HEAP-2 — Free-space map stores exact compactable-free, advisory, rebuilt on open
The FSM holds, per page, the free bytes achievable *after* compaction (current
free + tombstoned space), so deleted space becomes reusable without eagerly
compacting on every delete. It is advisory (a wrong entry costs a retry, never
correctness) and rebuilt by scanning on `HeapFile::open`. `dbcheck` will learn to
validate/rebuild it as its own rule.

## D-BUF-1 — Single-threaded buffer pool with a WAL seam; WAL-before-data is one assert
Per D3 the pool uses `Cell`/`RefCell` (no latches yet); a per-frame `RefCell`
turns any aliasing mistake into a loud panic — the same discipline latches will
enforce at P7. The `WalSync` trait is the seam through which eviction asks "is the
log durable enough to write this page?"; `NoWal` (P1) answers `u64::MAX`, so the
single `flushed_lsn >= pageLSN` assertion in `flush_frame` is vacuously true now
and becomes load-bearing when rung-1 WAL drops in at P3, without touching this
file.

## D-WAL-1 — Rung 1 of the ARIES ladder: redo-only WAL, no-steal/force, serial txns
The `wal` crate implements D5's rung 1. Every page mutation goes through
`log_and_apply` (append redo record → stamp `pageLSN` → apply), so unlogged
mutation is structurally impossible. The buffer pool runs **no-steal** (a dirty
page is never written before commit; enforced by `choose_victim` skipping dirty
victims under `set_no_steal`) and commit does **force** (fsync the log through
the COMMIT record — WAL-before-data — then flush the txn's pages and fsync the
data file). Recovery is a single **redo** pass over committed transactions'
records, `pageLSN < recordLSN`-guarded; with no checkpoints yet (rung 3) it
replays the whole log, so it reconstructs committed state even from an empty or
torn data file — the log is the source of truth. Transactions are serial (D3),
which is what makes no-steal/force clean (no two txns share a dirty page).
Consequences accepted: a single txn's dirty set must fit in the buffer pool (a
real no-steal limitation — setup commits one page at a time), and the redo record
is a **byte-range after-image** (physical within the page); page-logical records
and log truncation are rung-2/3 refinements. Rung 1 is proven by the
bank-accounts crash campaign: across vicious-crash seeds, the recovered file
equals the committed model exactly (money conserved, uncommitted work absent).

## D-WAL-2 — Rung 3: full ARIES (undo + CLRs + checkpoints); checkpoint flushes for now
Rung 3 completes the ladder. Log records carry before-images (undo) alongside
after-images (redo); `Update` and the FPW `FullPage` both do. A runtime abort
under steal performs a physical undo, writing a Compensation Log Record per change
with `undo_next` = the undone record's `prev_lsn` (resumable, never re-undone).
`recover_aries` runs Analysis → Redo (repeat history from min recLSN) → Undo
(losers, newest-first, writing CLRs), and is idempotent across repeated
interrupted runs (recovery-of-recovery). The buffer pool maintains a Dirty Page
Table (`note_dirty`/`dpt_snapshot`, cleared on flush) that a checkpoint snapshots.
**Simplification, recorded:** a *true* fuzzy checkpoint records the DPT without
flushing and relies on a background writer to eventually fsync stolen pages; there
is no background writer until the concurrency phase, so `checkpoint()` flushes and
fsyncs the dirty pages itself (a sharp checkpoint), then records the now-empty DPT.
The DPT/recLSN analysis and checkpoint-bounded redo are the real fuzzy-checkpoint
machinery — adding a background writer makes it fuzzy without changing recovery.
Undo of a page *creation* (deallocation) is deferred (losers in the campaign don't
create pages). Proven by the rung-3 crash campaign: steal/no-force/FPW + periodic
checkpoints + tearing, a durable in-flight loser undone and committed transfers
redone, recovered byte-for-byte to the committed model across seeds, stable under
three recovery passes each.

## D-BTREE-1 — B-tree nodes: shared header, own body; whole-node (de)serialization for P2
B-tree nodes reuse the page header (checksum, LSN, type, sibling link via `extra`)
through `keel_page::raw`, but define their own body — a sorted array of entries —
distinct from the heap's stable-slot layout. Because the checksum convention is
identical (CRC32 over `[4, PAGE_SIZE)`), the buffer pool, `dbcheck`, and the WAL
treat every page uniformly regardless of body. `dbcheck` applies slotted
structural checks only to `Heap` pages; B-tree structure is `btree::check`'s job.
For P2 a node is parsed whole on read and written whole on modify (O(node) per op)
— chosen for obvious correctness; the in-place slotted-node layout is a later
measured optimization. Splits happen at the byte midpoint (keys vary in length);
deletes are lazy (tombstone, tolerate underflow) with merge/redistribute deferred
and occupancy tracked by `check`. Each index is its own file for now (page 0 =
meta holding the root pointer); the catalog unifies files at P4.

## D-SQL-1 — Hand-written lexer + recursive-descent parser; the reference engine is the oracle
The SQL front end (`sql` crate) is a hand-written lexer and recursive-descent
parser over the freeze grammar (D10) — SQL's warts (precedence, `NOT IN` with
NULLs, case-insensitive identifiers folded to lowercase, output-alias vs
source-column ORDER BY) are owned, not generated. The `refengine` module is the
reference engine (§7.1): an in-memory catalog and the naivest `Vec<Row>` executor,
with three-valued NULL logic implemented exactly. It is the semantic oracle the
storage engine is differentially tested against, and it doubles as the executor
the storage engine delegates to until the Volcano executor lands. Buglog KEEL-0002
(ORDER BY over a non-selected column) was the differential campaign's first catch.

## D-DB-1 — Storage engine: self-hosting catalog, table-id-prefixed heap rows; SELECT materializes for now
The `db` crate mounts the SQL front end on real storage. The catalog lives in
KEEL's own heap (D12): table id 0 holds catalog records, so reopening rebuilds the
schema by scanning them. User rows are table-id-prefixed tuples in the same heap.
`CREATE`/`INSERT` are durable via checkpoint (flush + fsync). `SELECT` currently
materializes the referenced tables from the heap and runs the reference engine's
semantics over them — correct, and the differential oracle validates the storage
roundtrip (record encode/decode, catalog, heap scan). The streaming Volcano
executor (tuple-at-a-time iterators over the heap) and routing DML through the WAL
are the next phase (P5); one data file, one heap, in-memory catalog cache.

## D-EXEC-1 — Streaming Volcano executor as an independent second engine (§6.6, §7.1)
The `db::exec` module is a pull-based iterator executor (Scan → Filter → Project →
Distinct → Sort → Limit; Sort/Distinct blocking) — deliberately a *separate*
implementation from the reference engine, so the storage engine has two engines to
differ against. The planner is conservative (single table, subquery-free WHERE,
projection, DISTINCT, LIMIT, ORDER BY over output columns) and returns `None` for
anything else, so `Database::select` falls back to the materializing reference
engine. The two-engine differential (random NULL-heavy predicates, streaming vs
reference) is the §7.1 "two engines" idea realized. The base scan still
materializes from the heap (the heap's scan API is materializing); a true heap row
iterator, plus index scans wiring in the B-tree, joins, and hash aggregation, are
the executor's next increments.

## D-DB-2 — Secondary indexes: shared-file rooted B-trees, RID-suffixed keys, point-lookup
`CREATE INDEX` builds a durable B-tree over one column and wires the B-tree +
normalized-key codec into the query path. The B-tree lives in the same data file
as the heap (a *rooted* tree owning no meta page; `keel_page` page types keep heap
and node pages distinct, and `heap.scan` skips non-heap pages). The index catalog
lives in KEEL's own heap (table id 1); each index's root is rewritten in place when
the tree grows. Index keys are `normalized_column_key ++ 6-byte RID`, so duplicate
column values stay distinct and a `col = literal` lookup is a **prefix range** over
the tree. `INSERT` maintains every index on the table; the planner turns an
comparison conjunct (`= < <= > >=`) on an indexed column into an index scan (a
B-tree key range → candidate RIDs → heap fetch); the streaming filter re-applies
the full predicate, so the result is identical to a full scan (validated for both
point and range lookups). Bounds may over-fetch, never under-fetch. Deferred:
multi-column indexes, an index-only scan (avoiding the heap fetch), and index
maintenance on UPDATE/DELETE (there is no UPDATE/DELETE SQL yet).

## D-STATS-1 — Statistics + q-error: the optimizer's quantitative core (§6.3–6.4)
The `stats` crate computes per-column statistics (`ANALYZE`): null fraction, NDV
via **HyperLogLog** (p=12, ~4096 registers, SplitMix64-finalized hash — FNV alone
has weak high-bit avalanche and HLL leans on the high bits), min/max, and an
equi-depth **histogram** over numeric columns. `estimate_selectivity` turns a WHERE
into an expected surviving fraction: `1/NDV` for equality, histogram interpolation
for ranges, independence for `AND`, inclusion–exclusion for `OR`, `null_frac` for
`IS NULL`. The headline number is the **q-error** — `max(est/act, act/est)` per
query — which makes "how good is the estimator" quantitative (§6.4). Measured
distribution over random data: **median ≈ 1.24, heavy tail (p90 ≈ 265)** — exactly
the design's predicted shape (tight where columns are independent, a long tail
where correlation breaks the independence assumption; the motivation for learned
estimators, R2). `Database::analyze` caches stats and `index_rows` uses them for a
**cost-based access-path choice**: an equality on an indexed column uses the index
only when the estimated selectivity beats the `INDEX_CROSSOVER` (0.2 default); an
unselective range declines the index and full-scans. Deferred: Selinger join-order
DP (needs streaming joins), sampled ANALYZE, correlated-column multi-column stats.

## D-MVCC-1 — Snapshot-isolation visibility core, exhaustively tested; the write-skew exhibit (§5.2–5.3)
The `mvcc` crate is the MVCC semantic heart (D6 lane T2). `visible(version, snapshot,
clog, reader)` decides whether a tuple version `(xmin, xmax)` is visible: the insert
is visible if it is the reader's own or committed-before-snapshot; the delete hides
the tuple if it is the reader's own or committed-before-snapshot. It gets the §5.1
treatment — an **exhaustive matrix test** over every combination of the three txns'
statuses, header `(xmin, xmax)`, snapshot `xmax`/in-flight set, and reader, checked
against derived invariants (aborted inserts never visible, own live inserts always
visible, committed-live-before-snapshot always visible) — thousands of cases, before
it touches data. On top: an in-memory MVCC store with newest-first version chains
(D7), snapshot reads, and **first-updater-wins** (a concurrent write to a row the
newest version of which a concurrent txn created/deleted aborts with WriteConflict).
The **write-skew exhibit** (§5.3, two-doctors-on-call) is a passing test showing SI
*permits* write skew — the boundary documented, not hidden; Serializable SI (SIREAD
locks, rw-antidependency detection) is extension R1. Deferred: wiring versions into
the heap tuple header, running under real latching/threads, and a background vacuum
reclaiming versions below the oldest-active watermark.

## D-VEXEC-1 — Vectorized executor as a third independent engine (§9)
The `vexec` crate is a columnar, batch-at-a-time executor: a `Batch` holds one
`Vec<Value>` per column; `eval_vec` evaluates an expression over the whole batch
per operation (the amortized-dispatch primitive), preserving three-valued NULL
logic; `filter`/`project` are the vectorized operators. It is a *separate*
implementation from the row engines specifically so the tuple-vs-vector ablation
(§9's headline) compares engines that provably agree — the differential test
checks vectorized `filter` equals the reference engine's row-at-a-time result over
random NULL-heavy data (1,200 checks). KEEL now has **three** independent SELECT
executors (reference / streaming Volcano / vectorized), a strong differential base.
The measured tuple-vs-vector speedup on scans, and wiring the vectorized path into
`db` as a selectable engine, are the remaining P9 work.

## D-BENCH-1 — The tuple-vs-vector ablation, self-contained, honestly bounded (§8.3, §9)
`keel-bench` runs the design's headline performance event: the same filter over N
rows, row-at-a-time (per-tuple dispatch via `eval_public`) vs vectorized (batch via
`vexec`), on identical data, both engines being the ones the differential campaign
proves agree (asserted: the two counts match). Reported with §8.3 discipline — N
reps, medians ± MAD, throughput (M rows/s). Measured **~2.1× at 1M rows** (row 2.9
→ vector 6.1 M rows/s). Recorded honestly: because KEEL's `Value` is a tagged enum,
this understates a true columnar engine (native arrays, SIMD) and isolates only the
*dispatch-amortization* portion of the X100 result — the in-scope part. A benchmark
against SQLite/DuckDB (the TPC-H fight, §8.1–8.2) needs those installed and is the
remaining measurement work; the internal ablation is the part KEEL controls end to
end.

## D-VACUUM-1 — MVCC vacuum reclaims dead versions (§5.2)
`MvccStore::vacuum` sweeps the version chains and drops what no transaction can
reach, in two always-safe collections. **Aborted** versions are removed
unconditionally — SI's `visible` requires a committed `xmin`, so an aborted version
is invisible to every reader (this also clears the KEEL-0003 dead-tail buildup that
would otherwise grow without bound). And when **no transaction is active**, every
committed version older than a row's newest committed one is removed — a future
transaction's snapshot only ever resolves to the newest committed version, so the
rest are superseded and unreachable. In-progress versions (an active transaction's
own writes) are always kept, and a row never drops below one retained version. It
returns the count reclaimed. `vacuum_reclaims_dead_versions_safely` proves it
collapses an accumulated chain to a single version, never changes a visible value,
and — the safety half — does **not** prune a version an active reader still sees
(an old open snapshot keeps reading its value while a fresh reader sees the latest).
This is the explicit, testable core; a real engine runs it as a background sweep
keyed off the oldest-active-snapshot horizon, which is the remaining refinement.

## D-MVCC-2 — MVCC proven under real threads (P7/P8) — and it caught a real bug
The exhaustive visibility matrix proves the SI rule in isolation;
`mvcc/tests/threaded.rs` proves **first-updater-wins** actually prevents lost
updates when OS threads race. A bank of accounts lives in one `Mutex<MvccStore>`; 8
threads run 300 random transfers each as MVCC transactions (read both balances from
the snapshot, write, commit), retrying on `WriteConflict` against a fresh snapshot.
The invariant is money conservation — a lost update leaks money — and the test also
counts conflicts (so the race is real, not a serialized fluke) and updates the two
rows in index order (a transfer discipline that serializes contending pairs, the
same reason the lock test sorts). **The test earned its keep immediately: it hung,
exposing KEEL-0003** — `update` tested `versions.last()`, the *physically* newest
version, so an aborted transaction's dead tail version poisoned every future update
with a false `WriteConflict` forever (a livelock). Fixed by checking the newest
*non-aborted* version (`rposition(xmin != Aborted)`); regression pinned by
`aborted_version_does_not_poison_future_updates`. The single-threaded tests never
retried an aborted txn on the same row, so only the real multi-threaded retry loop
surfaced it — exactly why the design mandates the under-real-threads stress.
Deferred: vacuuming the accumulating dead versions (§5.2).

## D-LOCK-2 — The lock manager proven under real threads (P7, §5.1)
The scripted single-threaded tests prove the compatibility matrix and the deadlock
detector in isolation; `lockmgr/tests/threaded.rs` proves the whole thing works as a
concurrency-control *service* under genuine `std::thread` contention. A bank of
accounts is guarded **only** by the lock manager — the data has no mutex of its own
(each account is an `AtomicI64`), so correctness rests entirely on the manager
granting no two conflicting X locks at once. Eight threads run 400 random transfers
each; the invariant checked is money conservation (a broken manager → a lost update
→ a changed total). Two acquisition disciplines: **sorted** (canonical order —
deadlock-free, a pure mutual-exclusion stress) and **random** (cycles form, the
waits-for detector names victims, and the aborted transactions retry with a fresh
id). Both conserve the total exactly, and — the other half of the claim — no thread
hangs (every `join` returns), so `Waiting` grants are always eventually promoted and
deadlocks are always broken. The memory ordering is real, not hand-waved: every
`lock`/`release_all` goes through the shared `Mutex<LockManager>`, whose lock/unlock
ordering establishes the happens-before between a releaser and the next acquirer that
makes the in-critical-section atomic RMWs correct. This is the P7 "under real
threads" milestone for the lock lane; wiring it (with `mvcc`) into the storage
engine's own execution path is the remaining integration.

## D-LOCK-1 — Strict 2PL lock manager + waits-for deadlock detection (§5.1, D6 lane T1)
The `lockmgr` crate is the lock table for lane T1 (the lane that gives ARIES undo
its real exercise, since aborts must physically roll back). Resources are tables or
rows; modes are the multi-granularity set `{IS, IX, S, SIX, X}` with the standard
compatibility matrix. A request grants if compatible with all other holders, else
queues (FIFO, no queue-jumping, so no starvation) and records waits-for edges — and
before queuing, a **waits-for cycle check** runs; a cycle returns `Deadlock{victim}`
naming the youngest transaction on the cycle, and no edge is added. `release_all`
implements strict 2PL (hold to commit/abort) and promotes now-satisfiable waiters.
Built and tested single-threaded first (D3): scripted schedules prove the matrix,
S-coexistence, X-blocking-then-granting, FIFO promotion, and 2-way/3-way deadlock
detection with correct victims. Deferred: lock upgrades (S→X), real sharding, and
wiring it — together with `mvcc` — into the storage engine under actual threads,
the phase the whole single-threaded base (D3) was built to make safe.

## D-TXN-1 — Multi-statement transactions on the logical WAL (deferred-apply, atomic commit)
`BEGIN` / `COMMIT` / `ROLLBACK` — parsed since P4 but no-ops — become real
transactions in logged mode. A `BEGIN` opens a transaction that **buffers** its
mutating statements (in `txn: RefCell<Option<Vec<String>>>`); `COMMIT` writes them
as one committed unit — each an `S`-record, then a single `C` marker, all fsynced —
and only then applies them in order (`commit_batch`); `ROLLBACK` drops the buffer,
having applied and logged nothing. Recovery accumulates `S`-records into a pending
batch and applies it when a `C` marker arrives; a batch with no trailing `C` (a
transaction the crash cut off, or a torn tail) is discarded — so a commit is
**all-or-nothing**: a power loss before the `C` marker is durable erases the whole
transaction. Auto-commit statements (outside `BEGIN`) are just one-statement
committed units (`S` then `C`). The model is *deferred-apply*: a transaction's
mutations land together at commit and compose correctly (later statements see
earlier ones as they apply in order). **Read-your-own-writes** inside an open
transaction is supported by an **overlay** (D-TXN-2): a `SELECT` inside the
transaction runs against the committed state *plus* the transaction's buffered
statements replayed into a throwaway reference database (`select_with_overlay` over
`materialized_memdb`), so it sees its own pending mutations — while the durable
state stays untouched, keeping rollback trivial. Outside logged mode the three verbs
stay no-ops (unchanged). Proven in `wal_crash.rs`: commit is atomic across a crash,
rollback and an uncommitted-at-crash transaction leave no trace, a crash *between*
the statement records and the commit marker discards the batch, and
`transaction_read_your_writes` shows an in-transaction `SELECT` seeing its own
insert+update, that rollback erases it, and that commit makes it durable.

## D-COMPACT-1 — Torn-safe log compaction bounds recovery (append-only snapshot)
The logical WAL grows with history; `Database::compact` bounds *recovery* work
without a non-atomic file rewrite. It appends a **snapshot** — a minimal statement
script (`CREATE TABLE` + `INSERT` every current row + `CREATE INDEX`, emitted in
table-id order so replay reassigns the same ids) that reconstructs the whole
committed state — bracketed by begin (`B`) and end (`E`) marker records. Recovery
(`recover`) finds the **last complete `B…E`**, replays its statements to rebuild the
state, and skips everything before it as superseded; post-snapshot statements then
apply as the usual `S…C` committed batches. This is torn-safe by construction:
because the append is the only mutation and the snapshot is used *only* once its
closing `E` is durable, a crash mid-compact leaves a `B` with no `E` — which
recovery ignores (falling back to the prior history, still intact), and the tail
scan stops at that dangling `B` so its half-written statements are never mis-applied.
Proven in `wal_crash.rs`: `compaction_bounds_recovery_and_preserves_state` compacts
a table with ~130 statements of churn down to a ~52-record snapshot, crashes after
two more statements, and reopens to the exact state with `replay_count() < 70` (the
snapshot + tail, not the full history); `torn_compaction_is_ignored` writes a
dangling `B` + partial snapshot and confirms the original state survives. Honest
scope: this reclaims *recovery time*; physically reclaiming the dead prefix **bytes**
needs a file rewrite or the page-LSN physical redo, and stays deferred — but the
append-only design means the log's *live* content is now bounded by the state size
plus the inter-compaction tail, which is the property that matters for recovery.
The snapshot's value formatting (`value_to_sql`) is hardened by
`compaction_round_trips_all_value_types`: every type and edge case — negative
doubles, i32 min/max, big i64, an embedded `''`-escaped quote, an empty string,
NULLs, bools — survives a compact-then-reopen round-trip exactly (which also confirms
the lexer's quote-escaping, the one place a snapshot could silently corrupt text).
`compaction_round_trip_random_values_vs_oracle` extends this to a 25-seed fuzz —
arbitrary i64s, arbitrary finite f64s (from random bits), and quote/special-laden
random text — reconstructed *only* from the snapshot and matched against the `MemDb`
oracle, confirming `f64::to_string` is round-trippable through the parser for any
finite double and that arbitrary text survives.

## D-WALDB-1 — DML through a logical statement WAL (the SQL-level rung-1)
`Database::open_logged(data, log, frames)` routes mutations through a redo log
instead of a per-statement checkpoint. The mechanism mirrors the page-level rung-1
one layer up: **log-before-data** (every mutating statement is appended to the log
and fsynced *before* it is applied — `execute` logs, then `dispatch` applies) and
**no-steal** (the buffer never flushes data during the session, so the data file
stays at its last checkpoint). The log is therefore the sole durable record; on
open, the catalog is loaded from the data image and the log tail is replayed —
with no durable checkpoint yet, that image is empty and replay rebuilds the entire
committed state, exactly, because KEEL's statements are deterministic. The log is a
CRC-framed append file (`[MAGIC][len][crc32][bytes]`) fsynced per record, so a torn
tail fails its length/CRC check and is dropped whole — a statement is atomically
present or absent, never half-applied. Proven by a SQL redo crash campaign
(`wal_crash.rs`): 16 seeds of a random CREATE/INSERT/UPDATE/DELETE/DROP-recreate
history reconstruct byte-for-byte against the `MemDb` oracle from the durable log
onto a *fresh empty* data disk, plus an explicit torn-tail test. Honestly scoped as
the rung-1 analog: the whole DB lives in the buffer under no-steal, so bounding the
log with a torn-checkpoint-safe compaction (the rung-2/3 step — page-LSN-gated
physical redo under the heap/B-tree) is the remaining durability work. The
checkpoint-durable [`Database::open`] path is unchanged (`persist` = `checkpoint`
when there is no log), so every prior test still holds.

## D-TPCH-1 — TPC-H-shaped analytical subset runs correctly on the full stack
The design's V1 target is a "credible TPC-H fight". The engine now has every piece
the classic queries need — multi-way hash joins with cost-based ordering, hash
aggregation, GROUP BY / HAVING, ORDER BY / LIMIT — so `db/tests/tpch_subset.rs` runs
**Q1-, Q3-, and Q6-flavored** queries over a synthetic TPC-H-lite schema
(customer / orders / lineitem) and checks each against the reference oracle: Q6 a
filtered `SUM`, Q1 a grouped multi-measure aggregate (`COUNT`/`SUM`/`AVG`) ordered,
Q3 a **three-way join** (customer⋈orders⋈lineitem) with a filter, per-order revenue
`SUM`, `ORDER BY`, and `LIMIT`, plus a `HAVING` variant. Integer surrogates stand in
for TPC-H's decimals so the sums are order-independent and the differential is exact.
The `join_streams`/`agg_streams` counters assert the streaming join and aggregate
paths actually served the queries — so Q3 in particular exercises the newly-composed
**join → aggregate** pipeline (fold the tables, then group and reduce), which no
earlier test hit. This is the correctness half of the fight, passing clean. For KEEL's own
*timing* of these shapes, `keel-tpch` runs the same Q1/Q3/Q6 at scale and reports
median ± MAD latency — a representative release run (100k lineitem, 25k orders, 2.5k
customer): **Q6 ≈104 ms, Q1 ≈324 ms, Q3 (3-way join) ≈755 ms**, all served by the
streaming join/aggregate paths. The remaining half — a *head-to-head* against SQLite
and DuckDB — needs those engines installed, which this environment lacks (`which
sqlite3`/`duckdb` both empty); the numbers above are honestly KEEL-only, capped as
ever by the tagged-enum `Value` and per-tuple dispatch (§9).

## D-QBENCH-1 — End-to-end query numbers on the durable engine
`keel-qbench` measures the assembled engine a user runs (SQL → heap → buffer pool →
B-tree index → streaming executors), complementing the tuple-vs-vector ablation that
isolates one effect. It bulk-loads two related tables (batched multi-row INSERTs so
load time is inserts, not per-row fsyncs), builds an index, ANALYZEs, then times a
filtered scan, an indexed point lookup, and a hash join — median ± MAD over reps
(§8.3). Representative release run (50k orders ⋈ 100k lineitem): filtered scan ≈36 ms
(~1.4 M input-rows/s), **indexed lookup ≈5 ms — ~7× faster than the equivalent full
scan** (the cost-based access path earning its keep), hash join ≈210 ms producing
100k rows. These are KEEL's own numbers, honestly scoped: the tagged-enum `Value` and
per-tuple dispatch cap raw scan throughput (same caveat as §9), and a head-to-head
against SQLite/DuckDB — the actual "credible TPC-H fight" — is the next benchmarking
step, not this one. The query correctness underneath is already pinned by the
differentials; this binary adds the performance dimension.

## D-LIFECYCLE-1 — The full-lifecycle differential is the capstone correctness test
The per-surface differentials (SELECT, DML, joins) each pin one thing; `db/tests/
lifecycle_differential.rs` runs the whole SQL surface *in random combination*. Over
30 seeds it drives a random history — CREATE, INSERT (with NULLs), UPDATE (value and
indexed-key), DELETE, and periodic DROP-and-recreate — against the durable storage
engine and the in-memory reference oracle in lockstep on identical SQL, and after
**every** mutation compares a battery of eight read queries: filtered scan, indexed
lookup, DISTINCT, inner join, left join, GROUP BY / HAVING with aggregates, ORDER BY
/ LIMIT, and an IS NULL count. Any divergence in stored state or query semantics —
anywhere across the surface, in any order the fuzzer reaches — surfaces as a
mismatched result set with a replayable seed. An index on the mutated table means
the run also exercises index maintenance under the full DML mix while the oracle
(index-blind) holds results fixed. This is the design's differential-fuzzing
discipline turned on the assembled engine, and it passes clean.

## D-DROP-1 — DROP TABLE / DROP INDEX close the catalog lifecycle
The freeze grammar gains `DROP TABLE t` and `DROP INDEX ix`, so the catalog is no
longer create-only. `DROP TABLE` does one heap scan collecting every rid to remove —
the table's data rows, its own catalog record (matched by parsed table id), and any
index catalog records for it — then deletes them and forgets the table in the
in-memory catalog, index list, and stats cache; the drop persists because the
catalog record is physically gone, so a reopen's catalog rebuild never sees it (a
drop-then-recreate under the same name starts clean — the differently-typed new
table shows only its own rows). `DROP INDEX` deletes just the index catalog record
(rid already held in `IndexMeta`) and drops it from the access-path list, after
which queries correctly revert to full scans. Both error on unknown names. Honest
deferral: heap and B-tree **pages are tombstoned, not reclaimed** — page
deallocation / free-space return is still future work — so a drop frees logical
space, not yet physical file space; `next_tid` only ever advances, so a dropped
table id is never reused.

## D-SQLCRASH-1 — The crash campaign reaches the SQL surface
The page-level campaigns (`crash_smoke`) prove the storage floor; `db/tests/
sql_crash.rs` proves the same durability barrier in user terms: **after a statement
returns, its effect survives a power loss.** It builds a real SQL state (a table, an
index, 200 INSERTs, then an UPDATE and a DELETE — each statement checkpoints, so
each is a durability barrier), pulls the power with a benign `FaultDisk` crash,
reopens from the *durable image*, and requires the exact committed state to read
back: the row count reflects the DELETE, the k=3 rows carry their UPDATE, the index
still serves the k=3 lookup (asserted via the `index_lookups` counter), the whole
self-hosting catalog rebuilds (a two-table variant), and `dbcheck` finds the raw
file well-formed — all across 12 seeds. This is deliberately the *benign* crash at a
sync boundary, because that is exactly the guarantee checkpoint-durability makes:
the current engine is checkpoint-granular, not yet mid-statement-atomic. Closing
that last gap — a torn write *during* a statement's checkpoint leaving a consistent
state — is what routing DML through the page-level ARIES WAL (already built and
proven in `wal`) will buy; the test documents the boundary rather than overclaiming.

## D-JOINORDER-1 — Cost-based join reordering: Selinger left-deep subset-DP
The N-way hash-join fold used to join tables in FROM order; now, when every join is
inner and the ON clauses form a clean equijoin graph, `join_plan` reorders them to
minimize estimated cost. It extracts one edge per ON (each side must resolve in
exactly one table — `sole_table`; an unqualified or straddling column bails to FROM
order), then runs the **Selinger left-deep dynamic program** (`dp_order`): over
table-set bitmasks, `best[S]` is the minimum-cost left-deep order for set `S` and its
estimated cardinality, built by trying each table as the last added to `best[S\{t}]`
(processed in increasing mask order, so every subset is ready before its supersets);
cost is the classic sum of intermediate cardinalities, and an equijoin's selectivity
is `1 / max(ndv_a, ndv_b)` computed *exactly* over the in-memory rows (the streaming
executor already has them materialized). `build_steps` turns the optimal order into
the fold, keying each table on an edge to the joined prefix and applying any further
edges as residual equijoin filters; a disconnected graph (a cross product) or more
than 12 tables (the 2^n guard) bails to FROM order. Correctness is order-independent
and already pinned by the join differentials (a reordered inner-join set is the same
bag as any order) — they pass unchanged with reordering live. That it *fires
sensibly* is proven by `cost_based_join_reordering`: on FROM `big`(200) `JOIN`
`small`(4) `JOIN mid`(30), the DP joins the 200-row `big` table **last** (when the
accumulated result is smallest), differs from FROM order, and — via `join_order`, a
mini-`EXPLAIN` exposing the chosen order — still matches the oracle. Deferred: using
persisted `ANALYZE` NDVs (for when rows aren't all resident), bushy plans, and
outer-join reordering (only inner joins are freely reassociable).

## D-AGG-1 — Streaming hash aggregation (GROUP BY / HAVING / five aggregates)
Grouped and aggregated queries no longer fall back to the reference engine: the
streaming executor gains a hash-aggregate stage (`run_aggregate`) reached whenever
the query has a GROUP BY, a HAVING, or an aggregate in its SELECT list. It filters
by WHERE, groups rows by the GROUP BY key values (first-seen order preserved; an
empty GROUP BY is one group over all rows, so `COUNT(*)` on nothing is 0), reduces
each aggregate over its group (NULLs skipped; `DISTINCT` dedups; SUM stays an i64
wrapping sum unless a float promotes it; AVG is always a double; MIN/MAX by the same
total order the rest of the engine uses), applies HAVING, and projects. The design
choice that keeps it *independent yet consistent*: only the **grouping and the
reducers** are implemented here — every scalar/3-valued/arithmetic sub-expression
still goes through the shared `eval_public`, by rewriting each aggregate node to a
literal of its reduced value and evaluating the whole item against a representative
group row. So the differential still tests new code (grouping + reduction) without
re-deriving NULL logic. Proven by `aggregate_differential.rs`: 30 seeds × 9 shapes
(grouped counts/sums, MIN/MAX/AVG with NULLs, HAVING, whole-table aggregates,
aggregate arithmetic, `COUNT(DISTINCT)`, empty-result `SUM = NULL`, `CASE` over a
group key + aggregate) match the oracle, with an `agg_streams` counter asserting the
streaming path — not the fallback — served every one. Deferred: aggregates over
subqueries (still fall back).

## D-HASHJOIN-1 — Streaming hash join (inner + left equijoin), oracle-gated
The streaming executor gains joins so multi-table queries no longer always
materialize through the reference engine. `try_stream_join` takes the FROM tables
in order and folds them left-deep: for each join it extracts a pure equijoin key
from the ON (`extract_equijoin` — one side must resolve only in the left schema, the
other only in the right, else it declines), builds a `BTreeMap` hash table on the
right side (NULL keys never inserted, so `NULL = NULL` correctly fails to match),
and probes with the left stream; a LEFT join emits NULL-extended rows for unmatched
left tuples. The post-join tail (WHERE / project / DISTINCT / ORDER BY over output
columns / LIMIT) is the **shared `finish` stage** the single-table path now also
uses, so both routes are one code path from projection on. Anything it cannot prove
equivalent — a non-equijoin or straddling ON, aggregates, group-by/having, a
subquery, ORDER BY over a non-projected column — returns `None` and the query falls
back to the reference-engine oracle. Correctness is a **join differential**
(`join_differential_vs_reference`, 25 seeds × 5 shapes: inner, left, right-table
WHERE, DISTINCT, reversed ON), with duplicate keys, NULL keys, and non-matching
rows in the data. A `join_streams` counter asserts every query in the differential
was actually served by the hash-join path (not silently the fallback), so the test
proves the join, not oracle-vs-oracle. Deferred: right/full outer joins, non-equi
join predicates, and cost-based join ordering (Selinger DP).

**Multi-way (addendum).** The left-deep fold is not limited to two tables:
`multijoin_differential.rs` runs random 3- and 4-table joins (chains, a filtered
middle table, and a mixed inner-then-left shape) through the streaming path and
matches the oracle across 25 seeds, with a `join_streams` assertion that the
hash-join path served every one. Join *order* is still the FROM order (reordering is
the deferred Selinger step); this proves the N-way *mechanism* is correct.

## D-DML-1 — DELETE / UPDATE complete the write path, validated by a write differential
The freeze grammar (D10) gains `DELETE FROM t [WHERE p]` and `UPDATE t SET c = e,
… [WHERE p]`. In the storage engine both scan the heap for the target table,
evaluate the predicate row-at-a-time through the reference engine's `eval_public`
(TRUE-only, three-valued — a NULL or FALSE predicate spares the row, exactly as
SELECT), and mutate: DELETE calls `heap.delete` (which keeps forward chains
correct), UPDATE evaluates every right-hand side against the *pre-update* row (SQL
semantics — `SET a=5, b=a` reads the old `a`), coerces, enforces NOT NULL, then
`heap.update` (RID stays stable via forwarding, so index entries keyed by RID stay
valid). Indexes are maintained precisely: DELETE removes each row's key; UPDATE
replaces only the keys whose column value actually changed. Correctness rests on a
**write-path differential** (`dml_differential_vs_reference`): 20 seeds of random
INSERT/UPDATE/DELETE run against both the engine and the `MemDb` oracle, and the
full final table must match. Deliberate limits (honest): the row-at-a-time DML path
rejects subqueries in a DML predicate/RHS (the materializing SELECT path has them;
wiring them here is later), and durability is still `checkpoint`, not the WAL —
routing DML through `log_and_apply` is the remaining integration.

## D-SSI-1 — Serializable SI by dangerous-structure detection, single-abort form (R1, §5.3)
`SsiStore` (in `mvcc`) runs snapshot isolation but tracks each transaction's read
set, write set, and the **rw-antidependency** edges among concurrent transactions
(`T -> U`: T read a version U overwrote). Fekete's theorem: every non-serializable
SI schedule contains a *dangerous structure* — a pivot transaction with both an
inbound and an outbound rw-antidependency, whose successor commits first — and
aborting one such pivot restores serializability. We apply exactly that at commit:
abort iff the committer has an inbound edge **and** an outbound edge to an
already-committed transaction. This is the single-abort form (Cahill 2008), chosen
over the coarser sticky in&out-flag rule because that one aborts *both* parties of a
write skew when aborting one suffices. Sound (never commits a non-serializable
schedule), deliberately not complete (can abort some serializable schedules — the
false-positive rate the design set out to measure; `ssi_allows_benign_concurrency`
and `ssi_read_only_never_aborts` pin the lower bound that it does not abort
everything). The headline result: `ssi_forbids_write_skew` runs the *identical*
two-doctors schedule that `write_skew_is_permitted_under_si` commits, and SSI
rejects it — same operations, opposite outcome, the boundary made executable.
Deferred: SIREAD-lock granularity/GC, running it under real threads with the lock
manager, and measuring the false-positive rate on a workload.

## D-THREADSAFE-2 — The engine is now `Send` (the `wal` `Rc→Arc/Mutex` conversion landed)
The precondition mapped in D-THREADSAFE-1 has been done. The `wal` crate's shared
`Rc<RefCell<Log>>` (held by both `LogWal` and `TxnStore`) became `Arc<Mutex<Log>>`,
and its `RefCell<Option<Txn>>` / `RefCell<HashSet>` became `Mutex`; the buffer's WAL
seam is now `Box<dyn WalSync + Send>`. That was safe because the re-entrancy hazard —
a `TxnStore` method holding a `log` borrow while a buffer eviction calls
`LogWal::flush_until` (which re-locks `log`) — provably does not occur: every buffer
operation that can evict-and-flush runs *before* any `log` borrow is taken (the log
borrow only touches the log's own file, never the buffer), and RefCell would have
*panicked* on any nesting, so the whole passing test suite was the proof. The result:
`Database` is `Send` (a compile-time guard, `concurrency::database_is_send`, keeps it
so), and it can be shared across threads behind a `Mutex` —
`concurrent_writers_via_shared_handle` runs 8 threads inserting 800 rows through one
`Arc<Mutex<Database>>` with none lost or duplicated. **All 152 tests, including every
ARIES crash campaign, pass under both the default and `campaign` profiles after the
conversion** — the logic was preserved, only the pointer/cell types changed. This is
the *enabling* step: transactions in the `wal`/`db` layers are still serial (one at a
time), so this buys thread-*safety* (coarse-locked sharing), not yet
thread-*concurrency*; wiring the lock manager + MVCC for fine-grained latching is the
next phase, now unblocked because the types cross thread boundaries.

## D-PAGER-9 — The rest of the conversion surface, and why "`Cell`→atomics" is wrong for `BTree::root`
D-PAGER-8 left an explicit gap: the detector instruments only `Database`, while D-PAGER-6
also names `HeapFile`'s `fsm`/`cursor`/`stats` and `BTree`'s `root`. Enumerating that
remainder turned out to close the deadlock question entirely — and to surface a **different**
hazard that the recorded plan treats as already solved.

**The deadlock surface is now closed.**

| Field | Type | Re-entrancy (deadlock under `Mutex`) |
|---|---|---|
| `Database` ×4 | `RefCell` | 1 site found by instrument, fixed (D-PAGER-8); 0 remain |
| `HeapFile::fsm` | `RefCell` | 2 sites, both leaf — verified by inspection |
| `HeapFile::cursor`, `HeapFile::stats`, `BTree::root` | `Cell` | structurally impossible — `Cell` hands out no guard |

`fsm`'s two sites are `set_fsm` (pushes/indexes the `Vec` and returns) and `pick_page`
(reads `fsm`, touches `cursor`, returns). Neither calls back into `self`, so neither can
re-enter. This is inspection rather than instrumentation, which is weaker evidence — it is
sound here only because it is two leaf functions, not thirty-one call sites.

**The hazard that replaces it.** `Cell` cannot deadlock, and that is exactly why it is
dangerous: it converts silently and keeps its race. Three fields do a **non-atomic
read-modify-write**:

- `HeapFile::bump` — `stats.get()` → mutate → `stats.set()`; concurrent bumps lose one.
- `HeapFile::pick_page` — `cursor.get()` … `cursor.set()`, interleaved with the `fsm` read;
  two threads can pick the same page.
- `BTree::insert` — `root.get()` at the top, then after a full recursive descent, a page
  allocation and an internal-node write, `root.set(new_root)`. The read-modify-write spans
  the **entire split**.

The last one is the reason this entry exists. Making `root` an `AtomicU32` satisfies `Sync`,
compiles, and removes a deadlock that never existed — while leaving the widest lost-update
window in the codebase. Two concurrent root splits: one `set` wins, the loser's `right_pid`
subtree is never linked from any parent, so it is a leaked subtree and silently lost rows.
That failure is invisible to the type system and would not reproduce single-threaded.

So D-PAGER-6's "convert `Cell`/`RefCell` to `Mutex`/atomics" is not merely under-specified;
for `BTree::root` it is **the wrong primitive**. A root split is a multi-page structural
modification and needs mutual exclusion over the operation, not an atomic over the pointer.

**Measured, and the attribution above is partly wrong.** The paragraph on `BTree::insert`
was written by reading code, which is the weakest standard in this project. Branch
`prove/btree-root-race` performs the conversion D-PAGER-6 recommends so the scenario is
constructible at all (with `Cell` the tree is `!Sync` and the compiler refuses to share
it), and measures it:

| run | 4 threads x 1500 inserts, all `Ok` | rows unreachable from the root |
|---|---|---|
| 1 | 6000 | 2763 |
| 2 | 6000 | 2358 |
| 3 | 6000 | 3219 |
| control, `THREADS = 1` | 6000 | **0** |

The control matters: single-threaded the atomic conversion is perfectly correct, so the
loss is a race and not a botched port. Every insert returned success, so the loss is
silent.

What that corrects: this entry pinned the hazard on the root read-modify-write
specifically. **A ~40-54% loss rate cannot be explained by root splits alone** — the
recursive descent and the node writes are equally unsynchronised, and concurrent splits of
any node corrupt each other the same way. The conclusion stands (the hazard is real,
atomics are the wrong primitive) but the mechanism was drawn too narrowly. `BTree::insert`
needs mutual exclusion over the whole operation, not a fix aimed at the root pointer.

**Status.** Deadlock: enumerated and closed. Atomicity: measured, catastrophic, unfixed —
fixing it is a design question (lock granularity), not a type swap, and is deliberately
left open rather than guessed at. Workspace **212**, both profiles, clippy-clean.

## D-PAGER-8 — Localising the D-PAGER-7 hang: one re-entrant borrow, found and removed
D-PAGER-7 recorded *why* the `RefCell`→`Mutex` conversion hung (`RefCell` permits nested
shared borrows, `Mutex` permits none) but never recorded **where**. The hang was observed,
not located, which is why the remaining work was scoped as an open-ended "restructure the
call sites" rather than a finite list. This entry closes that gap.

**Method.** `db::borrowtrack` adds `TrackedRefCell`, a drop-in for `RefCell` that keeps a
per-thread depth count per cell instance and reports any borrow taken while that same cell
is already borrowed on the same thread — precisely the condition that deadlocks under
`Mutex`. It sits behind the `borrowtrack` feature; with the feature off `DbCell<T>` is a
plain `RefCell` and the build is unchanged. `Database`'s four cells (`catalog`, `indexes`,
`stats`, `txn`) are labelled so findings name the field.

**The detector was falsified before its output was trusted** (`db/tests/borrowtrack_selftest.rs`),
in both directions: it reports a deliberately nested shared borrow, and stays silent on two
sequential borrows. Without that, "the suite reported nothing" would have been
indistinguishable from "the detector does nothing" — the same unfalsifiable-experiment trap
that produced three invalid results earlier in this project.

**The finding.** Exactly one re-entrant site in the db suite:

```text
re-entrant borrow: stats [shared, depth 1]
    keel_db::Database::index_rows
    keel_db::Database::select
    keel_db::Database::q_error
```

`q_error` took `self.stats.borrow()` and held the guard across `self.select(&q)`, which
reaches `index_rows`, which borrows `stats` again. Legal today; a self-deadlock the moment
that cell becomes a `Mutex`, and also under `RwLock` if a writer queues between the two
reads. Fixed by scoping the borrow to the estimate that needs it and dropping it before the
`select` call — the estimate is a `f64`, so nothing needs the guard afterwards.

**Honest boundary.** This removes *one* blocker; it does not make the conversion safe.
The detector only sees paths the tests actually execute, so silence elsewhere is evidence,
not proof. More importantly it instruments **only `Database`** — D-PAGER-6 also names
`HeapFile`'s `fsm`/`cursor`/`stats` and `BTree`'s `root`, which are untracked and would
have to be covered the same way before a conversion is attempted again. The claim here is
narrow and deliberately so: the one re-entrancy that db's own coverage can reach is gone,
and there is now a reusable instrument for finding the rest.

Workspace **212**, both profiles, clippy-clean with the feature on and off.

## D-PAGER-7 — Making `Database` `Sync`: attempted, reverted, and why the reasoning was wrong
D-PAGER-6 identified the last blocker to concurrent SQL: `Database`'s `RefCell`/`Cell`
fields. Converting them (`RefCell`→`Mutex`, `Cell`→atomics, plus `StmtLog::end`) was
attempted and **reverted**. It is recorded rather than quietly dropped, because the failure
is instructive.

The conversion itself worked: it compiled, and `assert_sync::<Database<PageCache>>()`
**did** start compiling — the stated goal was mechanically reached. Then the db test suite
**hung**, and the hang is the point.

The justification for expecting this to be safe was: *"`RefCell` panics loudly on re-entrant
borrows, and all 34 db tests pass, so no re-entrant borrow exists on any tested path."*
**That reasoning is wrong, and subtly so.** `RefCell` permits **nested shared borrows** —
any number of simultaneous `borrow()`s are legal and panic-free. `Mutex::lock()` permits
**none**: a second lock on the same mutex from the same thread deadlocks. So the green
tests proved only "no nested `borrow_mut`", which is a strictly weaker property than the
one the conversion needed. A path holding `self.catalog.borrow()` across another
`self.catalog.borrow()` was perfectly legal before and hangs after.

Reverted to the known-good state (db 40/40 green). Doing this properly means either
restructuring the call sites so no lock is ever re-entered (the real work, and the reason
this is a separate initiative rather than a migration tail), or `RwLock` — which is *not*
a drop-in either, since a nested read can still deadlock against a waiting writer.

Two things worth keeping from this. First: **a "safe to convert" argument must match the
exact property the target primitive requires**, not a neighbouring one — this one was off by
"shared vs any". Second: the goal was measurable (`assert_sync` compiles) and it *was*
reached, which is exactly why the test suite mattered more than the type check. Hitting the
stated target is not the same as the change being correct.

## D-PAGER-6 — What the swap bought, and what it did **not** (an honest boundary)
It would be easy to read "the engine now runs on a `Send + Sync` page cache" as "KEEL
serves SQL concurrently". **It does not**, and this is pinned down as compile-time facts
(`db/tests/concurrency_claims.rs`) so the claim cannot drift:

| Type | `Send` | `Sync` | |
|---|---|---|---|
| `PageCache` | ✅ | ✅ | genuinely concurrent — what the `latch`/`cbuffer` arc was for |
| `Database<PageCache>` | ✅ | ❌ | still serialized by the **engine**, not the buffer |

`Database` owns single-threaded interior mutability — a `RefCell` catalog, index list,
stats and txn buffer, plus several `Cell` counters — so it is `!Sync` *regardless of which
pool it holds*. Verified empirically rather than asserted: adding
`assert_sync::<Database<PageCache>>()` fails to compile, and the compiler names the cause
(`RefCell<BTreeMap<String, (u16, Schema)>> cannot be shared between threads safely`).
Concurrent SQL still requires a `Mutex<Database>`, which is what the threaded test does.

So the value delivered is real but **narrower than "concurrent SQL"**: the storage layer is
no longer the thing standing in the way, and it has been proven equivalent to the old one
at every layer *and* under power loss. Getting the rest of the way means converting
`Database`'s interior mutability (and `HeapFile`'s `fsm`/`cursor`/`stats`, `BTree`'s `root`)
from `RefCell`/`Cell` to `Mutex`/atomics — a separate, independently testable change with
its own failure modes, not a continuation of the pool swap.

Recording this because the risk after a long migration is quietly inheriting a bigger claim
than the work supports. The swap is finished; the concurrency story is not, and the two
should not be conflated. Workspace **212**, both profiles, clippy-clean.

## D-PAGER-5c — `wal` on the concurrent cache: every consumer migrated
The deepest consumer and the last one. `TxnStore` is now `TxnStore<P: RecoveryPager =
BufferPool>`; `open`/`open_with` stay concrete (they construct a pool internally), every
existing test and both crash campaigns are unaffected, and `with_pager` is the seam.

The delicate site was `write`, the only place that holds a page guard **across the log
append**. Under closure-scoped access the whole log-then-apply step moves inside the
closure, which makes the lock order explicit: page-buffer → log. That is the same order
`cbuffer`'s flush path takes (it holds the buffer read guard across `wal.flush_until`,
which locks the log), so the two cannot cycle. The Explore report flagged this ordering as
something to write down before more locks arrived; it is now enforced by the shape of the
code rather than by a comment.

`wal/tests/on_cbuffer.rs` runs create-pages, a committed transaction of 40 byte-range
updates, an **aborted** transaction (exercising CLR undo), and a checkpoint — on both pools
— requiring byte-identical pages and update-record counts, and asserting the aborted
write left nothing behind. It passed first try, as did all five existing `wal` tests
including the 24-seed vicious-tearing crash campaigns and the ARIES ladder.

**The engine swap is functionally complete**: `heap`, `btree`, `db`, and `wal` all run on
either pool, chosen at the type level, with a differential at every layer (pager, recovery
surface, both data structures, the SQL engine, the transaction store). `keel-buffer` is now
one of two interchangeable backends rather than the only one. It is deliberately **not**
deleted: it remains the default type parameter, it is the reference implementation the
differentials compare against, and removing it would delete the oracle that proves the
concurrent path correct. Retiring it is a separate decision about defaults, not a
continuation of this migration. Workspace **208**, both profiles, clippy-clean.

## D-PAGER-5b — The whole SQL engine on the concurrent cache (`db` generic)
The payoff the whole arc was for. `Database` is now `Database<P: RecoveryPager =
BufferPool>`; `shell`, `bench`, `dbcheck`, and all 34 of `db`'s own tests compile
**untouched**. The single `impl Database` block splits in two: `impl
Database<BufferPool>` keeps `open`/`open_logged` (which construct a pool internally, so
their signatures cannot change), and everything else moves to `impl<P: RecoveryPager>
Database<P>`. `Database::with_pager` is the seam the concurrent cache enters through.

Only four things needed touching beyond the mechanical split — two private helpers and one
free function took `&HeapFile<'_>` (defaulting to `BufferPool`) and had to become
`&HeapFile<'_, P>`, and `PagerError` folds onto the existing `DbError::Buffer` variant so
the public error type is unchanged.

`db/tests/on_cbuffer.rs` runs a real workload — DDL, 400 inserts, a secondary index, an
`UPDATE`, a `DELETE`, `ANALYZE`, then an indexed point lookup, a join+aggregate with
`GROUP BY`/`ORDER BY`, a filtered ordered range, and a `COUNT(*)` — through
`Database<PageCache>` and `Database<BufferPool>`, requiring **identical results**, plus a
checkpoint-and-reopen on the concurrent cache. Every layer beneath was already
differentially compared (pager, recovery surface, heap, btree); this is the top of that
stack, and it agreed first try.

Worth noting what the four failures on the way to green were: `ANALYZE` is a method not a
parsed statement, `BETWEEN` is not in the frozen grammar (D10), a triple `unwrap`, and an
off-by-one in my own row-count assertion (`id < 20` deletes 20 rows, not 19). **None was a
pool disagreement** — the cross-pool comparisons passed every time. That is now the fourth
consecutive slice where the only failures were in my test scaffolding rather than the
migration, which is itself the signal that the layered differentials did their job.

## D-PAGER-5a — `RecoveryPager`: the recovery surface, split from `Pager` (engine-swap slice 5, first step)
`db` and `wal` are the last two consumers, and both need what `Pager` deliberately omits:
steal policy, abort-time invalidation, the Dirty Page Table, a fault-tolerant redo fetch,
and single-page durability. Rather than widen `Pager`, that surface is a **separate trait
extending it**. The split is the point: `heap` and `btree` need none of it, so they stay
generic over the *small* surface and cannot accidentally reach for a recovery primitive.
Only `wal` — and `db`, for its no-steal logged mode — depends on the wider one.
`with_page_for_redo` is closure-scoped for the same reason as the rest of the seam.

Implemented for both pools, and — following the pattern that has paid off at every layer —
**compared before anything migrates onto it**. `pager/tests/recovery.rs` runs one generic
recovery workload through both and requires agreement on: DPT snapshots (oldest recLSN
wins, sorted), the DPT after `invalidate`, whether an over-capacity allocation *refuses*
under no-steal, the contents of a redo-rebuilt missing page, and the allocation watermark
afterwards. These are precisely where a difference between the pools would surface as a
**recovery** bug — the worst kind, because it only appears after a crash.

Writing it surfaced a real interaction worth recording: the redo fetch must be exercised
*before* the no-steal section, because once every frame holds a dirty page, no-steal leaves
no evictable victim and the redo fetch itself correctly returns `Exhausted`. That is right
behaviour on both pools, but it would have masked the check. Workspace **205**, both
profiles, clippy-clean. What remains is the mechanical part: `db` and `wal` still *construct*
a `BufferPool`, so flipping them means making them generic (with the same default type
parameter) and adding a test that runs the whole SQL stack on a `PageCache`.

## D-PAGER-4b — `heap` runs on either pool (engine-swap slice 4 complete)
The half flagged as riskier, because `heap` has two shapes `btree` doesn't: the **FSM
rebuild** in `open` (scanning every page) and the **forwarding-stub** path, where an update
that no longer fits in place relocates the record to another page and leaves a stub —
the one operation touching two pages. Closure-scoped access forbids holding a guard across
another fetch, so those were the places most likely to resist conversion.

The check was done rather than assumed, and it came back clean: `heap` was already written
to copy bytes out and drop the guard before any multi-page step — its own comments say so
("copied out so no page guard is held across the multi-page steps"; "Collect this page's
slots up front so we don't hold a guard while following forwards"). All eleven sites
converted mechanically. `HeapFile<'a>` → `HeapFile<'a, P: Pager = BufferPool>`; `db`, `wal`,
and `dbcheck` compile **untouched**. One restructuring was needed: `scan`'s `continue` for a
non-heap page cannot cross a closure boundary, so the closure returns `Option` and the skip
happens outside.

`heap/tests/on_cbuffer.rs` drives the same heap over both pools through exactly those risky
paths — 300 inserts, then growing every third record to ~1 KB so relocation and stubs fire,
deletes, a reopen (re-running the FSM rebuild), full scan, per-RID probes — and requires
exact agreement, with `forward_hops > 0` asserting the stub path really ran.

**Two of my own metrics were wrong before the test was right**, both worth recording because
they encode real behaviour. First I asserted on `stats().new_pages` after reopening — but
`stats` belongs to the `HeapFile` instance and a reopen resets it, so that count is 0 by
construction. Then I measured page spread as distinct RIDs in the scan — but a forwarded
record is deliberately reported under its **stub's stable RID**, so relocation is invisible
there *by design*. The honest measure is `Pager::page_count`. In both cases every real
cross-pool comparison had already passed; only the sanity assertion was mismeasuring.
Workspace **204**, both profiles, clippy-clean. `heap` and `btree` now both run on either
pool; what remains is flipping `db`/`wal` to construct a `PageCache` (5), which needs the
D-PAGER-2 policy surface behind a trait or used pool-specifically.

## D-PAGER-4a — `btree` runs on either pool (engine-swap slice 4, first half)
The first slice that rewrites existing code. `BTree<'a>` becomes `BTree<'a, P: Pager =
BufferPool>` — the **default type parameter** is what makes this non-breaking: `db` calls
`BTree::open_rooted(&self.bp, root)` with a `&BufferPool` and compiles completely untouched,
as do all of `btree`'s own tests. Conversion needed **zero** call-site changes outside the
crate.

It converted cleanly because of a property worth noting: every guard in `btree` is acquired,
used, and released inside one function body — no guard is ever held across another page
fetch. So each of the seven sites became a single `with_page` / `with_page_mut` closure
mechanically. (`heap` is the other half of this slice and is *not* yet converted; its FSM
rebuild and forwarding-stub paths need checking for the same property before assuming it.)

The public error type is unchanged: `PagerError`'s three failures fold onto the existing
`BtreeError::Buffer(BufferError)` variant, so no downstream match had to change.

Evidence the generification is real rather than cosmetic: `btree/tests/on_cbuffer.rs` drives
the **same** B-tree over `BufferPool` and over `cbuffer::PageCache` — 3 000 shuffled inserts
(enough to split into many leaves and internal levels), deletes, full scan, range scan, point
probes, and the invariant checker — and requires the two to agree exactly, plus a
BTreeMap model check and a build-checkpoint-reopen-on-`PageCache` test. This is a
differential one rung above D-PAGER-3's: a divergence in eviction, allocation, or checksum
handling that survived the pager comparison would surface here as a wrong lookup or a broken
invariant. Workspace **202**, both profiles, clippy-clean.

## D-PAGER-3 — The `Pager` seam, and a differential between the two pools (engine-swap slice 3)
The migration map proposed a GAT-based trait returning a guard per pool. That does not
work safely: `BufferPool` hands out one self-contained `WriteGuard<'a>` holding a `RefMut`,
but `PageCache` hands out a `PageRef` whose lock guard **borrows that `PageRef`** — so a
single owning guard type would be self-referential, and `unsafe` is quarantined to `page`
(D4). The seam therefore hands bytes to a **closure**: the pool keeps whatever guards it
needs for exactly the duration of the call. Uniform, safe, and free.

Scope is deliberately only what `heap`/`btree` use — count, read, write, allocate slotted,
allocate raw, checkpoint. The policy/recovery surface from D-PAGER-2 stays pool-specific,
because only `wal` needs it and `wal` migrates last.

The point of landing the seam *before* converting `heap`/`btree` is the **pager
differential**: one generic workload run through both pools and compared. Behavioural
divergence in page numbering, allocation, eviction, checksum handling, or what a reopened
pool sees now surfaces in a 60-line test instead of inside a B-tree split at slice 4. A
second test writes through `BufferPool` and reads back through `PageCache` over the same
bytes, establishing the on-disk formats are interchangeable — the real precondition for
swapping pools under a live engine. Both pass, which also independently confirms
`cbuffer`'s new `PageFormat` reproduces `keel-buffer`'s long-standing central stamping:
the workload never stamps a checksum itself.

**A sensitivity gap, found and closed.** The first version of the differential *passed*
with a deliberately injected divergence (the `PageCache` impl initialising slotted pages
via `raw::init_header`). The cause is real and worth recording: `SlottedPage::insert` calls
`compact()` when space looks short, and compacting a page with no live tuples **repairs** a
zero-initialised header into a valid empty one — so the divergence self-heals the moment
records are written, and a contents-only comparison cannot see it. The fix is to sample
each page's header (`slot_count`, `free_start`, `free_end`) *at allocation*, before any
insert can repair it. With that, the injected divergence fails the test immediately.
Workspace **200**, both profiles, clippy-clean. Remaining: convert `heap`/`btree` to take
`P: Pager` (4), then flip `db` and `wal` and retire `keel-buffer` (5).

## D-PAGER-2 — Policy and recovery primitives (engine-swap slice 2)
The capabilities `wal::TxnStore` depends on that `cbuffer` simply did not have, added
still-additively (no consumer changed): **no-steal** (`set_no_steal`, a `steal` flag
`choose_victim` honours by refusing to consider a dirty frame at all — under rung-1
no-steal the log is a page's only durable record until commit forces it), **`invalidate`**
(drop a page unflushed on abort, reverting to the durable version, and drop it from the
DPT), the **Dirty Page Table** (`note_dirty` / `dpt_snapshot`, first-writer-wins because
analysis needs the *oldest* record that dirtied a page; cleared on flush since a page on
disk no longer constrains where redo starts), **`fetch_for_redo`**, **`flush_page`**, and
**`sync`**.

Two structural choices, both aimed at not re-introducing the bugs this cache has already
had. `fetch_for_redo` is not a second copy of `fetch`: both delegate to one `fetch_mode`,
differing only in the read discipline (`Normal` errors on a bad read or failed integrity
check; `Redo` rebuilds blank, because during recovery the log is the source of truth) and
in publishing dirty plus extending the allocation watermark past a page a truncated file
never held. Likewise `flush_page` does not re-implement flushing — `flush_all`'s body was
extracted into `flush_one`, so the delicate part (wait out an in-flight eviction per
KEEL-0008, re-validate frame identity under the buffer guard per KEEL-0007) exists in
exactly one copy. Three bugs have already lived in that skeleton; a fourth copy of it was
not worth the convenience.

Oracle `cbuffer/tests/policy.rs` asserts each primitive against what `TxnStore` actually
needs: no-steal refuses (`Exhausted`) rather than evicting a dirty page and writes nothing
to disk, yet an explicit checkpoint still lands them; `invalidate` reverts to the durable
page and clears the DPT; the DPT keeps the oldest recLSN and empties on flush;
`fetch_for_redo` rebuilds both a checksum-failing page and one past EOF while a normal
`fetch` still refuses the former. Verified non-vacuous — disabling the no-steal guard fails
the first. Workspace **198**, both profiles, clippy-clean. Remaining: the `Pager` trait
(slice 3, with `BufferPool` as the default type parameter so `heap`/`btree` generify without
touching a call site), then flipping `db` (4) and `wal` (5).

## D-PAGER-1 — `PageFormat`: the cache owns the checksum, so no caller can forget (engine-swap slice 1)
Mapping the `BufferPool` → `PageCache` migration surface turned up the finding that
reframes KEEL-0004: **`keel-buffer` stamps the page checksum in exactly one place (its
flush path) and verifies it in exactly one place (its load path); `cbuffer` did neither.**
That is not merely a migration gap — it is the *root cause*. `cheap` had to recompute the
checksum by hand at every mutating site precisely because the pager offered no guarantee,
and the single path that forgot shipped a stale checksum. KEEL-0010 is the same shape one
layer up: `ckv` owned an `intact()` helper and a `Corrupt` error, yet four of its six paths
never called them.

So the first migration slice is the structural cure, not more call-site discipline. A
`PageFormat { lsn_of, stamp, verify }` is handed to the cache at construction; the cache
stamps before **every** write and verifies after **every** read, surfacing
`CacheError::Corrupt(pid)` rather than publishing damaged bytes. `PageFormat::keel_page()`
wires the `keel_page` raw helpers (valid for slotted *and* raw pages, since the CRC covers
`[OFF_PAGE_LSN, PAGE_SIZE)`); `PageFormat::opaque()` preserves today's behaviour for callers
with their own scheme — `ckv` keeps its CRC at its own offset and must not be stamped over.

Two design points worth recording. The invariant is **"a page's checksum is correct on
disk"**, not in memory: a dirty cached page legitimately carries a stale checksum until it
is flushed, which is exactly `keel-buffer`'s contract. And stamping is applied to a *scratch
copy* rather than in place, because the flush paths deliberately hold only the buffer **read**
guard — that is what keeps disk I/O off the readers' backs and what pins the frame's identity
across the write (KEEL-0007). One page memcpy against a disk write is the right trade.

Purely additive: zero consumers changed, and the slice carries its own oracle
(`cbuffer/tests/format.rs`) proving the cache stamps pages **no caller ever stamped**, that a
byte flipped on disk surfaces as `Corrupt` while intact neighbours still load, and that the
opaque format rewrites nothing. Verified non-vacuous — disabling the stamp fails two of the
three. Workspace **194**, both profiles, clippy-clean. Remaining slices (policy/recovery
primitives, a `Pager` trait with a defaulted type parameter, then flipping `db` and `wal`) are
mapped in the migration report; the ordering keeps every existing test compiling untouched
until the very last step.

## D-AUDIT-1 — Audit the foundation *before* building the engine on it
The `cheap` review (D-LATCH-9) established that **green oracles do not mean correct**: three
of them passed while three real bugs were live. `ckv`, `latch`, and `cbuffer`'s
fetch/evict/publish paths had exactly that profile — concurrent, oracle-tested, never
adversarially reviewed — and the engine swap was about to be built on top of them. So they
were audited first, with the three confirmed bug *classes* handed to the reviewers as priors
(a flag mutated outside the lock guarding its data; a derived field not refreshed on every
mutating path; an error path that over- or under-restores state) so they hunted analogues
rather than starting cold.

**13 raised → 9 confirmed / 4 refuted**, every confirmed one with a deterministic
reproduction. The worst, KEEL-0007, is **cross-page corruption that defeats the checksum
layer entirely**: `flush_all` captured a frame's page identity, released the directory lock,
and — because it never pinned the frame — could write a *repurposed* frame's bytes to the
evicted page's offset. What lands on disk is a complete, self-consistent image of the wrong
page with a valid CRC, so every integrity check reports clean. Three lenses reported it
independently; notably **I had reasoned about that exact race an hour earlier while fixing
KEEL-0006 and guarded the wrong operation** (the dirty-flag clear, not the write).

Also fixed: `checkpoint` skipping dirty mid-eviction pages (KEEL-0008), a read-failure abort
serving a corrupt page as a cache hit (KEEL-0009), `ckv` never consulting its own checksum on
four of six paths and *laundering* corruption by re-sealing a torn page (KEEL-0010), and a
failed allocation stranding a page id below `page_count()` (KEEL-0011). The refutations
carried weight too — four findings were correctly rejected as documented contracts or
lifetimes that already prevent the hazard.

The most uncomfortable finding was KEEL-0012: `ckv`'s crash campaign **passed unchanged with
checksum verification removed**. A crash campaign proves whatever survived; it does not prove
detection works. Strengthening that test failed to fix it (the adversary mostly drops writes
rather than producing a readable torn page), so detection now has a *positive* test that
injects a known byte flip and asserts every read path reports `Corrupt` — verified falsifiable
by deleting the gates. The house rule this settles: **a test for a detection guarantee must be
shown to fail when the mechanism is removed.** Workspace **191**, both profiles, clippy-clean.

## D-LATCH-9 — The heap itself on the concurrent buffer (`cheap` crate), and what an adversarial review found
`ckv` proved a *data structure* could live on `cbuffer`; `cheap` is the real thing —
the **record heap** the SQL engine's `SeqScan`/`INSERT` sit on — in the engine's own
on-disk format: `keel_page::SlottedPage` pages, stable `(page, slot)` RIDs, growing via
`new_page` (D-LATCH-8), so `dbcheck` and the crash campaign apply unchanged. The
concurrency protocol is one `Mutex<Option<PageId>>` insert-frontier hint: an insert reads
the hint, takes that page's cbuffer **write latch**, and tries `SlottedPage::insert`; on
`PageFull` it **drops the latch first**, then takes the hint mutex and allocates a fresh
page *only if* no other thread already advanced past the page it saw full. A page latch is
never held while acquiring the hint, so the two locks have a fixed order and cannot
deadlock. `seal()` freezes the frontier (next insert starts a fresh page), which makes the
durability claim **deterministic**: checkpoint-then-seal pages are synced and never
re-dirtied, so no crash can touch them.

Three oracles: a **differential** (16 seeds × 3 000 random insert/get/delete/scan ops vs a
`HashMap` model through a 4-frame cache, exercising slot recycling), a **concurrent race**
(6 threads × 400 inserts, all RIDs distinct, every record present exactly once, intact
across checkpoint+reopen), and a **crash campaign** (24 seeds; committed records survive on
*every* seed, torn tail pages detected by checksum, never misread).

**All three passed while three real bugs were live.** An adversarial multi-lens review
(4 lenses → 13 findings → per-finding refutation → 4 confirmed / 9 refuted) caught them,
and the refutations mattered as much as the confirmations — three "durability leaks" were
correctly rejected as the documented no-WAL tradeoff or a contract misreading. The
confirmed ones are KEEL-0004 (a `compact()` that still returned `PageFull` left a **stale
checksum** on a dirty page → an intact committed page reads back as "torn"; 20 bad pages
per seed once tested for), KEEL-0005 (a **failed** eviction flush marked the victim clean,
so `checkpoint` skipped it and returned `Ok` while losing the data), and KEEL-0006 (a
concurrent checkpoint could clear a dirty flag it hadn't flushed). The first two now have
regression tests **verified non-vacuous by reverting the fix**; the third is honestly
labelled a stress guard, because a compiling buggy variant still passed 10/10 — a real
interleaving fixed by lock-coupling and reasoning, not by a reproducer.

The durable lesson wasn't any one bug: it was that **three data-equality oracles cannot
see a page-format invariant**, because `scan`/`get` return correct records from a
stale-checksummed page. So `cheap` got its own `dbcheck` rule (D12) — `Heap::verify`, which
walks every page for bad checksums, foreign page types, and the live-record count — asserted
from the differential, threaded, and checkpoint-race oracles. Re-introducing the exact bug
now fails the **differential** oracle (it deletes, so it forms the tombstones that make
`compact()` actually mutate); the insert-only oracles still pass, correctly, since without
tombstones the bug cannot be produced at all. 0-of-5 oracles caught it before, 2-of-5 after.
`cheap` is 5 tests, `cbuffer` 9; workspace **190**, both profiles, clippy-clean.

## D-LATCH-8 — Concurrent page allocation (`cbuffer::new_page`)
The one write-path primitive `cbuffer` still lacked: a growing structure (a heap, a
B-tree) can't only *read* existing pages — it must *allocate* new ones, and the real
integration hits this immediately. `new_page` hands out the next unused page id (a
`Dir::next_page` counter, seeded at open from `file.size() / PAGE_SIZE` and bumped **under
the directory lock** so concurrent allocations never collide), reserves a victim frame,
**zeroes** its buffer rather than reading disk (the page has no image yet), and publishes
it dirty so `checkpoint`/eviction materializes it. It reuses the same reserve/flush/publish
transition as a miss — a dirty victim is still flushed WAL-before-data first — so it
inherits all the concurrency correctness already proven; the only differences are the
atomic id bump and skipping the read. Ids are unique, not required to be gap-free (a failed
flush skips an id rather than reusing it — simpler and harmless). The race oracle
(`tests/alloc.rs`) runs 6 threads allocating 500 pages each through an 8-frame cache (so
allocations constantly evict and flush): all 3 000 ids come back **distinct** (a HashSet of
size 3 000), the allocation counter agrees, and after a `checkpoint` and reopen **every**
allocated page reads back its own id stamp — proving none was lost, aliased, or left
unmaterialized. With this, the concurrent buffer has the full read + allocate + write +
flush + checkpoint surface a heap needs; the heap/btree integration no longer waits on a
missing cache primitive. `cbuffer` is 8 tests (4 unit + 3 concurrent race oracles + crash
campaign); workspace **183**, both profiles.

## D-LATCH-7 — A real data structure on the concurrent buffer (`ckv` crate)
The concurrency and durability of `cbuffer` are only worth anything if a *data
structure* can be layered on them and inherit both without re-deriving them — so this is
the integration step, done as a reference rather than as the (large, risky) in-place
`BufferPool` swap. `ckv::PagedKv` is a durable, concurrent hash-bucket key→value store:
each bucket is one `cbuffer` page, and `put`/`get`/`update` take that page's reader/writer
latch through a `PageRef`, so the whole store inherits the buffer's per-page concurrency,
its `checkpoint` barrier, and its crash guarantees for free. `update` holds the write
latch across its read-modify-write, making same-key increments atomic; different buckets
proceed in parallel. Every bucket page carries a trailing CRC (sealed on each write), so a
torn page is *detected*, never read as valid data — the house rule that on-disk structures
carry a checksum from birth, now inherited by anything built on the cache. It is validated
with the **four KEEL lenses** a real subsystem gets, which is the point of building it:
- **fuzz-vs-model** — 5 000 random `put`/`get`/`update` against a `HashMap` oracle, exact
  agreement;
- **under-real-threads** — 6 threads × 20 000 increments over 40 keys / 16 buckets through
  an 8-frame cache (so it drives dirty eviction under load): the grand total equals every
  increment issued, i.e. **no lost update**, and every bucket's CRC still verifies;
- **crash campaign** — 24 seeds: checkpoint all keys at a base value, rewrite them newer
  *without* checkpointing (through a 3-frame cache so the newer writes evict into the
  un-synced pending set), vicious `crash`, reopen from the durable image; every *intact*
  bucket holds only base-or-newer values (never older, garbage, or silently-torn) and torn
  buckets are caught by CRC — with the adversary confirmed firing and at least one seed
  leaving every bucket intact;
- plus the ordinary unit round-trip and reopen-after-checkpoint tests.
Honest scope, stated in the crash test itself: `ckv` inherits *checkpoint durability* and
*torn detection*, not redo/undo of un-checkpointed work — that is the WAL/`recover_aries`
layer's job, which the real engine composes on top; this reference deliberately stops at
the buffer contract. `ckv` is the concrete evidence that the concurrent-buffer stack is a
usable storage substrate, and the template the heap/btree integration will follow. `ckv`
is 6 tests (4 unit + threaded + crash campaign); workspace **182** across 23 crates, both
profiles.

## D-LATCH-6 — The concurrent cache faces the crash adversary (`cbuffer` campaign)
KEEL's thesis is that durability is *earned against an adversary, not asserted*, so the
concurrent cache is now put over the fault-injecting disk (`faultfs`), closing the loop
that D-LATCH-4/5 opened: those proved the *ordering* (WAL-before-data) held under
concurrency; this proves the ordering actually *buys durability* under real power loss.
The property is the P1 boundary lifted onto `cbuffer`: after `checkpoint()` (flush every
dirty page, then `sync`), the checkpointed pages survive a **vicious** crash byte-exact,
and cache-resident changes never checkpointed correctly do *not* reach disk. The campaign
runs 24 seeds, each: pre-fill and sync a baseline; checkpoint pages 0..8 at a known
version through a deliberately small (4-frame) cache so writes steal-and-evict along the
way; then dirty a separate scratch set *after* the checkpoint and leave it un-synced;
`crash()`; reopen from the recovered durable image and read the checkpointed set back.
Every checkpointed page carries a CRC over its payload, so a torn checkpoint page would
be *caught*, not silently accepted — the assertion is CRC-valid **and** exact version
**and** correct identity, for all 8 pages, every seed. The scratch set exists so the
crash has un-synced writes to drop, tear, and reorder (the run asserts the adversary
actually fired — total pending ops across seeds > 0), while the checkpointed set stays
untouchable because a crash only ever layers pending writes *on top of* the durable image
it starts from. Added a `checkpoint()` = `flush_all` + `file.sync()` to make the barrier
a single call. This is the concurrent analogue of the `dbcheck` `crash_smoke` boundary,
and it ties `cbuffer` into the campaign discipline that is the project's identity. Honest
scope unchanged: `cbuffer` guarantees checkpoint durability and WAL-before-data ordering,
not redo/undo of un-checkpointed work — that is the WAL/`recover_aries` layer's job, which
a `BufferPool` swap-in composes with, not this cache. `cbuffer` is 7 tests (4 unit + 2
concurrent race oracles + a 24-seed crash campaign); workspace **176**, both profiles.

## D-LATCH-5 — Dirty-page eviction under WAL-before-data, concurrently (`cbuffer`)
The slice D-LATCH-4 named as next: a victim frame holding a *dirty* page must be flushed
before reuse, and that flush must respect WAL-before-data, all without a global I/O
stall and without the two-copies-of-a-dirty-page race. The miss path is now
**reserve → flush old → load new → publish**, and the correctness turns on one rule:
across the whole transition **both** the old page id and the new one resolve to the busy
victim frame, so a concurrent `fetch` of either waits rather than reading a stale on-disk
copy or starting a duplicate load. Concretely — the reserve step, under a single lock
hold, marks the victim `busy`, pins it, and inserts the *new* key while leaving the *old*
key mapped; the lock is released; the old page is flushed (`wal.flush_until(page_lsn)`
then `write_at` — WAL-before-data in one place, `flush_old`); the new page is read; then
publish, under the lock, removes the old key and clears `busy`. Because the miss-check and
both directory inserts are atomic, two threads can never both begin loading the same page;
because the old key stays mapped until the flush is durable, no one ever reads a stale old
copy. I/O errors roll the reservation back so a failed read or flush never wedges a page
or strands a waiter. The proof is a second race oracle (`tests/dirty.rs`) with a
`GuardDisk` that wraps the backing file and, on **every** write, reads the page's embedded
LSN and counts a violation if the shared WAL isn't already durable through it — the live
form of `buffer::flush_frame`'s `debug_assert`. Four threads run 80 000 fetches over a
16-page set through a 6-frame cache, dirtying ~half with fresh monotonic LSNs so eviction
constantly flushes: **0 WAL-before-data violations, every page keeps its id stamp through
the churn, flushes and evictions both non-zero.** A `flush_all` checkpoint barrier is
included and drains the rest. This completes the durability-relevant behaviour of a real
concurrent buffer over `vfs` in isolation; what a full `BufferPool` swap-in still adds is
integration (the heap/btree/catalog laying pages out through this cache instead of the
single-threaded one) and the DPT/checkpoint plumbing the ARIES recovery already expects —
mechanism now proven, wiring remaining. `cbuffer` is 6 tests (4 unit + 2 concurrent race
oracles); workspace **175** across 22 crates, both profiles.

## D-LATCH-4 — Concurrent cache with I/O *outside* the lock (`cbuffer` crate)
The one thing `ClockPool` (D-LATCH-3) simplified away — it built page contents while
holding the pool mutex — is the thing a real engine can't do, because a page read is
slow I/O and holding the single directory lock across it serializes the whole pool on
the disk. `cbuffer::PageCache` closes that gap against the real `vfs::BlockFile`: a miss
is split into **reserve → I/O → publish**. Under the directory `Mutex` a CLOCK victim is
chosen and marked *loading* for the wanted page (`pins = 1`, so no other thread can take
it); the lock is **released**; the disk read fills the frame's buffer (which lives behind
its own `Arc<RwLock<Vec<u8>>>`, not under the directory mutex); then the lock is retaken
to publish the frame *ready* and a `Condvar` wakes anyone waiting. A per-frame `loading`
flag means concurrent missers of the *same* page block on the one loader and re-check,
rather than each firing a duplicate read — the find-or-load TOCTOU closed without a global
I/O bottleneck. I/O errors roll the reservation back (frame freed, waiters woken) so a
failed read never wedges the page. The race oracle (`tests/race.rs`) stamps every page on
disk with its id, then runs 6 threads over a 24-page set through an 8-frame cache for
180 000 fetches, each asserting the pinned page's stamp equals its id: a frame published
for the wrong page, a duplicate load clobbering a pinned frame, or a victim taken while
pinned would all show up as a wrong stamp under a held pin — 0 wrong, real eviction
pressure, no pin leaked. This is the first real (disk-backed) concurrent buffer, built
**additively** as its own crate so it touches nothing in the single-threaded `buffer` and
needs no full-workspace revalidation. Honest scope of the slice: the cache is **clean**
(pages read, never dirtied), so eviction is a plain drop and the two-copies-of-a-dirty-page
hazard doesn't arise; **dirty-page eviction with WAL-before-data under concurrency** is
the next slice — the exact ordering `buffer::flush_frame` enforces serially today, lifted
onto this reserve/publish protocol. `cbuffer` is 4 tests (3 unit + 1 concurrent race);
workspace **173** across 22 crates, both profiles.

## D-LATCH-3 — The concurrent page cache, assembled and race-proven (`latch::ClockPool`)
The reference assembly of every primitive above, and the last piece before the real
`BufferPool` refactor: a fixed-capacity in-memory page cache with CLOCK replacement that
respects pins. It exists to prove the whole D-LATCH-0 protocol end to end in a
controlled, disk-free setting — `BufferPool` will mirror its shape, swapping the
in-memory `make` closure for a `vfs` read plus the WAL-before-data flush. The
load-bearing decision is the one the current pool gets wrong: **all** residency, pin,
ref-bit, and CLOCK-hand state lives under **one** mutex, so `acquire(pid, make)` chooses
a victim and takes it as a single atomic step. A frame can't be pinned by another thread
between selection and replacement, a pinned frame is never a candidate, and a resident
page is found (not re-loaded) — closing both races D-LATCH-0 named. Exhaustion (every
frame pinned) returns `None`, an honest signal rather than a silent stall (a house law:
no silent caps). The race oracle (`tests/clock.rs`) runs 6 threads over a 24-page working
set through an 8-frame pool for 180 000 acquisitions: each thread reads every page it
pins and asserts the value equals the id, so any victim-vs-pin race would surface as a
pinned frame reloaded with the wrong page — 0 mismatches, with real eviction pressure
(loads ≫ capacity, evictions > 0) and no pin leaked. What this reference deliberately
simplifies, and what the `BufferPool` assembly therefore still owns: it builds page
contents under the pool lock, whereas the engine must do its I/O *outside* the lock and
re-check residency after — the remaining complexity, named here so the gap is explicit
rather than hidden. With this, every element of the concurrency protocol is built and
proven in isolation; the outstanding work is wiring them into `BufferPool` to replace its
`Cell`/`RefCell` frames, which is gated on disk headroom for the crash-campaign reruns
(see the disk-full note). `latch` is now 16 tests (10 unit + 6 across three race oracles);
workspace **169**.

## D-LATCH-2 — Pin/evict handshake, the next protocol element (`latch::Frame`)
Picking up the piece D-LATCH-1 left "out of scope, composes on top": the pin count
that must interlock with eviction. `latch::Frame<T>` couples a page-content
[`PageLatch`] with a `PinState { pins, evicted }` under **one** mutex, so the two
decisions that the buffer's `choose_victim` currently makes from *separate* `Cell`s —
"is this frame pinned?" and "may I evict it?" — become a single atomic handshake:
- `pin()` returns a `PinGuard` (RAII, unpins on drop) but yields `None` if the frame is
  already `evicted`, so a page can't be resurrected in a frame that is being replaced.
- `try_evict()` succeeds only when `pins == 0 && !evicted`, then tombstones the frame so
  no later pin is granted.
Together these give the two invariants a concurrent CLOCK needs and lacks today: **never
evict a pinned frame**, **never pin an evicting frame**. Pins gate *residency* (held for
as long as a page is in use); the separate reader/writer latch gates *access* (held only
across one operation) — deliberately different locks. The race oracle
(`tests/pin.rs::pin_and_evict_never_overlap`) runs 6 pinner threads that pin a random
frame and, while holding it, assert it is not evicted, against an evictor hammering
`try_evict` — 0 violations over 120 000 pin attempts, with the evictor making real
progress (evicted count > 0, and it equals the number of tombstoned frames, so the test
is not vacuous) and no pin leaked. Split the pin/evict decision back across two locks and
this test finds a pinned-yet-evicted frame. Still out of scope (the actual pool refactor):
wiring `Frame` + `LatchTable` into `BufferPool` to replace the `Cell`/`RefCell` frames,
which is the D-LATCH-0 protocol assembly. `latch` is now 13 tests (8 unit + 5 across two
race oracles); workspace **166**.

## D-LATCH-1 — The latch seam, built and race-proven in isolation (`latch` crate)
Acting on D-LATCH-0 the way `lockmgr`/`mvcc` were built before being wired in: the
`latch` crate is the concurrency seam a future concurrent buffer will stand on,
implemented and proven on its own so the pool composes a *tested* primitive rather
than inventing the protocol inline. Three pieces, each answering one of D-LATCH-0's
named race sites:
- `LatchTable::get_or_install(pid, make)` — the **atomic find-or-install** directory.
  A single directory `Mutex` covers check-then-insert as one step, so threads racing
  on the same absent page converge on one `Arc<PageLatch>` and `make` runs exactly
  once. This is precisely the atomicity the buffer's `resident()`-then-load lacks
  (race site 1, the find-or-load TOCTOU).
- `PageLatch<T>` — a reader/writer **latch** over one page's value (`RwLock`-backed):
  many readers or one writer, the per-frame mutual exclusion the single-threaded
  design gets free from `RefCell`'s panic. `try_write` supports a future
  no-wait eviction probe.
- `write_two_ordered(a, b)` — two-latch acquisition in **global id order** (lower page
  first) regardless of argument order, returning guards positionally. Two threads
  taking the same pair from opposite sides request the same latch first, so no
  wait-for cycle can form — the deadlock-freedom the buffer needs when an op holds two
  pages (a B-tree split).
The proof is a race oracle (`tests/race.rs`, 4 tests) whose failure modes are real
data races and deadlocks no serial test can reach: `get_or_install_is_atomic_under_races`
(8 threads × 5 000 iters on 4 keys → exactly 4 installs, `make` called exactly 4×;
double-install would overshoot); `exclusive_latch_serializes_writes` (8 threads each
doing a non-atomic load-store through the write latch → every increment survives, so
no two writers overlapped); `shared_latches_are_concurrent` (all readers meet at a
`Barrier` while holding read guards → observed peak concurrency equals the reader
count, which a mutex would deadlock instead of reaching); and
`write_two_ordered_never_deadlocks_and_conserves` (two threads transferring between the
same pair from opposite directions run to completion — a naive acquisition order would
hang — with the sum conserved). Deliberately *out of scope* here: pin counts, eviction,
and any coupling to `BufferPool`; those compose on top of this. 9 tests (5 unit + 4
race), clippy-clean. The crate depends on nothing (a local `PageId = u32`) so it stays
a pure primitive. Total workspace: **162 tests**.

## D-LATCH-0 — Fine-grained latching is a protocol, not a type-swap (scoping)
Recorded so the next phase is entered with its cost mapped, the way D-THREADSAFE-1
mapped the `Send` step. Now that `Database` is `Send` (D-THREADSAFE-2) and the SQL
surface is proven correct under coarse locking (D-THREADSAFE-3), the tempting next
move is to mechanically swap the buffer pool's `RefCell`/`Cell` for `Mutex`/atomics —
just like the `wal` conversion — so `BufferPool` becomes `Sync` and threads share it
without the outer `Mutex<Database>`. **That shortcut is unsound, and this note exists
to say why.** The `wal` swap was safe because it changed *types only*, not the access
pattern — transactions stayed serial and a `RefCell` would have *panicked* on any
re-entrancy, so the passing suite was a proof. The buffer is different in kind: its
fetch path is a chain of non-atomic read-modify-writes that are only correct because
one thread runs them at a time. Concretely, three race sites make a bare type-swap
corrupt data even though it compiles:
- **find-or-load TOCTOU** — `resident(pid)` says "absent", so the caller loads; a
  second thread does the same between the check and the insert. Two frames now hold
  page *P*; `table.insert(pid, idx)` keeps one and orphans the other, and the orphan
  may carry a dirty update that is now lost.
- **shared victim** — `choose_victim` advances the CLOCK hand and clears ref-bits as
  it scans; two concurrent evictors can select the *same* frame, or evict a frame the
  instant another thread pins it.
- **evict-and-load tearing** — `evict_and_load` flushes, removes the old key, reads
  the new page, then inserts the new key as separate steps; interleave two and the
  page table and frame contents disagree.
So real concurrency needs a *protocol*, not a cast: a lock that covers find-or-load as
one atomic step (a page-table `Mutex`, or sharded/lock-free with a CAS install),
per-frame reader/writer **latches** with a defined acquisition order against that lock
(to keep the deadlock-freedom the single-thread design gets for free), a pin count
that composes with eviction (pin before releasing the table lock; never evict a pinned
frame), and the RAII guards reworked to hold real latch guards. None of it is provable
by the current suite — it needs its own adversarial interleaving/race-stress oracle,
because the failure mode is a data race no serial test can reach. That is why this is
the remaining *architectural phase*, not another increment: it is exactly the P7 work
the buffer's own module doc has always pointed at ("the same discipline latches will
enforce in phase 7"). The `lockmgr` and `mvcc` cores that fold in on top are already
built and thread-proven in isolation (D-LOCK-2, D-MVCC-2); this buffer protocol is the
seam that has to be designed first, and designed rather than rushed.

## D-THREADSAFE-3 — The whole SQL stack, exercised under real threads
The `Send` engine (D-THREADSAFE-2) is only worth having if the *SQL surface* — not
just raw INSERTs — survives concurrent use, so the claim is now backed by an
end-to-end test rather than an isolated-core one. `concurrent_sql_transfers_conserve_money`
runs six threads doing 900 bank transfers total over an **indexed** two-column table,
each transfer a `SELECT bal … WHERE id = ?` read of both accounts followed by two
`UPDATE … WHERE id = ?` writes. This drives the full pipeline — parse → bind →
indexed point-lookup → heap update → **secondary-index maintenance** → commit — from
many threads at once, where the standalone `lockmgr`/`mvcc` stress tests (D-LOCK-2,
D-MVCC-2) exercised only their own data structures. Atomicity of each read-modify-write
comes from holding the shared handle's lock across the whole transfer (the handle *is*
the critical section under coarse locking); the invariant is money conservation —
`SUM(bal)` must equal `accounts × start` afterward, which any lost update, torn index
entry, or mis-scoped `WHERE` would break. It passes under both profiles. This is the
honest ceiling of the current design: correct and safe under concurrency, but
serialized; it is the baseline the fine-grained-latching phase must beat on throughput
*without* breaking this invariant.

## D-THREADSAFE-1 — Thread-safe `Database` is gated on the `wal` crate's `Rc`
A concrete finding worth recording (it defines the shape of the concurrency work).
`Database` is `!Send` for exactly one reason: the buffer pool holds a
`Box<dyn WalSync>`, and the trait object isn't `Send`. Everything else already is —
`BlockFile` is declared `Send + Sync`, and there is no `Rc` in `buffer`/`db`/`heap`.
But making `WalSync: Send` *cascades*: the `wal` crate's `TxnStore` implements
`WalSync` over an `Rc<RefCell<Log>>` (the single-threaded design, D3), which is
`!Send`, so the bound fails to compile there. So a thread-safe engine is not a
one-line change — it requires refactoring `TxnStore`'s `Rc` to `Arc` and re-verifying
the recovery machinery under sharing. The concurrency cores (`lockmgr`, `mvcc`) are
already complete and proven under real threads as standalone services (D-LOCK-2,
D-MVCC-2); this `Rc→Arc` refactor is the precondition for folding them into the
engine's own execution path. Recorded rather than rushed.

## D-WRITEUP-1 — The compendium consolidates the build (§11)
[`writeup.md`](writeup.md) is the project write-up the design asks for: the
architecture and how the crates compose, the four-lens testing philosophy that is the
real product, the **root-cause distribution** over the bug log (§7.5 — three bugs,
each from a *different* lens, which is the whole argument for the campaign's breadth),
the honest performance numbers (ablation ~2.1×, indexed lookup ~7× a scan, the TPC-H
subset timings), and the deferred list with the reason for each deferral. It is the
narrative counterpart to this ledger and the bug log — the place the whole edifice is
told as one story, with its partial guarantees named as partial.

## D-CI-1 — A dedicated `campaign` profile keeps assertions hot
`profile.campaign` inherits release optimization but keeps `debug-assertions` and
`overflow-checks` on and uses `panic = "unwind"` (the fault injector simulates
crashes in-process and must not abort the harness). Campaigns run under it so
invariants trip mid-run at speed.
