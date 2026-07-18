# KEEL — Semantics (the tiebreaker)

When a question about behavior comes up, it is settled here the day it is
settled, and this file wins. Storage semantics come first; the SQL 3-valued logic,
the MVCC visibility rules, and the logical-WAL / transaction / compaction model
follow in their own sections below.

## Values and NULL

The freeze-set scalar types are `bool`, `int` (i32), `bigint` (i64), `double`
(f64), and `varchar(n)`. `NULL` is a first-class value of any column type.

Ordering of values (`keel_types::Value::total_cmp`) is a **total** order used by
tests and, later, by sort/group operators:

* `NULL` sorts below every non-NULL value (NULL-low). This matches the key codec.
* `double` uses the IEEE-754 total-order transform: `-0.0 < 0.0`, and NaN sorts
  at the high end. The freeze set does not produce NaN, but the order is defined.
* Cross-type comparisons fall back to a fixed type rank so any collection of
  mixed values remains well-ordered (this situation does not arise within one
  typed column, but the function is total).

## Three-valued logic (SQL, the reference engine is the yardstick)

Predicates evaluate in `{TRUE, FALSE, NULL}`, and the reference engine implements
this exactly (it is the oracle every executor is differenced against):

* **Comparison** (`= <> < <= > >=`) with any `NULL` operand is `NULL` — never
  `TRUE`. In particular `x = NULL` is `NULL`, so it matches nothing; use `IS NULL`.
* **`AND`**: `FALSE` dominates (`FALSE AND anything = FALSE`); `TRUE AND TRUE =
  TRUE`; every other combination is `NULL`.
* **`OR`**: `TRUE` dominates; `FALSE OR FALSE = FALSE`; else `NULL`.
* **`NOT`**: `NOT NULL = NULL`, `NOT TRUE = FALSE`, `NOT FALSE = TRUE`.
* **A `WHERE` / `HAVING` / join `ON` predicate keeps a row only when it is `TRUE`** —
  both `FALSE` and `NULL` drop it. (The streaming and vectorized executors apply the
  identical rule.)
* **`IS [NOT] NULL`** is two-valued (never `NULL`).
* **`[NOT] IN`**: `IN` is `NULL` if the probe is `NULL`; else `TRUE` on a match,
  `NULL` if no match but the list/subquery contains a `NULL`, `FALSE` otherwise.
  `NOT IN` is the negation with the same `NULL` propagation (so `x NOT IN (…, NULL)`
  is never `TRUE`).
* **Arithmetic** with a `NULL` operand is `NULL`. Integer arithmetic wraps
  (two's-complement); division by zero is an error, not a `NULL`.
* **Aggregates skip NULLs.** `COUNT(*)` counts rows; `COUNT(expr)`/`SUM`/`AVG`/
  `MIN`/`MAX` ignore `NULL` inputs. Over an empty (or all-`NULL`) group, `COUNT` is
  `0` but `SUM`/`AVG`/`MIN`/`MAX` are `NULL`. `SUM` of integers stays an integer
  (wrapping) unless a `double` promotes it; `AVG` is always a `double`.

This is policed continuously by the three-engine differential and the TLP
metamorphic fuzzer.

## Record (tuple) encoding

A heap tuple's user bytes are `encode_record(schema, row)`:

```
[ null bitmap: ceil(ncols / 8) bytes, bit i set  => column i is NULL ]
[ for each NON-NULL column, in column order:
    bool     -> 1 byte (0 / 1)
    int      -> 4 bytes little-endian
    bigint   -> 8 bytes little-endian
    double   -> 8 bytes, IEEE bits, little-endian
    varchar  -> u16 length little-endian, then the UTF-8 bytes ]
```

This is a deliberately simple, obviously-correct layout for P1. The offset-table
layout (fixed fields packed, varlen in a trailing section) is a later space
optimization and does not change semantics. `NOT NULL` and `varchar(n)` length
are enforced at encode time.

## Normalized keys (`memcmp` order == logical order)

`keel_keys` encodes a typed tuple to bytes whose lexicographic (`memcmp`) order
equals the tuple's logical order (D9). Each field:

* **Presence tag** — `0x00` for NULL, `0x01` for present. NULL-low, and composite
  keys parse unambiguously.
* **int / bigint** — big-endian with the sign bit flipped (`x ^ 2^(w-1)`), so
  negatives sort below positives under unsigned byte compare.
* **double** — the IEEE total-order transform, big-endian.
* **bool** — one byte, `0x00 < 0x01`.
* **varchar** — order-preserving escape: `0x00 -> 0x00 0xFF`, terminated by
  `0x00 0x00`. A string that is a prefix of another sorts before it; embedded
  NULs are safe.
* **composite** — concatenation of the per-field encodings. Every field encoding
  is fixed-width or self-terminating, so concatenation preserves tuple order and
  stays decodable.

Invariant (property-tested, the P2 gate): for any values `a`, `b` of the same
type, `encode(a) <= encode(b)` **iff** `a <= b` under `Value::total_cmp`.

## Heap and RIDs

A `RID = (page: u32, slot: u16)` is a tuple's permanent address. Slot indices are
stable across page compaction (D-PAGE-2). An update that outgrows its page leaves
a **forwarding stub** at the old RID, so indexes need no update on a move; chains
are held to length one (D-HEAP-1). A `scan` yields every logical tuple exactly
once under its stable RID (forward targets are skipped).

**Stale RID rule:** once a RID is deleted, it is dead. The engine may recycle its
slot internally. A stale RID therefore reads as absent; if it happens to resolve
to a `ForwardTarget`, operations on it are inert and never disturb the real
tuple. Holding a RID across its own deletion is a use-after-free on the client's
part; the engine's contract is only that it will not corrupt or panic.

## B+-tree index

A B+-tree maps normalized key bytes to a `RID` (D8). Because keys are
`memcmp`-comparable (above), the tree compares keys as raw byte slices and is
entirely type-oblivious. Semantics:

* **Unique keys.** `insert` of an existing key replaces its RID (upsert). This is
  exactly a primary-key index. Duplicate-key secondary indexes (RID-lists, or
  RID-suffixed keys) are a later extension.
* **Ordering.** `range(lo, hi)` returns entries with `lo <= key < hi` in
  ascending key order; `scan_all` returns all entries in key order via the leaf
  sibling chain.
* **Structure.** Leaves hold sorted `key -> RID` entries with left/right sibling
  links; internal nodes hold separator keys (a separator is the smallest key of
  the subtree to its right) and child pointers. Splits copy up (leaf) / push up
  (internal) at the byte midpoint. Deletes are lazy: the entry is removed, and
  underflow is tolerated (no merge yet), so the tree's height never shrinks. Each
  index is its own file; page 0 is a meta page holding the root pointer.
* **Invariants** (`btree::check`, the referee): keys strictly ascending within a
  node; every key within its subtree's separator bounds; all leaves at equal
  depth (balance); the sibling chain visits leaves in key order and agrees with
  an in-order traversal; `children == keys + 1`.

## Durability model

* **fsync policy** (`FsyncPolicy`): `Full` (fsync), `DataOnly` (fdatasync),
  `OffForBenchmarksOnly`. Every reported number must state which one it ran under.
* **WAL-before-data** (D-BUF-1): a dirty page is never written to disk until the
  log is durable through that page's `pageLSN`. One assertion, in
  `BufferPool::flush_frame`. (Vacuously true in P1; load-bearing from P3.)
* **Page self-verification**: every page carries a CRC32 over its body. A torn or
  rotted page fails verification on load and is reported (`BufferError::Corrupt`),
  never silently trusted.

## Write-ahead log and recovery (the ARIES ladder)

The `wal` crate implements the full ARIES ladder (D5), rung selected by `Policy`;
serial transactions (D3). **Rung 1** (D-WAL-1) is redo-only WAL with a
no-steal/force buffer policy. **Rung 2** is steal + no-force + full-page writes:
commit is durable on the log fsync alone, data is written lazily and redone, and
the first touch of a page each epoch logs a full-page image so a torn on-disk
page is fully reconstructable. **Rung 3** (D-WAL-2) completes ARIES: records carry
before-images, abort and loser-recovery roll back physically via Compensation Log
Records (`undo_next` makes an interrupted undo resumable), and `recover_aries`
runs Analysis → Redo (repeat history) → Undo (losers) and is idempotent under
repeated interruption. `checkpoint()` bounds the redo start. The remainder of this
section describes rung 1's shape; rungs 2–3 layer on it.

* **Log record** = `{lsn (byte offset), prev_lsn, txn, kind}`, CRC-framed. Kinds:
  `Begin`, `Update{page, body-offset, after-image bytes}`, `PageInit{page,type}`,
  `Commit`, `Abort`. A torn/incomplete tail record is detected by its CRC and
  marks the end of the durable log.
* **`log_and_apply`** is the sole page-mutation path: append the redo record,
  stamp the page's `pageLSN` with that record's LSN, then apply the change.
* **Commit** appends `Commit`, fsyncs the log through it (WAL-before-data), then
  forces the transaction's dirty pages and fsyncs the data file. Only after that
  does commit return, so a returned commit is durable.
* **Abort** discards the transaction's dirty pages (no-steal guarantees they were
  never written, so the durable version is intact); no undo is needed.
* **Recovery** = analysis (collect committed txns) + one redo pass over their
  records in LSN order, applying each only where `pageLSN < recordLSN`. With no
  checkpoints yet, the whole log is replayed, so committed state is reconstructed
  even onto an empty or torn data file.

**Guarantee (crash campaign, rung 1):** after any crash, `recover` reproduces
exactly the set of transactions whose `commit` returned before the crash — no
more (uncommitted work vanishes), no less (committed work is durable), and each
atomically (a multi-page transfer is all-or-nothing). The bank-accounts test
asserts the recovered file equals the committed model byte-for-byte.

## Crash model (the fault disk)

`keel_faultfs` models the honest disk of the ALICE work (Pillai et al., OSDI'14):

* a write becomes durable only after a following `sync`;
* between two syncs, un-synced writes may be reordered arbitrarily;
* across a sync there is no reordering (durability is a barrier);
* on a crash, an un-synced write may land fully, partially (torn at 512-byte
  sector boundaries), or not at all.

Every fault decision is drawn from a seed; a crash is fully described by
`(disk seed, crash schedule)` and replays byte-for-byte.

**P1 guarantee (crash campaign v0):** without recovery, a crash may leave the
database *inconsistent*, but never *silently* so. Every torn page is detected by
its checksum; any downstream inconsistency (a dangling forward) is only ever a
consequence of a detected torn page. Closing that gap — turning "detected" into
"repaired" — is exactly what the ARIES ladder does at P3.

## MVCC — snapshot isolation and serializable SI

Each row is a newest-first chain of versions `(xmin, xmax, value)`; `xmin` is the
transaction that created a version, `xmax` (or `INVALID`) the one that deleted it. A
transaction takes a **snapshot** at begin — `{xmax = next txn id, in_flight = the
set of txns active then}` — and the visibility rule (`visible`, exhaustively
matrix-tested) is:

* an insert `xmin` is visible iff it is the reader's own *or* it committed **before**
  the snapshot (`xmin < snapshot.xmax`, not in `in_flight`, and status `Committed`);
* a delete `xmax` hides the tuple under the same test; otherwise the tuple survives;
* a read returns the newest **visible** version, giving each transaction a stable
  view of the database as of its begin.

**First-updater-wins.** An `update` conflicts (`WriteConflict`, the transaction must
retry) if the row's newest **non-aborted** version was created by a transaction
concurrent with the writer (not committed-before-snapshot, not the writer's own).
Testing the newest *non-aborted* version is load-bearing: an aborted version left at
the chain tail is logically dead and must be skipped, or it poisons every future
update (KEEL-0003).

**Serializable SI (R1).** `SsiStore` tracks each transaction's read/write sets and
the rw-antidependency edges between concurrent transactions, and aborts the *pivot*
of a dangerous structure (a transaction with an inbound edge and an outbound edge to
an already-committed transaction — Fekete/Cahill). It is sound (never commits a
non-serializable schedule) and deliberately not complete (it may abort some
serializable ones). The visible effect: the write-skew schedule SI commits, SSI
rejects — the identical schedule, opposite outcome.

**Vacuum.** Dead versions are reclaimed: aborted versions always (invisible to
everyone), and — when no transaction is active — committed versions older than a
row's newest committed one (a future snapshot resolves only to the newest). An
in-progress transaction's own writes and any version an active reader can still see
are never pruned.

## Logical WAL, transactions, and compaction (the SQL durability model)

`Database::open` is checkpoint-durable (every mutating statement flushes + fsyncs the
data file). `Database::open_logged(data, log, frames)` instead routes DML through a
**logical statement WAL**:

* **Log-before-data, no-steal.** A mutating statement is appended to the redo log and
  fsynced *before* it is applied, and the buffer never flushes the data file during
  the session, so the log is the sole durable record. Recovery replays the log onto
  the loaded (empty, until a compaction) data image; because statements are
  deterministic, the committed state is reconstructed exactly. This is the SQL-level
  analog of the physical rung-1 property.
* **Transactions.** `BEGIN` opens a transaction whose mutations are *buffered*;
  `COMMIT` writes them as one crash-atomic unit (each statement an `S`-record, then a
  single `C` marker, all fsynced) and applies them in order; `ROLLBACK` discards the
  buffer, having logged and applied nothing. Recovery applies only committed units —
  a batch with no trailing `C` (a transaction the crash cut off, or a torn tail) is
  dropped. **Read-your-writes:** a `SELECT` inside an open transaction sees the
  committed state *plus* the transaction's own buffered mutations (via an overlay);
  the durable state is untouched until commit, which is what keeps rollback trivial.
* **Compaction.** `compact` appends a snapshot — a minimal statement script that
  reconstructs the current committed state — bracketed by `B`/`E` marker records.
  Recovery replays from the **last complete** `B…E` (bounding recovery work) and
  ignores everything before it; a crash mid-compact leaves a dangling `B` that
  recovery skips, so the prior state survives. It is torn-safe by construction
  (append-only; the snapshot is used only once its closing `E` is durable).

Honest boundary: `open_logged` durability is statement-granular and the data image is
only advanced by `compact`; reclaiming the log's dead prefix **bytes** (versus its
recovery *time*, which compaction already bounds) needs the page-LSN physical redo —
the rung-2/3 machinery — and is deferred.
