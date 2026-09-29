# LadybugDB 0.20.4 fixtures

Small databases written by LadybugDB **0.20.4**, the storage engine before the
0.21 string-hash change. The storage-engine cutover tests use them to show, on
real old-engine files, that:

- this engine's primary-key lookup of a non-ASCII key (`repo:default:café`)
  misses while a scan finds the row, which is the hazard the cutover guards
  against;
- NestWeaver refuses such a file before the engine opens it, and changes no
  byte of it;
- `backup save` and `publication rebuild` read it only by scans;
- a file with a write still in its log is refused before anything replays it.

| File | Contents |
| --- | --- |
| `pre-cutover.lbug.zst` | NestWeaver schema, an identity, a repository and symbols with non-ASCII keys; checkpointed |
| `pre-cutover-wal.lbug.zst`, `pre-cutover-wal.lbug.wal.zst` | The same, plus one committed write left in the log, as a crash leaves it |

## Regenerating

```sh
testdata/lbug-0.20.4/regenerate.sh            # replace the fixtures
testdata/lbug-0.20.4/regenerate.sh --out DIR  # dry run into DIR
```

The script builds `generator/` (a standalone crate, excluded from the
workspace so CI never compiles 0.20.4), runs it, checks the output with the
current engine, and only then compresses it into place. Building 0.20.4 from
source takes several minutes and a few GB of disk.

Run it on a signed-`char` host (macOS on arm64, or x86-64 Linux). The 0.21
hash change affects bytes at or above 0x80 only where `char` is signed; on an
unsigned-`char` host (e.g. aarch64 Linux) the old engine hashes non-ASCII keys
the new way, and the primary-key miss the fixture exists to show would not
reproduce (the script's check step then fails rather than writing it).

## Why the pin matters

Without `LBUG_VERSION=0.20.4` the `lbug` crate's build script links the
**latest** release library, so the "old" fixture would silently be written in
the new format and every test built on it would pass for the wrong reason. It
also caches a prebuilt library inside `~/.cargo/registry` unless
`LBUG_BUILD_FROM_SOURCE=1`. The script and `generator/.cargo/config.toml` both
set these, and the generator refuses to finish unless the linked engine
reports version 0.20.x and its output carries no engine-format marker. The
script's second step then requires the primary-key miss to reproduce under
the current engine before any fixture is replaced.
