# chilidb-common

Shared identifiers without planner, Arrow, buffer-manager, or storage dependencies.

## CTID

`Ctid { page_id: u32, slot_id: NonZeroU16 }` identifies one physical tuple version
within a relation. Page IDs are zero-based; tuple-directory slots are one-based.
A page ID is not a buffer-frame index; a slot is not a byte offset. The relation
is supplied by the scan or modification
target, not embedded in the CTID.

```rust
use chilidb_common::Ctid;

let ctid = Ctid::new(42, 3).unwrap();
assert_eq!(ctid.page_id, 42);
assert_eq!(ctid.slot_id.get(), 3);
assert_eq!(Ctid::from_le_bytes(ctid.to_le_bytes()), Some(ctid));
assert!(Ctid::new(42, 0).is_none());
```

The canonical execution-interchange encoding is six bytes: four little-endian
page-ID bytes followed by two little-endian slot-ID bytes. Serialize with the
provided methods, not by copying the Rust struct's memory or padding. This is
neither PostgreSQL ABI compatibility nor an on-disk heap-layout specification.

Representable addresses are not necessarily live tuples. UPDATE can create a
version with another CTID; reclamation can reuse an old address. Executors must
acquire pins/latches as needed and validate visibility and concurrent modification
before modifying rows. A CTID contains no pin, lock, transaction ID, or visibility proof.

Planning carries this encoding in an internal non-nullable Arrow
`FixedSizeBinary(6)` column named `__ctid`. It is not a SQL-visible system column.
Sources participating in UPDATE/DELETE must supply this tuple-version address;
query-only sources need not supply one.
