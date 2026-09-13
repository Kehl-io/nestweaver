# Release enforcement and archive verification

Main is protected by the policy in `.github/main-ruleset.json`: PRs, up-to-date
`Required CI` from GitHub Actions (app 15368), no bypass actors, and no force
push or deletion. The applied ruleset is 23123306. The single-owner repository
does not require an impossible second approval; CODEOWNERS requests review but
is not itself an enforced approval policy. Maintainer review of workflow edits
remains part of the trust boundary.

On 2026-09-13 the applied policy was read back independently. Release PR #385,
authored by github-actions at `d85e160399b6c8316df12cecaa1cb009910b269c`, had no
checks and changed from CLEAN to BLOCKED after enforcement. No merge was
attempted. This proves the actual automation PR cannot pass the missing-check
gate under the applied rules. It does not claim an adversarial workflow edit
cannot manufacture a check of the same name.

The canary verifier now checks the CI/PR policy without requiring CODEOWNER
approval. Its temporary PR continues to prove an absent or approval-blocked
workflow cannot merge, and cleanup remains exact-branch-only. Maintainers can
run release `dry-run` from a candidate branch to validate this policy before
merging it. Mutating recovery modes remain bound to the current main policy;
dry-run never publishes a release or package. Candidate artifact evidence and
the separately observed current-main canary base are both recorded.

The release matrix builds on native Ubuntu 22.04 for both GNU architectures.
The extracted consumer archive now passes `verify-linux-archive.py`, which
checks the executable and both bundled GCC runtime libraries, refuses missing
or escaping files and unreadable version requirements, and rejects glibc above
2.35. A disposable ELF with an actual version-needs string changed to
`GLIBC_9.99` supplies the negative control. The extracted binary must also run
version, capabilities, isolated indexing and a search that finds its fixture.
The final archive, rather than the build-tree binary alone, supplies evidence.

Local validation of the published v10.0.0 x86 archive found maxima of GLIBC_2.34
for nestweaver/libstdc++ and GLIBC_2.35 for libgcc; the injected GLIBC_9.99 was
rejected. This is verifier validation on that published archive; the candidate
matrix provides evidence for the eventual PR's source and both architectures.

Official references: [GitHub workflow triggers](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow)
and [ruleset API](https://docs.github.com/en/rest/repos/rules).
