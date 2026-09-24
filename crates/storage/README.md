# chilidb-storage

Fixed-size page stores and a synchronous buffer pool. No heap tuple layout,
indexes, MVCC, transactions, WAL, or recovery are implemented.

## Use

```rust
use std::sync::Arc;
use chilidb_storage::{BufferPool, MemoryPageStore};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let pool = BufferPool::new(2, Arc::new(MemoryPageStore::new()))?;
let id = {
    let pin = pool.new_page()?;
    pin.write(|page| page[..4].copy_from_slice(b"data"))?;
    pin.page_id()
};
let pin = pool.pin(id)?;
assert_eq!(pin.read(|page| page[..4].to_vec())?, b"data");
// The pin retains residency, but no content lock is held here.
pool.flush_all()?;
# Ok(())
# }
```

## Page stores

Pages are 8 KiB arrays. PageId is a u32 stored-page address within one PageStore,
not a buffer-frame index. Allocation is append-only, returns a fresh ID, and
initializes the page to zero. Reads and writes never allocate implicitly.

PageStore separates I/O from buffering. MemoryPageStore uses in-memory pages;
FilePageStore uses one serialized seek/read/write file with no header. Reopening
a file derives its allocation count from its length. Misaligned files and lengths
outside the PageId address space are rejected.

Use only one buffer pool per store, and one open store instance per underlying
file. Direct writes outside the pool or external file modifications break cache
coherence. File locking and multi-file page identities are not implemented.

## Performance and correctness principles

Buffer management and the storage layer are performance-critical. Choose data
layout and synchronization to minimize memory footprint, cache-line traffic,
copying, and contention. Compact descriptors, contiguous aligned pages, and
fine-grained locking are design goals; a convenient Rust wrapper is not itself
an architectural requirement. Page bytes may live separately from their locks.

Small, audited unsafe sections are acceptable when needed for these goals.
Separating data from a lock does **not** relax Rust's aliasing, lifetime, or
memory-ordering requirements: undefined behavior and data races are never valid
performance optimizations. State the synchronization invariant at each unsafe
boundary, keep raw-pointer access private, and expose safe ownership/guard APIs.
Measure the resulting space and performance trade-offs.

This experimental implementation uses formal methods to examine protocols before
making concurrency optimizations. Stateright models (or TLA+ specifications) must
state their bounds, atomic steps, environmental assumptions, and checked safety
or progress properties. Include reachability witnesses to establish that the
checked scenarios are reachable. Model checking complements compiler lifetime checks, Miri, fault
injection, and implementation tests; a bounded abstract model is not a proof of
all Rust executions or of the hardware memory model.

The first [buffer-loading model](models/README.md) specifies victim reservation,
asynchronous loading, duplicate requests, and failure recovery. It is a proposed
protocol, not the current synchronous buffer pool's implementation.

## Buffer pool

BufferPool has a fixed, nonzero frame capacity. `new` selects `ClockStrategy`;
`with_strategy(capacity, store, Box<dyn VictimStrategy>)` accepts another policy.
The policy receives separate cache-hit (`access`) and installation (`load`)
notifications, plus an eligibility callback when choosing a victim. `load` allows
FIFO/LRU-K policies to reset history on slot reuse. A failed replacement does not
send `load`; merely selecting a victim must not remove it from policy tracking.
Internal flush pins do not update policy recency. It owns its own replacement state, independently of page
storage, pin counts, and I/O. CLOCK maintains reference bits and a rotating hand.

Unused frames are assigned in order without consulting the policy: while the
mapping has fewer entries than capacity, its length identifies the next unused
slot. Failed loads do not advance it. This relies on frames never being unassigned;
invalidation or concurrent loading reservations would require separate accounting.
There is no free-frame list. Only unpinned frames
are eligible for replacement, and the pool validates every returned candidate
before touching its bytes or performing I/O. A buggy strategy cannot bypass pins:
an out-of-range or pinned candidate returns InvalidVictim. Strategies must return
None only when no eligible candidate is found. Policy callbacks run under the
metadata mutex and must not reenter the pool.
If every frame is pinned, operations return AllPinned rather than block waiting
for a frame. new_page checks frame availability before allocating a stored page.

Page bytes occupy one contiguous, 8 KiB-aligned allocation, with each page at an
8 KiB stride. A separate contiguous array contains 64-byte-aligned descriptors.
Content locks are RwLock<()> fields in descriptors, not wrappers around pages.
The fixed allocation never grows or moves while pins exist.

`pin(PageId)` resolves or loads a stored page and returns a
`PinnedBuffer<&BufferPool>` that retains residency. Its `BufferId` identifies a resident slot, not a stored
page; slots can be reused after unpinning. PageId is the lookup key, so callers
cannot accidentally pin a stale buffer-slot identity.

`PinnedBuffer::read` acquires a shared content lock for its callback;
`PinnedBuffer::write` acquires an exclusive one. Both release the lock when the
callback returns or unwinds. The pin remains until PinnedBuffer drops, so callers
can retain residency across several short accesses without retaining the latch.
References into the page cannot escape the callbacks through safe Rust.

`PinnedBuffer<P: AsRef<BufferPool>>` stores its pool handle and two u32 values:
BufferId and PageId. Access and Drop resolve the handle through `AsRef`.
Private construction preserves the originating pool's identity for the pin's
lifetime. Pool methods create borrowed handles; `PinnedBuffer<&BufferPool>`
occupies 16 bytes on the supported 64-bit layout, and its borrow keeps the pool
alive.

Write access conservatively marks dirty before invoking the callback, while
holding the exclusive latch—even if the callback writes nothing or panics. Dirty
status is visible before unpinning. Pinning and read callbacks do not dirty pages.

UnsafeCell permits interior mutation of the separate page array. Unsafe reference
creation is confined to internal content-lock accessors; public callbacks cannot
bypass latching. Pins protect buffer identity and keep replacement from racing
latch holders or waiters. Metadata synchronization protects lookup and replacement;
resident-page callbacks acquire no metadata mutex while waiting on their latch.
Misses and allocation still serialize under the metadata mutex.

Do not nest content callbacks or flush a page from inside its callback. Locks are
not reentrant, and even recursive reads may block behind a waiting writer. Finish
the callback first; retaining its pin is safe. Latches are not transaction-duration
row locks.

Dirty victims are written before reuse. Incoming data is read into scratch storage;
the frame's identity and bytes change only after all fallible I/O succeeds. A
failed load, dirty writeback, or allocation retains the old resident mapping and
data for retry. Normal operations report poisoned locks rather than exposing
potentially inconsistent bytes; pin cleanup still occurs during unwinding.

## Flushing and durability

flush_page writes a resident dirty page and returns false for a nonresident page.
It does not fsync. Its read latch remains held through writeback and the clean-bit
update, so later writers must dirty the page again.

flush_all takes a snapshot of resident IDs, flushes them, then syncs the store.
Concurrent writers may dirty pages afterward, and new allocations may fall outside
the snapshot. Quiesce writers before using it for a durable close. Clean means
written to the backing store, not necessarily fsynced; sync errors are returned.

Dropping a pin or pool does not flush dirty buffered pages. Call flush_all explicitly and
handle errors. There is no crash-atomic page write, WAL-before-data enforcement,
transaction commit, or rollback. An I/O failure can leave a partially written
backing page even though the buffer retains its valid dirty copy.

## Boundary for the next layers

[`chilidb-heapam`](../heapam/README.md) implements slotted byte records, heap page
chains, and CTID membership/liveness checks above this layer. Pins guarantee
residency only. Transaction IDs/status and
snapshots, followed by tuple-version visibility and conflict handling, belong
above this byte-oriented layer. A successful flush is not a transaction commit.
