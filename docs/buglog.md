# KEEL — Bug Log

Every bug the campaigns catch gets an entry: a replayable trigger, a minimized
reproducer, the subsystem, the root-cause class, and the fix. The root-cause
*distribution* over this log is a first-class write-up figure (§7.5). Root-cause
classes: `recovery-protocol`, `latching`, `visibility`, `null-semantics`,
`optimizer-rewrite`, `spill-path`, `storage-invariant`, `test-harness`.

---

## KEEL-0001 — Dangling forward from a stale, recycled RID
* **Found by:** crash campaign v0 (`dbcheck/tests/crash_smoke.rs`,
  `vicious_crashes_are_always_detected`), during the pre-crash workload — a
  logic bug, surfaced not by the crash but by the workload the campaign drives.
* **Subsystem:** `heap`.
* **Root-cause class:** `storage-invariant`.
* **Symptom:** `panic: unexpected page error on set at Rid { page: 6, slot: 39 }
  tag=2: BadSlot` — a live forward stub pointed at a tombstoned slot, and a later
  read chased the dangling forward.
* **Minimized reproducer** (now `heap::tests::stale_rid_recycled_as_forward_target_is_inert`):
  1. Insert a logical tuple `R`, then `delete(R)` — `R`'s slot is now a tombstone.
  2. Force some other tuple to forward; its internal `ForwardTarget` insert
     **recycles `R`'s freed slot**. Now a stub `S` points at `R`'s old slot.
  3. `delete(R)` again (stale RID): the old code saw tag `ForwardTarget`, took
     the "not a stub, just tombstone it" path, and deleted `S`'s target — leaving
     `S` dangling.
  4. A later `update`/`get` through `S` set/read the tombstoned slot → `BadSlot`.
* **Root cause:** the heap recycles a deleted logical slot as an internal forward
  target, but a *stale client RID* to that slot could still address it. The
  operations assumed a client RID only ever names a `Tuple` or a `Forward`.
* **Fix:** make the contract explicit (D-HEAP-1 / `semantics.md` stale-RID rule).
  A client RID that resolves to `ForwardTarget` is stale: `get` returns `None`,
  `delete`/`update` return `false`, and none of them touch the target (it belongs
  to another stub). Landed in `heap::{get, delete, update}` this session.
* **Lesson:** slot recycling across the logical-tuple / internal-target boundary
  is a use-after-free waiting to happen; the tag disambiguates it, and the engine
  must treat a stale RID as inert rather than trusting it. Exactly the
  "use-after-free with a search warrant" the design flags for vacuum (§5.2) — met
  early, in the heap.

---

## KEEL-0002 — ORDER BY on a non-selected column rejected
* **Found by:** the storage-vs-reference differential campaign
  (`db/tests/differential.rs`).
* **Subsystem:** `sql` (reference engine).
* **Root-cause class:** `null-semantics` (SQL semantics).
* **Symptom:** `SELECT name, CASE ... END FROM emp WHERE id < 10 ORDER BY id`
  errored `ORDER BY unknown column 'id'`, because `id` is not in the SELECT list.
* **Root cause:** the sort resolved ORDER BY keys only against *output* columns,
  but SQL allows ORDER BY over any column in scope (and over output aliases and
  1-based positions).
* **Fix:** carry a sort key per output row, resolving each ORDER BY expression as
  an output alias/name, a positional literal, or — falling back — an expression
  evaluated against the source row (or group). Landed in `refengine::run_select`
  / `resolve_order_key`.
* **Lesson:** ORDER BY binds against the FROM scope, not the projection — a
  classic SQL subtlety the differential oracle surfaced immediately.

---

## KEEL-0003 — Aborted version poisons every future update (MVCC livelock)
* **Found by:** the MVCC threaded stress test (`mvcc/tests/threaded.rs`,
  `first_updater_wins_prevents_lost_updates_under_threads`) — 8 threads running
  bank transfers hit the retry cap and the whole test hung; a livelock, not a
  crash.
* **Subsystem:** `mvcc` (`MvccStore::update`).
* **Root-cause class:** `visibility` (concurrency-control).
* **Symptom:** under contention every `update` on a hot row returned
  `WriteConflict` forever, so no transfer could ever commit.
* **Minimized reproducer** (now `mvcc::tests::aborted_version_does_not_poison_future_updates`):
  1. `bootstrap_row(x)`. 2. A transaction updates `x`, then **aborts** — leaving a
  version with `xmin = (aborted txn)` as the physically newest entry in `x`'s
  chain. 3. Any later `update(x)` read `versions.last()` (that aborted version),
  found its creator not committed, and returned `WriteConflict` — and, aborting,
  appended *another* aborted version, so the poison was self-perpetuating.
* **Root cause:** first-updater-wins tested the *physically* newest version, but an
  aborted version is *logically dead* and must be transparent. The single-threaded
  tests never retried an aborted transaction against the same row, so it hid.
* **Fix:** check (and supersede) the newest **non-aborted** version —
  `versions.iter().rposition(|v| clog.get(v.xmin) != Aborted)`. Landed in
  `MvccStore::update`.
* **Lesson:** a concurrency-control rule written against "the newest version" has
  to mean the newest *live* version; dead versions at the tail are a trap that only
  a real multi-threaded retry loop exposes — precisely why the design mandates the
  under-real-threads stress (P7). The proper cure for the accumulating dead
  versions is vacuum (§5.2), still deferred; skipping them keeps correctness in the
  meantime.

---

## KEEL-0004 — Stale page checksum after a compaction that still returned PageFull
* **Found by:** adversarial multi-lens review of `cheap` (4 confirmed / 13 raised),
  not by the crate's three green oracles — the differential, the concurrent race,
  and the crash campaign all passed while this was live.
* **Subsystem:** `cheap` (concurrent record heap).
* **Root-cause class:** `storage-invariant`.
* **Symptom:** after a `checkpoint`, an **intact** page holding committed records
  failed `verify_checksum()`, so any checksum-verifying reader (crash recovery,
  `dbcheck`) would classify it as torn and discard its records — silent loss of
  committed data on the recovery path, from a false-positive tear.
* **Minimized reproducer** (now `cheap/tests/checksum.rs::pagefull_after_compaction_keeps_checksum_valid`):
  1. Insert a near-page-sized record into page 0, then a small one.
  2. `delete` the small one — page 0 now holds a tombstone.
  3. Insert a record that cannot fit: `SlottedPage::insert` runs `compact()` to
     reclaim the hole, still doesn't fit, and returns `PageFull`.
  4. `checkpoint`, reopen → page 0's checksum no longer matches its bytes.
* **Root cause:** `Heap::insert` recomputed the checksum only on the **success**
  arm. But `PageRef::write()` marks the frame dirty *unconditionally*, and
  `compact()` rewrites header bytes (`free_end`, inside the CRC body) even when the
  insert ultimately fails. So the PageFull path left a dirty page whose stored
  checksum described its pre-compaction bytes, and the flush persisted it verbatim.
* **Fix:** recompute the checksum on **every** path that took the write latch —
  compute `outcome`, `sp.recompute_checksum()`, *then* match. Landed in
  `Heap::insert`.
* **Lesson:** the oracles all read through `scan`/`get`, which never verify
  checksums, so a page could be *data-correct but checksum-stale* and every test
  stayed green. The invariant "a dirtied page always leaves the latch with a valid
  checksum" needed its own test, not a data-equality one. Confirmed non-vacuous:
  reverting the fix fails the deterministic case (2 bad pages) and the randomized
  one (**20** bad pages on seed 0).
* **Systemic fix (beyond the point bug):** a regression test for *this* bug does not
  stop the *class*. `Heap::verify` was added as `cheap`'s `dbcheck` rule (D12) — it
  walks every page and reports bad checksums, foreign page types, and the live-record
  count — and is now asserted from the differential, threaded, and checkpoint-race
  oracles. Measured effect, re-introducing the exact bug: the **differential oracle
  goes from blind to catching it** (it deletes, so it forms the tombstones that make
  `compact()` actually mutate). The three insert-only oracles still pass, and that is
  correct rather than a gap — with no deletes there are no tombstones, `compact()` is
  byte-identical, and the stale checksum is never produced. Before: 0 of 5 oracles
  caught it; after: 2 of 5, and the other 3 cannot produce it by construction.
* **Method note:** the first attempt to prove the regression test non-vacuous was
  invalid twice over — one revert failed to compile (its compile errors would have
  been miscounted as bug detections), and a later one over-neutered (removing the
  checksum update from the *success* path too, a far grosser bug than KEEL-0004).
  When reverting a fix to prove a test catches it, **verify the reverted code both
  compiles and reproduces the specific defect, not a bigger one.**

---

## KEEL-0005 — A failed flush marked an unflushed victim clean, so checkpoint skipped it
* **Found by:** adversarial review (durability lens) of `cbuffer`.
* **Subsystem:** `cbuffer` (concurrent page cache).
* **Root-cause class:** `recovery-protocol`.
* **Symptom:** with a write error on the eviction flush (e.g. `ENOSPC` — a
  documented recurring condition on this machine), a page's modifications stayed
  only in memory yet its frame was marked **clean**. A subsequent `checkpoint()`
  filters on `dirty`, so it skipped the page, `sync()`ed, and returned `Ok` — and
  the data was lost on power loss although every call had succeeded.
* **Minimized reproducer** (now `cbuffer/tests/flush_fail.rs`): a `BlockFile` that
  fails exactly one `write_at`; write a record into page 0, arm the disk, allocate
  page 1 (cap 1) so evicting page 0 flushes and fails, then `checkpoint` and reopen
  — the record is absent from disk.
* **Root cause:** `abort_reservation` unconditionally set `dirty = false`. That is
  right for the *read*-failure caller (its flush already succeeded, so the old page
  really is clean on disk) but wrong for the two *flush*-failure callers, where the
  old page's changes never reached the disk at all.
* **Fix:** pass the intended state — `abort_reservation(victim, pid, victim_dirty)`;
  the flush-failure paths in `fetch` and `new_page` pass `true` so the page stays
  dirty and a later checkpoint retries it. Landed in `cbuffer`.
* **Lesson:** an error path that "restores" state must distinguish *which* step
  failed. Collapsing "the page is clean on disk" and "we failed to clean it" into
  one flag turned a surfaced I/O error into silent, deferred data loss. Confirmed
  non-vacuous: reverting the fix fails the reproducer.

---

## KEEL-0006 — flush_all cleared the dirty flag outside the buffer guard (concurrent-checkpoint race)
* **Found by:** adversarial review (concurrency lens) of `cbuffer`.
* **Subsystem:** `cbuffer`.
* **Root-cause class:** `latching`.
* **Symptom:** a checkpoint running **concurrently with an insert** could mark a
  page clean whose newest record it had not written. On the next eviction the clean
  frame is discarded without flushing, so the record vanishes — **without any
  crash** — and its returned RID dangles.
* **Trigger (by inspection; see Lesson):** `flush_all` wrote page P's bytes with the
  buffer guard released, then took the directory lock and cleared `dirty`. An insert
  that acquired the page between those two steps set `dirty = true` and added a
  record; the clear then clobbered that flag.
* **Root cause:** the dirty flag's transitions were decoupled from the buffer lock
  that protects the bytes the flag describes, so flag and bytes could be observed
  out of step.
* **Fix:** couple both transitions to the buffer lock. `PageRef::write` now sets
  `dirty` **while holding the buffer write guard**; `flush_all` holds the buffer
  **read** guard across the disk write *and* the clear, and only clears if the frame
  still holds that page and isn't mid-eviction. Buffer-before-directory ordering is
  uniform, so the two cannot deadlock.
* **Lesson:** **honest scope** — unlike KEEL-0004/0005, this one is *not*
  deterministically reproducible: the window is a single lock acquire, and a
  compiling clear-after-release variant passed the concurrent stress 10/10 runs. It
  is a real, legal interleaving fixed by reasoning and a standard lock-coupling, and
  `cheap/tests/checkpoint_race.rs` guards the concurrent path (no lost/torn record,
  no deadlock) — but that test does not *prove* the fix by failing without it, and
  is documented as a stress guard rather than a reproducer.

---

## KEEL-0007 — flush_all wrote a repurposed frame's bytes to the evicted page's offset
* **Found by:** adversarial audit of the concurrent foundation (13 raised → 9 confirmed
  / 4 refuted). Reported independently by three lenses; I had also just reasoned about
  this exact race while fixing KEEL-0006 and **guarded the wrong operation**.
* **Subsystem:** `cbuffer`. **Root-cause class:** `latching`.
* **Symptom:** silent **cross-page corruption that defeats the checksum layer**. The
  bytes landed at page P's offset are a complete, self-consistent image of a *different*
  page Q, carrying a valid CRC over Q's payload — so `dbcheck`, `ckv::intact` and
  `cheap::verify` all report clean while page P's contents are gone.
* **Reproducer:** `flush_all` snapshots `(idx, P, buf)` under the directory lock, then
  releases it. It never pins the frame, so `choose_victim` may immediately select `idx`:
  a concurrent `fetch(Q)` flushes P correctly, reads Q into the *same buffer*, and
  publishes. `flush_all` resumes, takes `buf.read()` — now Q's bytes — and writes them to
  `page_offset(P)`.
* **Root cause:** the identity `(idx → P)` was captured once and never re-validated
  before the write. The existing guard checked identity only around the dirty-flag
  *clear*, three lines after the damaging write.
* **Fix:** take the buffer read guard **first**, then re-validate `frames[idx].pid ==
  Some(pid)` under the directory lock before issuing `write_at`, skipping on mismatch.
  While the read guard is held the bytes cannot change (a repurpose needs the write guard
  to `read_at`), so a validated identity is stable for the whole write.
  Buffer-before-directory ordering is preserved, so no deadlock.
* **Lesson:** when a value is captured under a lock and used after releasing it, the
  *use* needs re-validation, not the bookkeeping around it. I had reasoned about this
  race and dismissed it as "pre-existing/orthogonal" — being adjacent to a bug is not the
  same as having checked it.

---

## KEEL-0008 — checkpoint returned Ok while skipping a dirty mid-eviction page
* **Found by:** adversarial audit (durability lens). **Subsystem:** `cbuffer`.
  **Root-cause class:** `recovery-protocol`.
* **Symptom:** `checkpoint()` returned `Ok` before an already-completed write reached
  disk, so it was lost on power loss — a durability-barrier violation.
* **Root cause:** `flush_all`'s work list filtered on `f.dirty && !f.busy`, silently
  excluding frames mid-eviction. Such a frame *is* flushed by the evictor, but possibly
  after our `file.sync()`, so the barrier did not cover it.
* **Fix:** include dirty busy frames and **wait them out** on the `ready` condvar before
  deciding. Once the eviction completes the page is either still ours (we flush it) or
  gone (the evictor already wrote it, WAL-before-data).
* **Lesson:** "someone else will handle it" is not durability. A barrier must *observe*
  the handoff completing, not assume it.

---

## KEEL-0009 — Read-failure abort restored page identity but not bytes
* **Found by:** adversarial audit (fetch lens). **Subsystem:** `cbuffer`.
  **Root-cause class:** `storage-invariant`.
* **Symptom:** after a failed page read the frame kept claiming the *old* page while
  holding indeterminate bytes (`read_at` may partially fill before erroring, as `OsFile`'s
  pread loop can). The next `fetch(old)` was a cache **hit** serving corrupted data.
* **Root cause:** one undo path served two different failures. `abort_reservation`
  restored the old identity unconditionally — right after a *flush* failure, wrong after a
  *read* failure.
* **Fix:** split the undo into an explicit `Abort::{KeepOldDirty, Discard}`. Read failure
  now empties the frame (unmap, `pid = None`), so the next fetch re-reads from disk.
* **Lesson:** the same shape as KEEL-0005 — an error path must distinguish *which* step
  failed. That one lost data; this one served corruption. An earlier review had *refuted*
  a related finding as "MemDisk/FaultDisk are all-or-nothing, OsFile-only" — true, but
  `OsFile` is the production backend, so that refutation bounded the blast radius rather
  than dismissing the bug.

---

## KEEL-0010 — ckv never validated a bucket's CRC, and laundered corruption on write
* **Found by:** adversarial audit (ckv concurrency + durability lenses).
  **Subsystem:** `ckv`. **Root-cause class:** `storage-invariant`.
* **Symptom:** `get`/`total` decoded a torn page as if it were data — the opposite of the
  crate's documented "corruption is never silent". Worse, `put`/`update` read-modify-wrote
  a torn page and called `seal`, stamping a **fresh valid CRC over the damage** and
  permanently laundering it into a page every later check calls intact.
* **Fix:** validate `intact(&b)` immediately after taking the guard in `get`, `put`,
  `update`, and `total`, returning `Corrupt(bucket)` before any decode or re-seal.
* **Lesson:** a checksum only helps on paths that *consult* it. Having `intact()` and a
  `Corrupt` variant made the guarantee look implemented; only two of six paths used them.

---

## KEEL-0011 — A failed allocation stranded a page id below page_count()
* **Found by:** adversarial audit (fetch lens). **Subsystem:** `cbuffer`.
  **Root-cause class:** `storage-invariant`.
* **Symptom:** after `new_page` failed its eviction flush, `next_page` stayed bumped, so
  `page_count()` advertised an id the file was too short to satisfy. Every full scan
  (`cheap::scan`, `cheap::verify`) then failed on an unreadable hole.
* **Root cause:** I had written the comment "ids are unique, not required to be gap-free"
  — true for uniqueness, but `page_count()` is what full scans *walk*, so a gap below it
  is not benign.
* **Fix:** roll `next_page` back on the failure path when no other allocation intervened.
  Regression test asserts `page_count()` and that every page below it is readable;
  verified non-vacuous by removing the rollback.
* **Lesson:** a comment asserting an invariant is not the same as checking it against
  every consumer of that invariant.

---

## KEEL-0012 — The ckv crash test was structurally unfalsifiable
* **Found by:** adversarial audit (test-rigor lens). **Subsystem:** `ckv` tests.
  **Root-cause class:** `test-harness`.
* **Symptom:** the crash campaign **passed unchanged with checksum verification removed**,
  so it never tested the "corruption is detected" guarantee it claimed.
* **Root cause:** both rounds wrote the *same* entry count to each bucket, so tearing
  produced no structurally observable inconsistency; and across 24 seeds the adversary
  mostly *dropped* pending writes (leaving the intact checkpointed page) rather than
  mixing sectors into a plausible-but-wrong page.
* **Fix (and a failed first attempt, recorded honestly):** first I strengthened the crash
  test — round 2 now also *adds* keys so the entry count differs, plus an assertion that
  every returned key actually hashes to the bucket it came from. **That did not achieve
  falsifiability**: it still passed with the gate removed, because the workload does not
  reliably generate a readable torn page. The real fix is a *positive* detection test
  (`tests/detect.rs`): flip one byte in a bucket page on disk and assert every read path —
  `get`, `put`, `update`, `total`, `bucket_entries`, `bucket_intact` — reports `Corrupt`,
  while an untouched bucket stays fully usable. Verified falsifiable: with the five gates
  removed it fails immediately.
* **Lesson:** a crash campaign proves *whatever survived*, not that detection works.
  Testing a detection guarantee needs corruption you injected on purpose and can point at.
  The falsifiability check — delete the mechanism, confirm the test fails — is the only way
  to know a test tests anything.
