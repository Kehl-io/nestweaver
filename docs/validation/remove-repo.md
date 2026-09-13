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
