# Agent Instructions

## Choreographr Coordination Platform (`choreo-content`)

The `choreo-content` crate implements the feature-gated `content` tool group (the
Choreographr Coordination Platform blockchain content registry). The group is
compiled only behind the daemon's `content` cargo feature (off by default;
enable with `--features content`) — a plain build contains no `coord`-era
group at all, and persisted sessions carrying the stale pre-rename `coord`
group name silently ignore it. It owns a
tokio **sidecar runtime** used **only** to drive `subxt` for signed chain
writes; the daemon itself stays thread-only and calls the crate's blocking
`execute_*` functions. IPFS (`ureq`) and the indexer (`tungstenite`) are
synchronous and never touch the sidecar.

The Polkadot account credential (`ServiceCredential::Substrate`) lives in
`choreo-keystore`; the TUI imports a Polkadot-JS keystore export client-side and
sends it over the existing `AddCredential` path.

**Temporary credential plumbing**: the content write tools currently receive the
daemon's single Substrate credential through the `Tool` trait's one
`x_credentials` slot (see `// TEMPORARY` comments in `requests.rs` and
`sessions.rs`). This single-slot reuse is a stopgap until a proper
tool→keystore credential-access system replaces it; do not rely on it
remaining as the permanent mechanism.

## Refactoring

Always try to refactor when implementing new features. Look for opportunities to improve code structure, reduce duplication, and simplify existing code alongside any additions.

## Documentation

When making changes, ensure [ARCHITECTURE.md](./ARCHITECTURE.md) and [README.md](./README.md) are kept up to date. If a change affects the architectural decisions, module structure, data flow, or any other documented aspect, update the files accordingly.

Every non-trivial change (new features, fixes, refactors, dependency updates, behavior changes — anything a reviewer would mention in a commit summary) must also get an entry in [CHANGELOG.md](./CHANGELOG.md) under the `## [Unreleased]` section, using the Keep a Changelog categories (`Added` / `Changed` / `Fixed` / etc.). Only trivial changes (typo fixes, comment-only edits, test-only tweaks) may skip it. Release tooling promotes that section at tag time — see [RELEASE.md](./RELEASE.md).

## Test Discipline

- **Unit tests** (in <code>src/</code> <code>#[cfg(test)]</code> modules) must never use time-based waits (`sleep`, `delay_for`, etc.). Use deterministic patterns only.
- **Integration tests** (tests that bind network sockets, spawn external processes, use `UnixStream::pair()` to exercise the full handler pipeline, or perform filesystem I/O exercising the system boundary) belong in crate-level `tests/` directories, not in `src/`.
- Integration tests are marked <code>#[ignore]</code>. Use the nextest aliases defined in <code>.cargo/config.toml</code>: <code>cargo test-fast</code> (unit tests), <code>cargo test-integration</code> (the <code>#[ignore]</code> suite), and <code>cargo test-all</code> (everything in one pass). Plain <code>cargo test</code> runs libtest (serialized) and is only a fallback when nextest is unavailable.

## Task Execution

When implementing a list of code changes across multiple files, delegate each task to a subagent and run them in series (one at a time), not in parallel. This avoids filesystem conflicts from concurrent edits to overlapping files and keeps each subagent's context focused. Subagents should verify their work by running `cargo nextest run -p <crates>` on only the crates they modified. (The `cargo test-*` aliases bake in `--workspace` and reject `-p`, so call nextest directly when targeting specific crates.)

## Dependency Management

Always use the latest stable version of crates where possible. When adding or upgrading a dependency:

1. Use the latest stable semver-compatible release for each crate (check `cargo search <name> --limit 1` for the current version).
2. If a dependency is locked to an older version upstream, accept the duplication rather than patching — upstream issues should resolve naturally over time.
3. If a dependency is used by two or more workspace members, declare it in `[workspace.dependencies]` and reference it with `dep.workspace = true` in member crates. This is not optional — when adding a crate-level dependency that already exists (or is being introduced simultaneously) in another workspace member, promote it to the workspace and update both crates in the same change.

## Testing New Code

Always write unit and/or integration tests for any new code added to the codebase. Unit tests belong in `src/` `#[cfg(test)]` modules; integration tests belong in crate-level `tests/` directories. Follow the conventions in the **Test Discipline** section above.

## Error Handling

Never use `expect()`, `unwrap()`, or `panic!()` in production code. These create crash surfaces that can take down the daemon. Follow these rules:

1. **Library crates** — define structured error types with `thiserror` and propagate errors with `?`.
2. **Binary crates** — use `anyhow::Context` / `.context()` to attach meaningful context to errors at key boundaries, then propagate with `?`.
3. **Infallible operations** — if an operation truly cannot fail, use `unwrap_or_default()` or `unwrap_or(fallback)` rather than bare `unwrap()`.
4. **Mutex poisoning** — use `.lock().unwrap_or_else(|e| e.into_inner())` to recover from poisoned mutexes instead of panicking.
5. **`unwrap()`/`expect()`/`panic!()` are permitted only in `#[cfg(test)]` modules and `tests/` integration test files.**

## Logging

All crates in the workspace (`choreographr`, `choreo-client-core`, `choreo-keystore`, `choreo-im`, `choreo-gui`, `choreo-markdown`, `choreo-proto`, `choreo-tui`) must log extensively using the `tracing` crate. Every module should emit `tracing` events (`info!`, `warn!`, `error!`, `debug!`, `trace!`) at appropriate levels to provide observability into key operations, state transitions, and error conditions.

In the `choreo-tui` crate specifically, do not use `eprintln!` for diagnostics — output goes to the per-process log file `$TMPDIR/choreo-tui-<pid>.log` (`std::env::temp_dir()`, so it works on Termux/Android too). A failure to create that log file must degrade to no logging, never abort the TUI.

## Thread Communication

Do not share mutable state between threads. Use message passing (`mpsc` channels) for all cross-thread communication. Shared-state patterns (`Arc<RwLock<…>>`, `Arc<Mutex<…>>`) should be avoided in favor of channel-based designs.

Five sanctioned exceptions, each single-purpose, lock-free or minimally scoped, and documented in code and in ARCHITECTURE.md:

1. **Cooperative cancellation flags** (`Arc<AtomicBool>`, e.g. `ToolContext.cancelled`). A blocking tool call cannot be interrupted by a channel message, so a tiny lock-free flag is used as a best-effort stop hint for work that consults it. Keep such flags single-bit (carry no data), document each use in code, and route all control flow — results, cancellation events, kills, streaming — over channels.
2. **The Noise transport state** (`choreo-transport`'s `Arc<Mutex<TransportState>>` on `NoiseStream`, plus its `Arc<AtomicBool>` single-writer guard). An encrypted duplex stream is cloned via `try_clone` across the reader and writer threads of one connection, which must interleave encrypt/decrypt against the same snow `TransportState` — so the state has to be shared, not channeled. The lock is held only per chunk, never across blocking socket I/O (that scope is what prevents the bidirectional large-message deadlock), and the guard is a single-bit flag in the spirit of exception 1. The full rationale lives in ARCHITECTURE.md's `noise.rs` module row.
3. **The daemon's live-connection counter** (`choreo-daemon`'s `Arc<AtomicUsize>` on `server/lifecycle.rs`, held by the RAII `ConnectionSlot`). The concurrent-connection cap (`MAX_CONCURRENT_CONNECTIONS`) is enforced atomically across the two accept paths (Unix main thread + TCP accept thread) and decremented from every connection thread's exit — a channel cannot express that without a dedicated accounting thread, so a single lock-free counter is shared instead. It carries no protocol data: a bookkeeping count whose only role is to bound resource accumulation, and the RAII slot releases it on panic too. The rationale lives in ARCHITECTURE.md's `server/lifecycle.rs` module row.
4. **The provider catalog `ArcSwap`** (`choreo-ai-protocols`'s `PROVIDER_CATALOG`, a `LazyLock<ArcSwap<Vec<ProviderEntry>>>`). Readers are lock-free and the swap is an atomic `store()`, but there is a strict **single-writer invariant** — only the daemon command loop calls `replace_catalog` (after a catalog refresh, overlay change, or `/refresh-models`); every change *request* still travels by channel, and only the atomic store mutates the catalog. It carries no per-message data: a process-wide immutable snapshot that is atomically replaced wholesale. The rationale lives in ARCHITECTURE.md's `catalog/` module row.
5. **The Windows Job Object kill-switch** (`choreo-daemon`'s `Arc<ChildJob>` on `tools/shell_util.rs`, plus the `ProcessIsAlive` process-handle copy). A Windows shell-tool child is assigned to a Job Object so a timeout (or the last handle closing) can terminate the whole process tree. A `HANDLE` is an index into the kernel handle table, not a pointer into our address space: every Job Object operation (Assign/Terminate/Close) is thread-safe kernel-side, so the watchdog and drain threads can share one `Arc<ChildJob>` — the Rust value is immutable and the handle is closed exactly once, by `Drop` on the last owner (`ChildJob` has no `Clone`). `ProcessIsAlive` shares a *copy* of the std `Child`'s process handle so the watchdog can distinguish "still running" from "already exited" at timeout time (the Windows analogue of the Unix pidfd/ESRCH check); it is valid until the `Child` is dropped, and the watchdog is always joined before that. The rationale lives in ARCHITECTURE.md's `tools/shell_util.rs` row.
6. **The delivery-lag byte counters** (`choreo-daemon`'s `broadcast::SubscriberSink.bytes_in_flight` — one `Arc<AtomicUsize>` per subscriber queue — plus the daemon-wide `global_lag` total). The lossless delivery design gives every client an unbounded channel and bounds memory by evicting clients whose in-flight bytes cross a threshold; a queue's byte backlog is inherently shared state, because the producers that increment it on enqueue and the connection writer thread that decrements it on dequeue run on different threads and must both touch the same running total — a channel cannot express that without a dedicated accounting thread. Lock-free, single-purpose, carries no protocol data (a bookkeeping count used only to bound per-client memory). The rationale lives in ARCHITECTURE.md's `broadcast.rs` module row.

## Inline Comments

Always write inline comments around new code explaining how it works. Focus on the "why" — the reasoning, intent, and non-obvious details — rather than restating what the code literally does.

## Pre-Commit Workflow

Before committing:
1. Run `cargo test-all` — full suite (unit + integration) via nextest must pass
2. Stage changes with `git add`
3. Commit with `git commit`

The `.githooks/pre-commit` hook has been removed. Run `cargo clippy` and `cargo fmt` manually before committing.
