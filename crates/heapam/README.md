# chilidb-heapam

A pre-MVCC heap access method over `chilidb-storage`. Tuples are nonempty, opaque
byte records; this layer does not encode SQL values or enforce SQL constraints.

```rust
use std::sync::Arc;
use chilidb_heapam::HeapTable;
use chilidb_storage::{BufferPool, MemoryPageStore};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let pool = Arc::new(BufferPool::new(2, Arc::new(MemoryPageStore::new()))?);
let heap = HeapTable::create(pool.clone())?;
let ctid = heap.insert(b"hello")?;
assert_eq!(heap.get(ctid)?, Some(b"hello".to_vec()));
assert!(heap.replace(ctid, b"longer record")?);
for row in heap.scan()? {
    let (address, bytes) = row?;
    assert_eq!(address, ctid);
    assert_eq!(bytes, b"longer record");
}
assert!(heap.delete(ctid)?);
assert_eq!(heap.get(ctid)?, None);
pool.flush_all()?;
# Ok(())
# }
```

## Ownership and addressing

`HeapTable::create` allocates a root data page. Retain `head_page_id()` to reopen
with `HeapTable::open(pool, head)`. Open validates the complete page chain and
caches membership for constant-time CTID page lookup. A root ID is meaningful
only within its page store; it is not a query-local binder relation ID.

Open a heap once and share its handle with Arc. Separate handles for the same
root do not coordinate their cached chains. Do not modify its pages directly
through the buffer pool. A mutex serializes operations on a shared heap handle.
Heap operations pin pages and access their bytes through short read/write
callbacks. No heap mutex, page pin, or latch escapes into returned records.

A CTID consists of a store-local page ID and a one-based slot. The enclosing heap
validates page membership. Its page IDs need not be contiguous: other heaps can
allocate interleaved pages from the same pool. Compaction moves bytes, not slots.
Deleting a tuple leaves a tombstone and never reuses its slot ID, so that address
does not silently become a different record in this implementation.

## Page format

All integers use little-endian encoding; Rust struct layouts are never persisted.

| Byte range | Meaning |
| --- | --- |
| 0..8 | `CHILIHP1` format magic/version |
| 8..12 | Owning heap root page ID |
| 12..16 | Next page ID, meaningful only when byte 16 is 1 |
| 16..20 | Next-page-present flag and three zero reserved bytes |
| 20..24 | u16 slot count and u16 payload lower offset (`upper`) |

Four-byte slots grow upward from byte 24: u16 payload offset plus u16 length.
Payloads grow downward from the end of the 8 KiB page. A deleted slot has zero
offset and length. Empty records are rejected rather than confused with tombstones.
The largest record on a fresh page is `MAX_TUPLE_SIZE` (8,164 bytes).

Validation checks the format, directory/payload bounds, flags, and overlapping
live records. There are no checksums. Compaction reclaims deleted payload space,
not directory slots; no vacuum or page reclamation is implemented.

## Operations and scans

Insertion tries the tail page, then allocates and links a new page. There is no
free-space map to search older pages. Oversized records fail before allocation;
overflow pages and out-of-line values are not supported. Even a one-frame buffer
pool works: a new page is unpinned before the old tail is fetched to link it.

`get` copies bytes out. Missing/deleted slots on a member page return None;
addresses outside the heap return InvalidCtid. `delete` returns whether a live
record was removed. `replace` preserves the CTID and returns false for a missing
slot. If growth cannot fit on the same page, it returns NoSpace without changing
the record. Moving an updated tuple to another page is not implemented here.

A scan captures page and slot upper bounds at creation, excluding subsequently
inserted slots and pages. It copies at most one page's records at a time and holds
no pins across iterator calls. Replacements and deletions can affect pages not yet
read; records already copied into the scan are unchanged. This is **not a stable
transaction snapshot**. An error terminates the iterator.

## Persistence boundary

Use `BufferPool::flush_all` after quiescing mutations for clean-close persistence.
Neither heap Drop nor pin Drop performs writeback. Reopen with the same root
ID after dropping old heap/catalog handles.

Page edits are validated before publication, but multi-page mutations are not
crash atomic. An I/O failure while linking a new tail can leave an allocated,
unlinked page; the live handle does not publish its tuple or membership. Such
pages are not reclaimed. There is no WAL, undo, transaction isolation, MVCC,
statement rollback, or durable commit protocol. This is a storage foundation,
not a complete SQL table implementation.
