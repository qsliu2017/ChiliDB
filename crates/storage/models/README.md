# Buffer victim and asynchronous-loading model

This is a bounded Stateright model of a **proposed** buffer-pool protocol. The
production buffer pool still performs synchronous I/O under its metadata mutex
and reads replacements into scratch storage. The model is not a claim that async
loading is already implemented, or that the existing Rust code refines this model.

## Run

```sh
cargo test -p chilidb-storage --test buffer_model -- --nocapture
cargo run -p chilidb-storage --example check_buffer_model
```

The example defaults to the four larger configurations below. Pass explicit
`frames pages backends` arguments for a single configuration, for example:

```sh
cargo run -p chilidb-storage --example check_buffer_model -- 2 2 2
```

Stateright is a development dependency only. The model is separate from page I/O,
heap storage, and production replacement policies. Every eligible victim is a
possible choice, rather than modeling only CLOCK. A backend represents one
concurrent database worker/session, not necessarily an operating-system process.
Each backend has at most one outstanding page request or pin.

## Checked bounds

Tests exhaust the reachable graph with breadth-first exploration, without a depth
or state-count cutoff. The safety checks remain active after reachability witnesses
are found. The example prints counts and witness action paths.

| Frames | Pages | Backends | Reachable states |
| --- | --- | --- | --- |
| 1 | 1 | 1 | 27 |
| 1 | 1 | 2 | 261 |
| 1 | 2 | 2 | 1,273 |
| 2 | 2 | 2 | 13,061 |
| 2 | 3 | 2 | 59,296 |
| 2 | 2 | 3 | 209,687 |

Counts are for the current model; changing its state or transitions can change
them. Reachability obligations are enabled only for bounds where their scenarios
can occur.

## Protocol requirements

A victim is reserved before its identity or contents can be reused. Reservation
must exclude existing pins and new acquisitions of the old identity. Dirty data
must be written successfully before being discarded; writeback failure retains
the old page and dirty status.

A stale miss can recheck the mapping and join a newly published load without
reserving another frame. A backend that did reserve a victim must recheck the
target mapping when publishing its candidate. Another backend may have won in
the meantime. The loser releases
its extra reservation and joins the winner rather than issuing another read.
A Loading mapping is published before starting the read, so duplicate requests
find the same in-flight operation.

Loading frames permit no backend byte access; only the loader may fill their
bytes. Submission and completion are separate actions; no completion is enabled before submission. A successful
completion makes the frame Ready and grants pins to registered waiters in one
atomic handoff, before backends can obtain byte access. A failed read removes the
failed mapping, releases the frame, and makes waiters able to retry. Any partially
written bytes are invalid, not data belonging to the old or requested page.

An I/O operation owns its reservation until a terminal completion. Canceling a
waiting backend must not free its loading frame while that operation can still
write into it. Canceling physical I/O, delivering late/duplicate completions, and
generation-token schemes require a separate extension of this protocol.

## What the checks mean

Nine safety properties check:

- Access requires a Ready frame with the requested page and a pin; writers exclude
  other readers and writers.
- Page uniqueness counts physical published frames, not merely unique map keys;
  mappings must correspond exactly to those frames.
- Pin accounting excludes eviction of pinned frames; reservations and waiter
  references must agree with their owners and frame states.
- Failed reads immediately free their frame/mapping and notify waiters; failed
  dirty writeback retains the old dirty page and mapping.
- A competing publisher that loses the recheck releases its extra reserved frame.

Named fields make trace entries explicit, for example
`Request { backend_id: 0, page_id: 1 }` and
`LoadDone { frame_id: 0, success: false }`.

Error recovery is checked as a postcondition: failure releases the load's frame
and mapping rather than leaving a permanently failed cache entry. Reachability
checks must also demonstrate successful retry, duplicate waiters, and the
competing-publisher path. These witnesses establish that the checked scenarios
are reachable.

A successful-retry witness is an **existential** result, not a guarantee that all
schedules complete. No progress is promised if I/O never completes, every retry
fails, or the scheduler indefinitely ignores a runnable backend. An eventual-success
proof would need explicit scheduling/I/O fairness and error assumptions.

## Abstraction and implementation obligations

Each model transition is an atomic protocol step, not necessarily one machine
instruction. Mapping publication/recheck, victim reservation, pin acquisition,
and validity publication need corresponding synchronization in Rust. Victim
eligibility must exclude reservations and in-flight I/O, not just nonzero backend
pin counts. Completion's atomic waiter-to-pin handoff must also be implemented
or explicitly refined; a mere broadcast followed by independent repinning is a different
protocol. Each backend has at most one outstanding request/pin. Reservation and
writeback-owner cancellation are not modeled; only loading-waiter cancellation
and release of completed pins are covered. I/O ownership is represented by the
frame phase, not an independent queue of generation-tagged callbacks.

Bytes, allocator layout, weak memory ordering, pointer provenance, actual OS I/O,
and crash recovery are outside this model. Errors are reported I/O failures, not
panicking callbacks or process crashes. Use Miri and implementation fault/concurrency
tests in addition to model checking.

The checker explores finite configured bounds, not arbitrary pool or backend counts.
Stateright uses state fingerprints for deduplication, so its usual hash-collision
assumption also applies. Do not describe bounded exploration as an unbounded
mathematical proof or as a proof of Rust memory safety.

The current `pages.len()` unused-slot shortcut relies on a dense, never-unassigned
frame prefix. Async reservations, failed-load reclamation, and losing candidates
can create holes: the proposed implementation needs explicit frame allocation
state instead. Merely removing the scratch copy or releasing the existing mutex
around I/O does not implement this protocol.
