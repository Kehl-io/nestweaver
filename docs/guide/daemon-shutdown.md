# Daemon shutdown

How `nestweaver daemon stop` and `nestweaver daemon restart` behave, what they
guarantee, and — importantly — what they cannot guarantee when a process
supervisor is in the picture.

## The short version

- Both `daemon stop` (SIGTERM) and `daemon restart` (gRPC `Shutdown` RPC) run
  the **same drain**, with **listeners still up**. Reads keep being served while
  writes drain — though an individual read can stall for seconds while a write
  commits, which is pre-existing store behaviour and not caused by the drain.
- **New writes are refused** (`UNAVAILABLE`) once shutdown starts, so new work
  cannot extend the drain. A genuinely running write or index job is waited on
  without a deadline, including work that may be wedged. Reads are not gated.
  gRPC write RPCs and web admin mutations both refuse new work (gRPC
  `UNAVAILABLE` or HTTP 503); admitted mutations remain counted by the drain.
- An idle daemon exits **immediately**, including when jobs are only queued or
  deferred and `indexing_active` is stale. Queued jobs persist for the next
  start; the flag alone is not evidence of a running job.
- If a write is still in flight when the stop grace expires, `daemon stop`
  **reports and stands down**. It leaves the daemon running and exits non-zero.
  It does **not** SIGKILL.
- To end an in-flight write anyway: `nestweaver daemon stop --force`, or
  `kill -9 <pid>`. Both abandon the write.
- **Under a process supervisor, the supervisor's own timer wins.** Nothing in
  this codebase can prevent that.
- **Autostart's two legacy-daemon cleanup paths still SIGKILL on a 2s timer.**
  See "What still escalates" below.

## Why `daemon stop` does not escalate automatically

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

## What still escalates

Two paths in the **client's autostart**, both untouched by this change, do
SIGTERM → wait 2s → SIGKILL when they find a *legacy* daemon holding the DB
write lock:

- `nestweaver_daemon::lifecycle::stop_legacy_hash_daemon` — daemons whose
  instance ID predates the SHA-256 upgrade.
- the `$TMPDIR`-path cleanup in `nestweaver-client/src/autostart.rs` — daemons
  at the pre-v0.26.2 socket location.

Two seconds is far inside a normal drain, so either can SIGKILL a daemon that is
draining exactly as designed. Neither is reachable from `daemon stop`; both are
migration paths that run when a *new* daemon is starting. They are listed here
because "nothing escalates automatically" would otherwise be an overclaim.

## What `daemon stop` prints

While waiting, every 60s:

```
  still draining after 120s — new writes are being refused; check the daemon
  log for what it is waiting on
```

If the grace expires with work still in flight and the daemon still reachable:

```
Daemon (PID 4242) is STILL DRAINING after 690s and was NOT stopped.
It is still running and still answering on its socket — reads are still served,
though they can stall for seconds while a write commits. Work that cannot be
aborted (`spawn_blocking` is not cancellable) has to finish.
Nothing was killed: the graph store is not crash-safe, and a SIGKILL here is
what left a stale WAL making a live database look absent (nw-126).
Options:
  - wait: the daemon exits on its own when the work completes (no re-run
    needed; the SIGTERM drain is already in progress, and new writes are being
    refused)
  - end it now, abandoning that work: `nestweaver daemon stop --force`, or
    `kill -9 4242`
Check the daemon log for what it is waiting on.
```

The command **re-probes the socket** immediately before printing, and says so
honestly when the answer is bad. A stuck `indexing_active` flag
(`active_writes == 0`, no worker job in flight) no longer holds the drain
(nw-830). If the socket still does not answer, the message reports only that
observation and does not name a cause:

```
It is still running, but its socket did not answer just now, so reads may be
DOWN. The daemon may be mid-exit, its socket may already have been cleaned up,
or this process may be at its file-descriptor limit; this probe cannot tell
that apart — check the daemon log before concluding.
```

Exit status is non-zero in both cases: the operator asked for the daemon to stop
and it has not stopped.

The daemon's own log separately reports active writes, genuinely in-flight index
jobs and the queued-job hint on a 30-second cadence while waiting. An absent
hint is not a zero count. A queued backlog does not hold the drain once the
actual write and in-flight-job counters reach zero.

## What is still served during a drain

Most reads. Reads do not take the write gate — which is why killing read service
during a write drain bought nothing and was pure cost. `plan_embed` takes that
gate and can block until the in-flight write completes. `embed` is a write RPC
and is refused with `UNAVAILABLE` once shutdown starts.

"Served" is not "fast". A read can stall for **seconds** while a write commits —
measured at up to 15s under load. This is store-level contention, not something
the drain introduces: a control run indexing the same repo with **no shutdown at
all** produced the same stalls at the same magnitude. Do not read "reads keep
working" as a latency guarantee.

New write RPCs during a drain are refused outright with `UNAVAILABLE`; new
arrivals cannot extend the wait, but existing work still has to finish.

## Timeouts

| Knob | Default | What it is |
|---|---|---|
| `NESTWEAVER_DRAIN_TIMEOUT_SECS` | 660 | Drain ceiling. With a write in flight — or a genuinely running index job (the drain reads the worker pool's own in-flight counter) — this is a **reporting** threshold, not a deadline. With only a stuck `indexing_active` flag (no job in flight, no write) the drain does not wait at all and exits promptly (nw-830). |
| `NESTWEAVER_STOP_GRACE_SECS` | ceiling + 30 (690) | How long `daemon stop` waits before reporting and standing down. **Not** a kill deadline, and not a limit for a supervisor's stop policy. |
| `daemon stop --force` | 10s, fixed | SIGTERM, brief wait for a clean exit, then SIGKILL. Ignores the two variables above — an operator using `--force` should not have to wait 11 minutes first. |

## Process supervisors — read this before assuming a guarantee

Everything above describes what **this process** does with SIGTERM. A supervisor
that SIGKILLs on its own timer still does so, at its own deadline, and the
daemon cannot extend or refuse it. Concretely:

Daemon startup also records lifecycle ownership. A process forked by
`nestweaver daemon start` is NestWeaver-managed and may be replaced
automatically after a client/server version mismatch. A foreground,
systemd-owned, launchd-owned, or older daemon with unknown provenance is never
shut down and replaced by a detached process; the client fails before shutdown
and names the supervisor/manual restart commands instead.

### Linux, `daemon start` — **fully fixed**

This repo installs **no systemd unit** and has no service-installer command. On
Linux `nestweaver daemon start` spawns a bare detached process supervised by nothing.
There is no external timer, so the drain really is unbounded and `daemon stop`
really does refuse to kill. This is the case the change fixes completely.

### Linux, your own systemd unit — wait for genuine work

A finite systemd stop timeout can kill a daemon that is correctly draining.
Neither the daemon's 660 s reporting threshold nor the CLI's 690 s observation
grace bounds a genuine write. Use an unbounded stop policy rather than adding a
buffer to either number:

```ini
[Service]
ExecStart=/usr/local/bin/nestweaver daemon --db /var/lib/nestweaver/brain.lbug run
TimeoutStopSec=infinity
KillSignal=SIGTERM
KillMode=mixed
SendSIGKILL=no
Restart=no
```

Keep the daemon in the foreground with `daemon run`, not `daemon start`.
With no `ExecStop` helper, systemd sends SIGTERM to its tracked main process.
`KillMode=mixed` leaves its child work to the daemon during that drain;
`SendSIGKILL=no` disables subsequent automatic escalation. The full
[server-unit example](../server-mode.md#runtime-directory-and-systemd) also sets
the service account's HOME and directory ownership explicitly.

`systemctl stop` can remain deactivating indefinitely while a genuine write or
index job runs. This is deliberate: inspect the live PID/socket and periodic
drain reports instead of forcing a timing ceiling. Jobs that are merely queued
or deferred persist and do not delay an idle stop. A longer finite timeout such
as 720 s only postpones the same unsafe kill; it does not prove that work will
have finished.

### Stopping the wrong daemon — **the supervisor does not own the lock**

`systemctl stop nestweaver` stops *the unit*. It does not stop "the daemon
serving this database", and those are not always the same process.

Every CLI command auto-starts a daemon on first use. So if the unit is stopped
when an index runs, the index starts its **own ad-hoc daemon**, unsupervised and
outside systemd's control. A later `systemctl stop` then reports success having
stopped nothing, the ad-hoc daemon keeps the write lock, and the next write —
typically `embed` — blocks against a daemon the operator does not know exists.

Use the CLI, which resolves the owner rather than the supervisor:

```sh
nestweaver daemon --db /var/lib/nestweaver/brain.lbug stop
```

`daemon stop` checks the launchd job (macOS) *and* falls through to the
pidfile/socket, so it only reports success once nothing is left serving the
database. That fall-through exists precisely for this case.

For a scripted refresh, start the supervised service **before** the first
command that touches the database, so no ad-hoc daemon is ever created:

```sh
systemctl start nestweaver          # or: nestweaver daemon start --config <toml>
nestweaver brain refresh ... --config <toml>
```

> **Ask `daemon status` which kind you have.** On macOS the text output
> distinguishes `Daemon is running (launchd agent)` from a bare PID. On Linux
> the text output prints `Daemon is running (PID <n>)` followed by a
> `Supervision:` line, and `nestweaver daemon --db <db> status --json` reports
> the same value: a daemon owned by a **user** service (`systemctl --user`)
> reads `systemd-user`, an unsupervised one reads `ad-hoc`, and a **system**
> unit like the example above reads `unknown/unverifiable` (see "Observe the
> selected runtime before recovery" below). Either way, `systemctl is-active` and
> `nestweaver daemon status` answer **different questions**: whether the unit
> is up, versus whether something is serving the database.

### macOS, launchd — **was the tightest exposure of all; now partially fixed**

`daemon start` on macOS installs a launch agent. The generated plist set **no
`ExitTimeOut` at all**, so launchd applied its **20-second** default. Against a
660s drain ceiling that means macOS has been SIGKILLing the daemon **20 seconds
into a drain**, on launchd's timer, regardless of anything `daemon stop` or the
SIGTERM handler did — a far tighter guillotine than the systemd default, present
in every macOS install, and not previously known.

The plist now sets `ExitTimeOut` to the drain ceiling + 60 — strictly later than `daemon stop`'s own ceiling + 30 window, so the CLI can never report "still running" about a process launchd has just killed. When `NESTWEAVER_DRAIN_TIMEOUT_SECS` is set at install time it is baked into the plist's `EnvironmentVariables`, because launchd jobs inherit no shell environment and the daemon would otherwise run a different ceiling from the one its own kill deadline was derived from. **Re-run
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
deadline in the repo, against a 660s drain ceiling. It is now 720s — ceiling +
60, the same budget as the launchd `ExitTimeOut` and deliberately later than
`daemon stop`'s ceiling + 30 window. The
`Dockerfile` sets no `STOPSIGNAL`, so Docker's default SIGTERM is correct and
reaches the drain. If you run the image outside the shipped compose file, set
`--stop-timeout` / `stop_grace_period` yourself; Docker's default is **10
seconds**.

### Honest summary

Interactive shutdown and the recommended systemd policy wait for genuine work
without an automatic kill deadline. The finite launchd and Docker policies
described above still expose a sufficiently long drain to a forced kill.
Widening those timeouts does not make the kill safe. None of these policies
makes the graph store crash-safe or protects a write from an explicit forced
kill; that requires crash safety in the store.

### Observe the selected runtime before recovery

`nestweaver daemon --db /path/to/brain.lbug status --json` reports the selected
instance, socket, observed ownership evidence and supervision. A held database
lock with a missing socket is `running_but_unreachable`; an unreadable lock is
`unknown`, not proof that the database is stopped. Neither status path starts a
daemon or removes runtime files. `daemon status` exits **69** (`EX_UNAVAILABLE`)
in those two states, so a script can tell "stopped" (exit 0) from "stuck and
needs an operator"; exit 1 means the daemon is not running and the database
cannot be used. Identify the lock holder before signaling it:
a direct indexing process can own the same write lock as a daemon.

Linux supervision is `systemd-user` only when the live process belongs to a
user-manager service cgroup, with its process generation unchanged across the
probe. `ad-hoc` means the observed cgroup has no service owner. Unreadable process
state, system services and unsupported platforms report `unknown/unverifiable`.
Unit-file existence is not evidence. A systemd-owned process may restart until
its owning unit is stopped; `daemon stop` addresses the selected database's
verified daemon, while `systemctl --user stop UNIT` addresses that unit.

Use `daemon inspect-runtime /exact/runtime/root/0123abcd` to report a single
entry's contents, age and missing ownership proof without changing it. After
inspection, `daemon prune-runtime /exact/runtime/root/0123abcd --database
/path/to/brain.lbug` can retire an empty entry or one containing only an empty
spawnlock. It requires an existing regular database with an exact matching
current or known legacy hash, holds spawn admission
and exclusive database authority through removal, and refuses any PID, socket,
other content, held lock, or ambiguity. A legacy hash whose database identity
cannot be reconstructed remains spared, as does an entry whose database no
longer exists. Runtime cleanup never creates a database. There is no age-based or recursive
runtime-root deletion fallback.

### Watcher controller lifetime

Code and vault watchers report one registration containing kind, target,
controller PID, session id, start time and age through health and brain status.
Registration age is distinct from `write_holder_seconds`: an idle watcher does
not hold the write gate, so a zero write-holder age while idle is correct.
Controllers poll their registration with bounded RPC waits. Replacement, stop
or disconnect terminates the displaced controller; Ctrl-C stops only its own
session, so an old controller cannot stop its replacement. A daemon restart
creates a new session sequence. Existing explicit `watch-stop` remains the
administrator operation that stops the current watcher.
