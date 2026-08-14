# Daemon shutdown

How `nestweaver daemon stop` and `nestweaver daemon restart` behave, what they
guarantee, and — importantly — what they cannot guarantee when a process
supervisor is in the picture.

## The short version

- Both `daemon stop` (SIGTERM) and `daemon restart` (gRPC `Shutdown` RPC) run
  the **same drain**, with **listeners still up**. Reads keep being served while
  writes drain.
- An idle daemon exits **immediately**. Nothing about this makes a normal stop
  slower.
- If a write is still in flight when the stop grace expires, `daemon stop`
  **reports and stands down**. It leaves the daemon running and exits non-zero.
  It does **not** SIGKILL.
- To end an in-flight write anyway: `nestweaver daemon stop --force`, or
  `kill -9 <pid>`. Both abandon the write.
- **Under a process supervisor, the supervisor's own timer wins.** Nothing in
  this codebase can prevent that.

## Why nothing escalates automatically

Daemon writes run on `spawn_blocking` threads. Tokio cannot cancel them, so
there is no mechanism in the process to abort an in-flight write — the shutdown
broadcast only stops listeners *accepting*, it cannot preempt work already
running. The drain therefore waits, and says so.

The graph store is **not crash-safe**. In nw-126 a SIGKILLed daemon left a stale
42-byte WAL that made a live 5.6 GB database look absent. That evidence is why
the drain ceiling is a reporting threshold rather than a kill switch.

`daemon stop` used to contradict that: after SIGTERM it polled for the stop
grace and then SIGKILLed regardless. So the command operators actually reach for
performed, automatically and on a timer, exactly the unsafe act that was
deliberately refused elsewhere. That escalation is gone. Killing a daemon
mid-write is a decision with a known cost, so it is now an explicit operator act.

## What `daemon stop` prints

While waiting, every 60s:

```
  still draining after 120s — the daemon is still serving reads; check its log
  for the in-flight count
```

If the grace expires with a write still running:

```
Daemon (PID 4242) is STILL DRAINING after 690s and was NOT stopped.
It is still running and still serving reads — an in-flight write cannot be
aborted (`spawn_blocking` work is not cancellable), so it has to finish.
Nothing was killed: the graph store is not crash-safe, and a SIGKILL here is
what left a stale WAL making a live database look absent (nw-126).
Options:
  - wait: the daemon exits on its own when the write completes (no re-run
    needed; the SIGTERM drain is already in progress)
  - end it now, abandoning that write: `nestweaver daemon stop --force`, or
    `kill -9 4242`
Check the daemon log for the in-flight write count.
```

Exit status is non-zero: the operator asked for the daemon to stop and it has
not stopped.

## What is still served during a drain

Most reads. Reads do not take the write gate — which is why killing read service
during a write drain bought nothing and was pure cost. The exceptions are
`embed` and `plan_embed`: they take the same write gate and block until the
in-flight write completes.

## Timeouts

| Knob | Default | What it is |
|---|---|---|
| `NESTWEAVER_DRAIN_TIMEOUT_SECS` | 660 | Drain ceiling. With a write in flight this is a **reporting** threshold, not a deadline. With only an index job in flight it **is** a deadline (see CLAUDE.md for why). |
| `NESTWEAVER_STOP_GRACE_SECS` | ceiling + 30 (690) | How long `daemon stop` waits before reporting and standing down. **Not** a kill deadline. |
| `daemon stop --force` | 10s, fixed | SIGTERM, brief wait for a clean exit, then SIGKILL. Ignores the two variables above — an operator using `--force` should not have to wait 11 minutes first. |

## Process supervisors — read this before assuming a guarantee

Everything above describes what **this process** does with SIGTERM. A supervisor
that SIGKILLs on its own timer still does so, at its own deadline, and the
daemon cannot extend or refuse it. Concretely:

### Linux, `daemon start` — **fully fixed**

This repo ships **no systemd unit** and no service-installer command. On Linux
`nestweaver daemon start` spawns a bare detached process supervised by nothing.
There is no external timer, so the drain really is unbounded and `daemon stop`
really does refuse to kill. This is the case the change fixes completely.

### Linux, your own systemd unit — **exposed unless you configure it**

If you wrote a unit yourself, systemd's `TimeoutStopSec` defaults to **90
seconds**, after which it sends SIGKILL. That is well inside a normal index, so
`systemctl stop` will crash the daemon mid-write regardless of anything here.
Set it to at least the drain ceiling plus a buffer:

```ini
[Service]
ExecStart=/usr/local/bin/nestweaver daemon --db /var/lib/nestweaver/brain.lbug run
# Must exceed NESTWEAVER_DRAIN_TIMEOUT_SECS (default 660) or systemd SIGKILLs
# a draining daemon mid-write. `infinity` removes the deadline entirely at the
# cost of a stop that can hang; 690s matches `nestweaver daemon stop`.
TimeoutStopSec=690
KillSignal=SIGTERM
Restart=on-failure
```

Even so, this is a **hard deadline** systemd enforces. It moves the exposure
window; it does not remove it.

### macOS, launchd — **was the tightest exposure of all; now partially fixed**

`daemon start` on macOS installs a launch agent. The generated plist set **no
`ExitTimeOut` at all**, so launchd applied its **20-second** default. Against a
660s drain ceiling that means macOS has been SIGKILLing the daemon **20 seconds
into a drain**, on launchd's timer, regardless of anything `daemon stop` or the
SIGTERM handler did — a far tighter guillotine than the systemd default, present
in every macOS install, and not previously known.

The plist now sets `ExitTimeOut` to the drain ceiling + 30. **Re-run
`nestweaver daemon start` to regenerate an existing plist** — an
already-installed agent keeps the old 20s behaviour until regenerated.

> **Unverified.** `crates/nestweaver-daemon/src/lib.rs` declares the module as
> `#[cfg(target_os = "macos")] pub mod launchd;`, so none of this compiles on
> Linux: `cargo check --all-targets`, `cargo clippy` and
> `cargo test -p nestweaver-daemon` all pass on Linux without ever type-checking
> the `ExitTimeOut` change, and `plist_sets_exit_timeout_from_the_drain_ceiling`
> does not appear in the Linux test list at all. Neither the code nor its test
> has been executed anywhere yet. The change is correct by inspection only and
> needs CI's Cold Metal (macOS) job before this section describes observed
> behaviour.

Still a hard deadline: a write running at `ExitTimeOut` is SIGKILLed by launchd.
launchd treats `0` as infinity, but a job that can refuse to die is a wedged
logout, so it is not used.

### Docker / Compose — **partially fixed**

`docker-compose.yml` previously set `stop_grace_period: 30s`, the tightest
deadline in the repo, against a 660s drain ceiling. It is now 690s. The
`Dockerfile` sets no `STOPSIGNAL`, so Docker's default SIGTERM is correct and
reaches the drain. If you run the image outside the shipped compose file, set
`--stop-timeout` / `stop_grace_period` yourself; Docker's default is **10
seconds**.

### Honest summary

**The interactive case is fixed. The supervised case remains exposed until the
graph store is crash-safe.** Widening supervisor timeouts moves the deadline out
to where a legitimate drain usually fits; it does not make the kill safe, and a
drain longer than the configured timeout still ends in the nw-126 crash. The
real fix is crash safety in the store, not a larger number here.
