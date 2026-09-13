# Large detect-changes validation

The candidate binary built from production changes through `488c5f8d` has SHA256
`865253b05f151ff110a915e5b1bda96f924f47c77d2dd76b7749f93af97bf0c5`.
The [retained fixture and evidence](evidence/detect-changes-488c5f8d/SHA256SUMS.json)
record the 56 Rust paths reconstructed from the v7.0.0-to-main delta at
`5149d22cd885107fa5d8f047a00d87e1fff86847`.

A complete source archive of that main commit was indexed by the installed
v10.0.0 release binary into an isolated database. All 56 requested paths mapped
to symbols, no dirty publication remained, and resolver generation 5 matched the
candidate. The seed reported its existing ignored frontend dist/public directory
policy as degraded; that qualification remains part of the fixture provenance.
This is a documented equivalent, not the unavailable original multi-repository
graph or a reproduction of its 300-second timeout.

The candidate daemon served repeated `detect-changes --json` requests, passing
one `--files` argument for every retained path, at limits 1000, 1 and 1000. Each
request overlapped three `brain status --json` controls. A fourth limit-1000
request retained its raw timing events before the temporary daemon was stopped.

| Limit | Wall seconds | Planning ms | Graph load ms | Traversal ms | Process sorting ms | CLI bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1000 | 51.476 | 36,963 | 5,746 | 8,205 | 19 | 355,462 |
| 1 | 51.375 | 30,968 | 6,364 | 13,719 | 33 | 3,946 |
| 1000 | 50.260 | 39,979 | 4,191 | 5,710 | 18 | 355,462 |
| 1000, retained trace | 34.997 | 25,063 | 3,600 | 5,973 | 16 | 355,462 |

All requests returned `status: complete`, exact totals of 6,304 affected symbols
and 5,955 affected processes, and 1,950,009 traversal steps independent of display
limit. Risk remained high with `gate_state: risk-flagged`. Display omissions were
reported exactly; no deadline/work truncation or missing-file/read-error
notification occurred. Returned UIDs had no duplicates. Repeated limit-1000
results matched except measured phase timings, and limit 1 was the exact prefix.
All twelve concurrent status reads overlapped their query and succeeded in
0.582–2.141 seconds. Every fixture daemon stopped successfully.

The [retained timing events](evidence/detect-changes-488c5f8d/timing-events.log)
measure MCP symbol sorting independently at 13,615 microseconds for 6,304 symbols,
and actual daemon serialization at 41,440 microseconds for 279,019 compact JSON
bytes. The CLI pretty renderer produced 355,462 bytes. Planning dominates this
fixture; serialization is small. The first three requests ran under shared-host
build/test load; the fourth followed termination of local compilers. None is a
quiet-host throughput claim or a controlled comparison against the historical
failure.

The implementation uses a 2,000,000-step traversal budget and a shared 60-second
cooperative database/engine read deadline, independently of display cardinality.
Native deadline-interruption/recovery, thread isolation and deterministic
work-budget regressions cover the partial/degraded path. This is not a hard
end-to-end deadline: symbol sorting and serialization are measured separately.
No continuation protocol was introduced, so there is no pagination cursor claim.

The instrumented replay also verifies the daemon logging fix: an explicit
`RUST_LOG=info,nestweaver_daemon=debug,nestweaver_mcp=debug` now reaches the existing
log subscriber. Absent or invalid filters retain INFO. Temporary-database daemon
cleanup intentionally removes its ephemeral log directory, so timing events were
copied before shutdown. Normal database log retention is unchanged.
