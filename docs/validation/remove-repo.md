# Production-degree remove-repo measurement

Run both complete release-profile ladders at 1,000, 8,700 and 86,800 hub edges
on the isolated runner using `scripts/run-backlog-benchmarks.sh`. Preserve its
hardware, pinned commit/lockfile, load, raw output and resource records.

Use the custom `hub_degree=...: mean` removal timings consistently. Criterion's
headline measurements also include teardown; the incident-degree manual timer
does not. Whole-invocation peak RSS describes the ladder, not any one degree.

The decision criteria were selected before inspecting candidate results:

- At each degree, require `abs(run2-run1)/mean(run1,run2) <= 0.20`; both runs
  must support the same decision. Otherwise repeat under controlled load.
- Report adjacent time ratios and `ln(time ratio)/ln(degree ratio)`. Linear
  reference ratios are 8.7 and approximately 9.98; quadratic references are
  75.69 and approximately 99.54. Three points do not prove asymptotic complexity.
- If the 8,700-to-86,800 time ratio exceeds 20 in both runs, open the parent
  issue's upstream A/B investigation with the NestWeaver maintainer as owner.
  Preserve current transaction behavior until a measured mitigation exists.
- Otherwise record no additional scaling mitigation justified by this fixture.
  This does not establish an absolute latency service objective or refute a
  production incident on a different graph. Chunking (C) requires a separate
  decision about its atomicity tradeoff.

The benchmark's exit status enforces only its smallest-degree sanity bound.
The measurement item also needs the repeat calculation and explicit decision
recorded on its parent backlog item before acceptance.

## Recorded result — 2026-09-13

[Performance job](https://github.com/Kehl-io/nestweaver/actions/runs/34736783556/job/103669515714)
passed on `a771754e7cccc8be96013194813ff3a2c865ef79`. The run's other CI jobs
contained subsequently fixed contract failures; only this successful job supplies
performance acceptance for that recorded source. Later feedback fixes change
authority, eligibility and query code; these timings have not been remeasured
on the follow-up head. The benchmark and removal algorithm remain unchanged.

| Hub degree | Run 1 (seconds) | Run 2 (seconds) | Repeat difference |
| --- | ---: | ---: | ---: |
| 1,000 | 0.083719248 | 0.084052186 | 0.396895% |
| 8,700 | 0.737974507 | 0.737861485 | 0.015316% |
| 86,800 | 10.912034457 | 10.886338553 | 0.235760% |

Adjacent ratios are 8.814873 / 14.786465 for run 1 and 8.778611 / 14.753905 for
run 2. Corresponding exponents are 1.006064 / 1.171035 and 1.004158 / 1.170077.
Whole-ladder peak RSS was 284,568 / 284,504 KiB.

All repeats pass and both high-degree ratios remain below 20. Decision: no
additional scaling mitigation is justified by this fixture; retain current
transaction behavior. Owner: NestWeaver maintainer, Kory Kehl. Reconsider with a
representative production reproduction or a controlled ladder exceeding the
recorded threshold. This does not establish an absolute latency objective or
refute the historical incident.

The runner had four logical CPUs on AMD EPYC 9V74, approximately 16.77 GB memory,
ext4, Rust 1.98.1 and locked lbug 0.19.1. Three consecutive five-second CPU samples
were 1.402%, 0.150% and 0.200% busy before measurement; process snapshots show no
competing compiler or benchmark. Retained load averages include earlier builds.
[Raw evidence and checksums](evidence/backlog-performance-a771754e/SHA256SUMS.json)
include both removal logs, hardware/load snapshots and lockfile source/checksum
provenance. The exact lockfile copy remains in the CI artifact.
