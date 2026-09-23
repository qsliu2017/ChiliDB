# Heap-backed catalog

`HeapCatalog::create` allocates a dedicated metadata heap containing a recognizable,
versioned manifest. Retain `root_page_id()` externally and pass it to `open`; the
root is not necessarily page zero. Table records contain names, column types and
nullability, constraint column indices, and table heap heads in a compact binary
format. No sidecar file or query-local relation identity is used. These are
metadata records, not SQL-queryable pg_class/pg_attribute system tables.

Share one catalog instance per root using `Arc`. Creation and lookup serialize
through a mutex. Cached `HeapTableSource` handles preserve `Arc` identity across
lookups and expose immutable schemas, declared constraints, and table heaps.
Opening loads and validates all metadata, rejecting missing manifests, malformed
records, duplicate names, and duplicate or self-referencing heap roots.

Names are stored exactly as supplied; the binder performs SQL identifier
normalization before `create_table` receives a bound declaration. This method is
a library operation, not SQL command dispatch in the query executor.

Declarations and metadata size are validated before allocating a table heap;
duplicate names also fail before allocation. Publication failures can orphan an
unpublished heap page. There is no WAL, crash-atomic publication, MVCC, DROP, or
ALTER. A successful buffer-pool flush with mutations quiesced permits reopening using
the retained root, after dropping old catalog and table handles.
Constraints are metadata only: row-level uniqueness, primary-key and nullability
enforcement belongs to higher layers and is not implemented here.
