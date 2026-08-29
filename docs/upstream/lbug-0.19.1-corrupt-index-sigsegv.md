# lbug 0.19.1 — SIGSEGV opening a database with a corrupted on-disk index

**Status:** written up for filing upstream. Not filed from this repository.
**Affected version:** `lbug = "=0.19.1"`, built from source
(`LBUG_BUILD_FROM_SOURCE=1`), macOS arm64 (Darwin 25.6, Apple M-series).

## Summary

`lbug::Database::new` dereferences unvalidated length/offset fields read from
the database file while deserialising `PrimaryKeyIndex` / `DiskArray`
structures. When those bytes are corrupt the constructor faults before it can
throw, so the C++ → Rust error path is never entered: the process dies with
SIGSEGV rather than returning `Err`.

The practical consequence for an embedder is that **there is no way to open a
database defensively.** Wrapping the constructor in a `match` cannot help,
because control never returns.

## Reproduction

1. Create an ordinary database and write enough rows that the on-disk index has
   real structure (a few thousand nodes is plenty).
2. Close it cleanly.
3. Overwrite the bytes between 40% and 60% of the main database file with
   `0xFF`.

```python
import os
p = "scratch.lbug"
sz = os.path.getsize(p)
with open(p, "r+b") as f:
    start, end = int(sz * 0.4), int(sz * 0.6)
    f.seek(start)
    f.write(bytes([0xFF]) * (end - start))
```

4. Open it: `lbug::Database::new(path, config)`.

Result: SIGSEGV, deterministic, on every attempt. Reproduced twice with a
symbolicated crash report each time. Also reproduces with a read-only
`SystemConfig` and with `max_num_threads(1)`, so it is not the concurrency
fault tracked separately.

## Stack (symbolicated, top frame first)

```
lbug::storage::DiskArrayInternal::DiskArrayInternal(...)
lbug::storage::HashIndex<...>::HashIndex(...)
lbug::storage::PrimaryKeyIndex::initOverflowAndSubIndices(...)
lbug::storage::PrimaryKeyIndex::load(...)
lbug::storage::IndexHolder::load(...)
lbug::storage::NodeTable::deserialize(...)
lbug::storage::StorageManager::deserialize(...)
lbug::storage::Checkpointer::readCheckpoint(...)
lbug::storage::WALReplayer::replay(...)
lbug::storage::StorageManager::recover(...)
lbug::main::Database::initMembers(...)
```

Every frame is inside the engine; there is no embedder frame below the FFI
boundary.

## Why the corruption offset matters

The corrupted range is 40%–60% of the file — inside the on-disk index region,
well past the header. A header or magic-byte validation would accept this input
unchanged. Any fix (and any embedder-side guard) that only inspects the file
prefix will pass this reproduction while leaving the fault intact.

## Suggested fix

Bounds-check the deserialised header fields in `DiskArrayInternal` and
`PrimaryKeyIndex::load` against the actual file length and the containing
page/frame extents, and raise the engine's own exception type on violation, so
the failure arrives through `Database::new`'s existing error channel. The
`Checkpointer::readCheckpoint` path is the highest-value place to validate,
since `StorageManager::recover` runs it on every open.

A cheaper partial mitigation, if full validation is too invasive: checksum each
index page at write time and verify on load. That still turns an
unrecoverable process death into a returnable error.

## What this workspace does in the meantime

`crates/nestweaver-store/src/open_crash_guard.rs` installs a SIGSEGV/SIGBUS
handler for the duration of an open. It does NOT attempt to survive the fault —
unwinding a C++ frame from a signal handler across the FFI boundary is
undefined behaviour, and would leave the buffer manager in an unknown state.
It writes a preformatted diagnostic naming the database and a recovery path,
then `_exit(1)`, so the caller sees a normal failure instead of exit 139 with
no output. The guard is worth keeping even after an upstream fix, because it
also covers whatever the next unvalidated field turns out to be.
