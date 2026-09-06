# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Vision input: `read_image` tool feeding per-provider image parts; image
  bytes persisted durably via a `session_attachments` table.
- Image format surface: HEIC + SVG support, EXIF orientation baked in,
  AVIF behind a feature gate; image decoding consolidated into the new
  `choreo-image` crate.
- `retrieve_webpage` tool rendering pages with a local headless browser,
  with true full-page capture and `file://` URL support.
- `session_inspect` read-only diagnostic tool (debug tool group).
- Web-fetch fallback order documented in the default system prompt;
  brotli response decompression in `ureq`.
- LaTeX math rendered as pretty Unicode in markdown and the TUI.
- New `choreo-sanitize` crate hardening tool output across all tools.
- Shell tool: live stderr streaming; child-process waits moved from polling
  to channels with a bounded drain completion grace; shell tools can raise
  the outer deadline above the 300 s floor.
- models.dev provider catalog with overlay support and runtime refresh,
  persisted in the DB with a 25-hour attempt cooldown.
- Data-driven model facts in the catalog: `reasoning_content`,
  `max_output_tokens`, `supports_temperature`, `deprecated`.
- Background model prefetch: model lists warmed on session join instead of
  at unlock, never blocking ListModels.
- Per-session opencode gateway routing headers and a `choreographr`
  User-Agent on inference requests.
- Retry handling driven strictly by HTTP status and `Retry-After`, with a
  three-layer bounded retry budget.
- Live config watching: `accounts.toml` edits hot-rebuild providers.
- New `choreo-blockchain` crate with EVM/Polkadot tools behind the
  `blockchain` feature; real ENS resolution, `evm_call` block-tag support,
  RPC timeouts, single WebSocket connection.
- Choreographr Coordination Platform: new `choreo-content` crate (initially
  `choreo-coord`) behind the default-disabled `content` feature, with an
  orchestration layer composing the chain/indexer/IPFS pipelines; Substrate
  (Polkadot account) credential type and Polkadot-JS keyring import;
  `coord` tool group wired into the daemon; `coord_image` tool;
  `coord_status` reporting live chain health (best + finalized blocks).
- Keystore: per-daemon keystore unlock keys with hardened per-daemon state;
  unlock UX reworked (`/unlock` uses the stored key, `/unlock <key>` records
  it); `BindKeystore` as the sole binding path with frontend auto-bind
  (TUI/GUI/IM bind fresh unbound daemons and confirm on Bound).
- Client trust model: Noise XX first-contact mode with TCP wire v5 handshake
  preamble, fingerprint rendering and a `known_servers.toml` pin store,
  pinned-mode confirmation flow, hot-reloaded client ACL from
  `authorized_clients.toml`, `/acl add` enrollment, and the
  `choreographr acl-add` / `fingerprint` subcommands. The TUI refuses to
  start against an untrusted daemon and never crashes on connection errors.
- zstd compression of `session_turns` values via a schema 1→2 migration.
- Lossless streaming delivery: unbounded per-client queues with lag-based
  eviction (protocol v3: `TurnFinalized` removed, `Evicted` added).
- Noise transport hardening: message fragmentation with a validated length
  prefix and reassembly cap, single-writer guard, absolute handshake
  deadline, accept-time writer registration, and a concurrent-connection cap
  with `ConnectionSlot` RAII accounting.
- Default Unix socket moved under the platform temp dir, with the path named
  in bind errors; ACP adapter log file likewise, uncreatable log never fatal.
- Ctrl+C daemon shutdown path instrumented stage-by-stage with SIGINT
  regression tests.
- TUI: modal account wizard with searchable provider picker; mouse
  select-to-copy in the history pane (OSC 52); picker click-to-select with
  mouse wheel and pin-at-middle navigation; account/session rows
  click-to-enter; per-session unsent input drafts; Ctrl+Backspace clears the
  draft; model selector rebound to Ctrl+O for legacy terminals; opt-in
  side-by-side diff rendering via `diff` fences (extended to `git_add`
  output); `write_file` results rendered as markdown; request failures
  reported in the UI with a wrapped error block; exit-on-eviction/shutdown
  handling; TUI log written to the platform temp dir.
- Choreographr Coordination Platform TUI: Polkadot-account import wizard
  (`p` key).
- Build & release: Android build target (Dioxus Native GUI + Termux suite
  binaries) with automated environment discovery and setup docs; Windows
  support (`x86_64-pc-windows-gnu`); iOS support groundwork for `choreo-gui`
  (per-SDK device/simulator staging, self-contained staticlib link); a
  Termux-native `.deb` as the fifth release artifact; GitHub Actions release
  pipeline building all platforms with per-target binary-execution smoke
  tests (qemu Termux rootfs, hermetic daemon smoke on desktop artifacts)
  and an iOS build/link gate; crates.io publish path; nightly-by-default
  builds with stable via `build-stable.sh`; dedicated fat-LTO
  `[profile.dist]` shipped-artifact profile with per-target CPU floors;
  `choreo-im`, `choreo-acp`, and `choreo-mcp` feature-gated off by default.

### Changed

- Protocol rework: `DaemonMessage` split into a `SessionEvent` bus behind a
  `session_id` envelope (`Option<u64>`, replacing the `session_id: 0`
  sentinel) with explicit broadcast-origin provenance.
- Provider catalog facts (model list, reasoning support, etc.) are now
  data-driven; stale glm-5.3-flash context window corrected (200k → 1M).
- `session_turns` storage moved to the compressed schema 2 (zstd, pure-Rust
  `structured-zstd` crate replacing the C zstd binding).
- Default sockets and log files now live under the platform temp dir instead
  of hard-coded `/tmp` paths (TUI log, daemon socket, ACP adapter log).
- Dependency/MSRV: workspace crates refreshed, MSRV raised to 1.94.1 and
  decoupled from dependency resolution; release workflow actions bumped
  past the Node.js 20 deprecation; Android Termux `.deb` xz-compressed for
  Termux's dpkg and installed at the real `$PREFIX`; desktop `.deb` forced
  to xz as well.

### Removed

- `identity.pk.enc` file — the unlock key is stored in the keystore
  (`/unlock` uses it; `/unlock <key>` records it); rejected-unlock-key
  revert semantics replaced by survivor semantics.

### Fixed

- Retry: hand-built configs hardened, validation gap closed, retry budget
  bounded against pathological configurations.
- Empty assistant messages are never shipped after a model switch
  (fallback generalized beyond DeepSeek/Kimi); empty `reasoning_content` is
  injected on DeepSeek/Kimi chat assistant turns.
- Per-provider error decoding: Responses API `response.failed` object
  decoded, provider JSON error envelopes unwrapped, duplicate request
  errors deduplicated; rate-limit status carried in `RateLimited` detail.
- Mid-turn token-usage sync keeps streaming results and the scrollbar in
  lockstep; token-usage merge policy hardened with a bounded turn-version map.
- Transport: broadcast-origin tripwire gap closed, lifecycle broadcasts
  delivered to all-activity subscribers, lag-byte accounting balanced on
  every path, `approx_wire_size` a true over-estimate, tool-stream
  abort-disconnect truncation fixed, peer close classified as
  `ConnectionClosed`.
- Shell streaming: byte-identical truncated records, bounded line memory,
  escape-before-budget, char-boundary flushes, footer-safe finish path,
  one-shot VM truncation, bounded HTTP bodies, CRLF folding.
- TUI: copy selection preserves blank lines and copies unwrapped text;
  selection stays anchored to the text and tracks the cursor while
  scrolling; highlight visible on shaded turns; selection clamped to
  per-line content ranges; side-by-side diff panes pinned to exact width;
  diff fences hardened against early-close; tabs expanded to 4-column
  stops; plain-text tool output wrapped at content width; `git_show`
  commit/tag messages emitted unindented and fenced, directory entries
  skipped in commit diffs; "Running command:" line shown reliably during
  streaming; list-click hit-test clamped to drawn rows; pre-migration DB
  backup taken before redb locks the file.
- Vision: decode limits aligned across providers; HEIC grid canvas bounds
  verified via iloc/grid parsing; raster dimension probe guarded.
- Coordination platform: revision resolution filtered to the item's own
  events; indexer key wire format and camelCase event decoding corrected.
- Windows: MSVC link failure fixed (windows-sys `WaitForSingleObject`);
  bionic TLS-alignment abort on Android fixed (.tdata/.tbss aligned to 64
  at link); BSD sed compatibility in `build-stable.sh`.

### Security

- Dependency supply chain hardened against the arrayref attack.
- Tool-output safety: six output-sanitization gaps closed across the tool
  suite; shell-tool spawning hardened; streaming bounded across all tools.
- Keystore: secrets zeroized; credential modal, keystore auto-bind, and
  daemon lock-state handling hardened.
- Transport: fragment reassembly capped and continuation authenticated;
  handshake hardened to an absolute deadline; concurrent connections
  capped; writer-loop joins bounded.
- Trust: client fingerprint comparison tightened with pinned-mode failure
  UX; enrollment & transport trust model documented in ARCHITECTURE.md.

## [0.1.0]

Initial release: daemon, TUI, protocol, Noise transport, provider catalog,
markdown rendering, PDF tooling, and the 14-crate crates.io suite.

[Unreleased]: https://github.com/choreographr/choreographr/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/choreographr/choreographr/releases/tag/v0.1.0
