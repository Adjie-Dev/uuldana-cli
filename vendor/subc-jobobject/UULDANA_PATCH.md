# Uuldana native completion observer patch

Scope: a native completion observer for the Windows teardown path. The launcher
and the timeouts are unchanged.

Upstream: crates.io `subc-jobobject` **0.2.1**, package checksum
`03430519b384d9e7b9acce23db2eaabd8c1f7ec166cc5fad8adbaafa8076fff9`, VCS revision
`a99da1841ecc33be6c886e67e527dc83047e6977`, path `crates/subc-jobobject`.
`UPSTREAM_HASHES.json`, `.cargo_vcs_info.json` and `Cargo.toml.orig` preserve
provenance. The published package declares **MIT** in both manifests and ships
no standalone license or copyright-holder file; those declarations are retained.
Uuldana does not invent an upstream copyright holder. The original dev dependency
and integration tests remain present. The separate lockfile is for native tests.

## Native boundary and invariants

New native code lives in `src/completion.rs`; the existing audited `src/sys.rs`
adds ownership/configuration glue. Workspace crates continue to forbid unsafe
code. Both modules retain explicit unsafe allowances inside this dependency only.

- Job creation now owns a private I/O completion port, associated before the
  first assignment. The existing create-suspended, assign, discover/retain thread
  and single-resume order, creation flags, argv, environment, cwd, and native
  provider process boundary remain unchanged. Assignment duplicates the process
  handle before it can resume; assignment failure drops that duplicate.
- Every process and port handle has exactly one RAII owner. Observation duplicates
  root handles; it never borrows a handle across owner destruction. Handles are
  non-inherited. Manual `Send` implementations move exclusive process-wide handle
  ownership; no new `Sync` implementation or public raw Job accessor is added.
- The port uses a nonrestrictive `u32::MAX` concurrency limit. A capture caller and
  the wait worker may dequeue on different threads; concurrency 1 could gate the
  second caller while the first remains runnable. The native regression exercises
  that exact thread transfer. All dequeues use a zero timeout and at most 2,048
  messages per poll.
- Only `NEW_PROCESS` notifications supply discovery hints. `EXIT_PROCESS` and
  `ACTIVE_PROCESS_ZERO` messages never prove termination. Notification delivery
  is not a correctness prerequisite: independent Job process-list snapshots find
  live members, and cumulative `TotalProcesses` detects missing identities.
- Live processes are opened with synchronization/query access, and membership is
  verified through the owned handle. Pinned handles prevent PID reuse. Foreign
  membership or acquisition races never count as proof for the queried identity.
- For a trusted nonzero birth PID, `OpenProcess` returning
  `ERROR_INVALID_PARAMETER` establishes that the process object is gone. It is
  independently retired, rather than interpreted as an exited handle. Access
  denied is an error, not a successful exit. Snapshot disappearance alone is not
  counted. Native-gone birth PIDs are deduplicated; repeated gone PID reuse may
  conservatively undercount and produce a bounded diagnostic, never fabricate
  coverage. A recycled foreign PID is unverified and does not count as gone.
- Completion requires zero active membership, cumulative assigned identities
  equal to pinned plus deduplicated native-gone identities, and **every pinned
  process handle signaled**. Native exit value 259 is not a liveness test.
- The SDK process-list buffer uses initialized `usize` storage for pointer
  alignment, an eight-byte header and native-width PID entries on x86/x64. A
  successful query accepts the documented returned prefix even if assigned and
  returned counts differ during membership churn. This supplies acquisition hints,
  never completeness proof. ERROR_MORE_DATA retries; invalid lengths cannot
  succeed. Growth/retries are limited to eight
  queries and 65,536 PID slots; native errors and accounting regression propagate.

## Transport ordering and bounded fallback

Capture precedes the first termination request. The wait worker retains Job and
observer ownership while polling outside the tree mutex. After capture/termination,
a separate direct-reap signal permits asynchronous ConPTY close while native
observation continues. This avoids making ConPTY EOF depend on the final observer
notification, and avoids closing it before descendant handles have been captured.

The existing three-second cleanup allowance reserves 250 ms for the unchanged D1
reader-progress clock, leaving 2,750 ms for native observation from the first
successful termination request. Only native completion starts that reader budget
without a cleanup diagnostic. Backpressure still pauses D1; output progress never
replenishes it. Caller deadlines remain authoritative.

If hints are missing, periodic snapshots and handle polling continue within that
bound. Missing identity coverage, native acquisition/query failure, and unsignaled
handles remain failures at the bound. **EOF cannot replace missing native evidence.**
Actual EOF is recorded only after the reader has admitted its final tail; it is
included in timeout diagnostics to distinguish stream state from native state.
Receiver closure, a failed read/send, a full channel, or ConPTY close alone is not
successful native cleanup. Kill-on-close remains the final fallback. Diagnostics
remain fatal to HostProbe, and the two-second whole-probe boundary is unchanged.

## Primary references

The port is associated for the Job lifetime; queued notifications are consumed
when completion observation starts, rather than by a permanent background
collector. Queue and identity-record storage therefore grow with process churn.
The per-poll work caps and observation deadline bound cleanup work, not total
historical storage. Missed already-gone births may leave an unrecoverable coverage
gap and a conservative bounded diagnostic. Real long-lived provider workloads
remain a separate resource/performance validation risk.

- [Job completion-port messages and delivery limitations](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_associate_completion_port)
- [Process-object/PID lifetime and opening exited processes](https://devblogs.microsoft.com/oldnewthing/20110107-00/?p=11803/)
- [IOCP thread association and concurrency](https://learn.microsoft.com/en-us/windows/win32/fileio/i-o-completion-ports)
- [Process termination and signaling](https://learn.microsoft.com/en-us/windows/win32/procthread/terminating-a-process)
- [Job process-list buffer](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_basic_process_id_list)
