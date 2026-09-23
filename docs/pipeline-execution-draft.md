# Draft: source-selected tuple and columnar pipelines

## Scope and status

This is an exploratory alternative, not the active execution architecture.
ChiliDB uses DataFusion's native physical planner and pull-based Arrow executor.
Page stores, buffering, byte-record heaps, and heap-backed metadata are implemented
separately. SQL tuple encoding, table-scan integration, MVCC, and transaction
management remain future work.

DataFusion remains the logical optimization representation. Using that
representation does not require DataFusion's RecordBatch-stream executor.
This alternative would lower optimized logical plans into separate physical
operators and explicit pipelines; the default backend does not take that path.

## Goal

Keep heap-sourced streaming work tuple-based so filtering and projection can run
before any heap-to-Arrow materialization. Use columnar execution for other driving
sources. Conversion is explicit at materialization or terminal boundaries, never
an implicit adapter between arbitrary streaming operators.

DataFusion can expose a heap through a custom provider, but its executor expects
Arrow batches at the source boundary. The proposed execution model postpones that
representation change and can avoid it entirely for simple modifications.

## Initial format policy

A pipeline has one driving source, streaming operators, a sink, a format, and
explicit dependencies on other pipelines or shared states.

| Driving source | Format |
| --- | --- |
| Heap scan | Tuple |
| Index scan fetching heap tuples | Tuple |
| Values, aggregate results, sorted/materialized results | Columnar |

The decision is local to the current pipeline's source. It must not inspect all
transitive dependencies for heap scans: a pipeline reading aggregate results is
columnar even when aggregation consumed heap tuples upstream.

Initial columnar representation: Arrow RecordBatch. The format is an explicit
physical property rather than a hard-coded test for a particular source node's
name. Non-heap tuple sources or index-only sources will need a deliberate format
choice when introduced.

Sketch, not an implemented API:

```text
Pipeline {
    source,
    streaming_operators,
    sink,
    format: Tuple | Columnar,
    dependencies,
}
```

Every streaming operator in a pipeline must support its format. Unsupported
combinations require an explicit boundary or a planning error, not hidden
per-tuple Arrow construction.

## Examples

### Heap modification

```text
Tuple: HeapScan → Filter → Projection → ModifyTable sink
```

Assignments read original tuple values. CTIDs can stay as native `Ctid` values.
The terminal modification sink can consume tuples directly; no Arrow conversion
is required merely because the pipeline ends.

### INSERT VALUES

```text
Columnar: Values → ModifyTable sink
```

The sink constructs heap records from columnar values. This is the reverse
representation boundary and requires a columnar input path for the sink.

### Aggregation

```text
P0 Tuple:    HeapScan → Filter → AggregateSink
                                    ↓ shared aggregate state
P1 Columnar: AggregateSource → Projection → ResultSink
```

The row-capable aggregation sink updates owned state directly. Its source emits
columnar aggregate results. This avoids constructing Arrow arrays for every input
row merely to feed a batch-only aggregate implementation.

### Hash join

```text
P0 Tuple: HeapScan(build) → HashBuildSink
                                  ↓ build state
P1 Tuple: HeapScan(probe) → HashProbe → ResultSink
```

The build dependency transfers access to shared state, not necessarily Arrow
batches. Probe format follows its own driving source; a columnar probe source
would require a columnar HashProbe implementation.

Pipeline format governs data moving between streaming operators. It does not
require hash tables, aggregate accumulators, or other internal state to use that
same storage layout.

## Ownership and CTIDs

- Streaming tuple views may borrow page data under storage's pin, latch, and
  visibility protocol. A pin by itself is not a row lock or MVCC validation.
- Sinks must own any values retained beyond the input borrow. Blocking operations
  must not indefinitely retain all scanned page pins.
- Materialization preserves required CTIDs and their association with rows.
  Native `Ctid` values become the defined six-byte little-endian encoding only
  when an Arrow boundary requires `FixedSizeBinary(6)`.
- A CTID identifies a relation-local physical tuple version, not a permanent row.
  ModifyTable must validate liveness and concurrent modification before mutation.
- Output buffers and shared state need explicit ownership and release rules,
  including cancellation, errors, and spill paths.

## Expression and operator implementation

Tuple pipelines need a row-capable expression evaluator or compiled expressions.
DataFusion column names should resolve to physical input slots during physical
planning, not through name lookup for each row.

Reusing DataFusion's logical optimizer does not imply calling its batch-oriented
physical expression kernels or aggregate accumulators per tuple. Selected
optimizer rules may evaluate constants during planning; that is separate from
heap execution.

Row and columnar implementations must share SQL semantics: types, NULL handling,
casts, overflow behavior, comparisons, and aggregate/window definitions. The
physical planner must reject unsupported function or plan shapes emitted by the
selected logical rules.

## Decisions for the next design session

1. Define physical source/operator/sink interfaces and supported formats.
2. Define boundary kinds: batch materialization, shared-state dependencies, and
   terminal sinks; specify when a boundary actually converts representation.
3. Define tuple-view ownership, pin/latch lifetimes, and sink state ownership.
4. Choose the first executable slice and expression evaluator, including the
   required tuple and columnar ModifyTable input paths.
5. Define scheduling, parallelism, cancellation, memory accounting, and spill
   contracts without conflating these with row-versus-column representation.

No physical execution interfaces or algorithms are implemented by this draft.
