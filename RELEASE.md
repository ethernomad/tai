# Release SOP — Choreographr

Standard Operating Procedure for cutting a Choreographr release. Follow the
phases in order; each phase has a **gate** that must pass before moving on.

A release ships three things:

1. **14 crates to crates.io** (everything except `choreo-gui`, in dependency
   order) — enables `cargo install choreographr` / `cargo binstall`.
2. **GitHub release `vX.Y.Z`** on `choreographr/choreographr` with prebuilt
   artifacts (musl/macOS/Android tarballs, Windows `.zip`, desktop `.deb` +
   `.rpm`, Termux-native `.deb`, combined `SHA256SUMS`) — enables Homebrew,
   AUR, `cargo binstall`, and the `choreographr.com` installer.
3. **Channel updates** — Homebrew tap, AUR, choreographr.com.

One release conductor drives all three. The binary artifacts for all shipped
platforms are built **on GitHub Actions** by `.github/workflows/release.yml`
(see [CI builds](#ci-builds-github-actions)): pushing the `vX.Y.Z` tag builds
every platform and creates the GitHub release with the combined
`SHA256SUMS`. The two-machine build/upload flow is no longer part of the
normal path — it is documented in condensed form in the
[appendix](#appendix--manual-build--upload-fallback) for releases from
machines without push access (rare; it also covers what CI does not, namely
nothing — the channel updates are conductor tasks in every case).

---

## CI builds (GitHub Actions)

`.github/workflows/release.yml` builds the release binaries on GitHub-hosted
runners. It reuses `scripts/release.sh` (Linux + macOS jobs run it verbatim)
and `scripts/build-android.sh`, so build flags, `--locked`, feature selection,
artifact naming, and smoke tests stay in the scripts — the workflow only adds
per-runner toolchain setup and artifact plumbing. `choreo-gui` (a stub) is
built nowhere.

**Triggers** (deliberately narrow — full multi-platform builds are expensive):

- **`v*` tag push** — builds everything, then creates the GitHub release with
  all artifacts and one combined `SHA256SUMS`. The release job guards that the
  pushed tag matches the manifest version, extracts the version's
  `CHANGELOG.md` section for the release body, and creates the release with
  all artifacts and one combined `SHA256SUMS`.
- **`workflow_dispatch`** — identical builds, but **no release is created**;
  artifacts attach to the workflow run (default 90-day retention). This is
  how the pipeline itself is tested without spamming tags.

| Job | Runner | Artifacts |
|---|---|---|
| `linux-musl` | ubuntu-latest | static `x86_64-unknown-linux-musl` tarball + `.deb` + `.rpm` (via `scripts/release.sh`; `rpmbuild` is apt-installed in the job, since it is not preinstalled) |
| `macos-arm64` | macos-latest | native `aarch64-apple-darwin` tarball (via `scripts/release.sh`) |
| `windows-msvc` | windows-latest | `x86_64-pc-windows-msvc` zip of the shipped `.exe` files |
| `android-termux` | ubuntu-latest + NDK | `aarch64-linux-android` Termux tarball (via `scripts/build-android.sh --features metrics,blockchain`) + the Termux-native `.deb` (via `scripts/build-deb-termux.sh`, structural smoke-test on the runner; the packaged binaries are then extracted with Termux's own dpkg-deb under qemu-user and executed against an unpacked Termux aarch64 rootfs — see the workflow's qemu step) |
| `ios-build` | macos-latest | **none** — `choreo-gui` (the only crate that ships to iOS) compile check for both iOS targets, the real Xcode app link via the `ios/` scaffold, and a non-blocking simulator boot smoke (`continue-on-error` until the plumbing has proven stable). Deliberately not part of the release; a failing link is diagnosed from the log |

Every build job smoke-tests its own artifact beyond the clap surface:
the three desktop jobs run `scripts/daemon-smoke.sh` (boots the shipped
daemon hermetically — scratch socket + config dir — and proves the listener
comes up), and the android job **executes** its binaries under qemu-user
against the official Termux aarch64 rootfs (skopeo fetches the image layers;
no docker), closing the "never executed before release" gap.

The `release` job (tag pushes only) downloads all build-job artifacts,
generates one combined `SHA256SUMS` over everything, guards that the pushed
tag matches the manifest version, extracts the version's section from
`CHANGELOG.md` (the Keep a Changelog promotion from Phase 1 makes it the
release body — a missing section fails the job), and creates the release
with `gh release create vX.Y.Z dist/* --notes-file … --generate-notes`. A
re-run after the release already exists fails on create — assets are
immutable once uploaded; delete and re-create per the
[Hotfix / rollback](#hotfix--rollback) section rather than editing the job.

Every job builds the same shipped binaries (`choreographr choreo-tui` — the
`choreo-im`/`choreo-acp` bridges are feature-gated and excluded from release
artifacts) with `--features metrics,blockchain` on the **stable** toolchain,
smoke-tests its artifact, and uploads it; the `release` job only runs for tag
pushes. Windows/Termux artifacts ship **in addition to** the Homebrew/AUR
channels — those package the same tarballs CI produces.

How this slots into the SOP: push the `vX.Y.Z` tag (Phase 3) **after** the
crates.io publish (Phase 2) — the tag push both publishes binaries and is the
release trigger — then verify the release page (Phase 3's gate) and do the
channel updates in Phase 4 as before.

---

## Versioning & gates

- **Version source of truth:** `[workspace.package] version` in the root
  `Cargo.toml`. `scripts/release.sh`, the Homebrew formula, and the AUR
  PKGBUILD all mirror it — do not edit them by hand for a version bump; let
  `cargo release` do it (Phase 1).
- **Tag format:** `vX.Y.Z` (e.g. `v0.1.1`). Release notes are generated from
  the tag diff (`gh release create --generate-notes`).

### Preflight (before Phase 1)

```bash
# 1. Working tree clean, on master, up to date with origin.
git status --porcelain      # must be empty
git checkout master && git pull --ff-only origin master

# 2. Full quality gate — fmt, clippy (warnings denied), unit + integration.
just ci

# 3. Tooling the conductor runs locally (Phases 1–5): gh, jq, git.
#    The per-platform build toolchains (zig, cargo-zigbuild, NDK) live in the
#    CI workflow — no local setup is needed unless you are using the manual
#    fallback in the appendix.
just preflight               # checks cargo + zig, notes nextest
```

---

## Phase 1 — Decide & bump the version

1. **Decide the level** — the release conductor's judgment call, made before
   any tooling runs. There are only three options; which one applies is
   determined by what changed since the last tag (the `CHANGELOG.md`
   `[Unreleased]` section is the working evidence — keep it current as
   features land):

   | Level | Bump | When to pick it |
   |---|---|---|
   | `patch` | 0.1.0 → 0.1.1 | Bug fixes, security fixes, doc/UX polish — no new user-facing features |
   | `minor` | 0.1.1 → 0.2.0 | New features or behavior changes. While on 0.x, breaking changes also land here (semver treats 0.x minor as "may break") |
   | `major` | 0.2.0 → 1.0.0 | Breaking changes after 1.0, or the deliberate move to 1.0.0 (stability commitment) |

   **After 1.0.0 this policy shifts.** `minor` (1.0.0 → 1.1.0) starts
   *promising* backwards compatibility, so breaking changes move from
   `minor` to `major` (1.x → 2.x) and the everyday bump becomes `minor`, not
   `patch`. The inter-crate requirements flip from `"0.1"` (which Cargo reads
   as `< 0.2`) to `"1"` (`< 2`), so `dependent-version = "fix"` stops
   rewriting manifests on ordinary releases and only fires on a major. Update
   this table's examples when 1.0.0 ships (Phase 5 commits doc drift).

2. **Enact the decision** — the command that carries it out is
   `cargo release version <level>`, where `<level>` is replaced with the
   level you decided in step 1 (`patch` / `minor` / `major`). Nothing else
   needs to know the decision: `cargo release publish` takes no level, and
   there is no config flag — the level is this one argument. The command
   makes the single `[workspace.package] version` edit (plus `Cargo.lock`);
   all members inherit it. Dry-run first (the default); `-x` applies it:

   ```bash
   cargo release version <level>    # dry-run: preview the bump plan
   cargo release version <level> -x # apply — e.g. decided `minor`:
                                    #   cargo release version minor -x
                                    # edits version = "0.1.1" → "0.2.0"
   ```

   `cargo release version` only edits the manifests — it does **not** commit
   or tag. Before committing: promote the changelog section — rename
   `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD` in `CHANGELOG.md` (and
   start a fresh empty `[Unreleased]` above it, moving the compare link),
   plus update any user-facing docs that state a version or install command
   (README install section):

   ```bash
   git add Cargo.toml Cargo.lock README.md CHANGELOG.md   # + any other docs touched
   git commit -m "release: bump to X.Y.Z"
   ```

   (Prefer this over the one-shot `cargo release <level>`, which bumps, tags,
   publishes, and pushes in a single cargo-release-made commit — fine when
   nothing else needs to ride along with the bump.)

3. **Tag name check:** confirm no tag `vX.Y.Z` exists yet:
   `git ls-remote --tags origin | grep vX.Y.Z`.

   > **Why the changelog section must exist at the tag:** the CI `release`
   > job extracts the `## [X.Y.Z]` section from `CHANGELOG.md` for the
   > release body and fails the job if it is absent — the curated notes are
   > the release notes, not an afterthought.

4. **Tag the bump commit** (cargo-release reads the version back from
   `Cargo.toml`): `cargo release tag -x` → creates `vX.Y.Z` at HEAD. The tag
   is pushed together with the commit once Phase 2 has published. First
   release only: if `v0.1.0` was already tagged locally before the release
   tooling existed (`git tag -l`), `cargo release tag` reports `disabled due
   to existing tag` and skips — that's fine as long as the tag sits on the
   commit you're shipping; just push it in Phase 2.

**Gate:** `just ci` green, tree clean, no conflicting tag.

---

## Phase 2 — Publish crates to crates.io

Runs **before** any binary building (binaries are versioned by the same
bump, and `cargo install` must resolve the published crates), on a clean
tree:

0. **Sync the MSRV claim first.** Dependency resolution is MSRV-unconstrained
   (`resolver.incompatible-rust-versions = "allow"` in `.cargo/config.toml`),
   so the workspace `rust-version` may lag the resolved tree. Compute the
   resolved tree's floor:
   `cargo metadata --format-version 1 | jq -r '[.packages[].rust_version | select(. != null)] | sort_by(split(".") | map(tonumber)) | last'`
   and set the result in `[workspace.package]` `rust-version` (root
   `Cargo.toml`), committing the bump together with `Cargo.lock`, so the
   crates.io metadata published below is truthful.

```bash
./scripts/publish-stable.sh publish --workspace  # publishes the Phase-1-bumped version in
                                                 # topological order — publish does NOT bump
                                                 # or tag (that was `cargo release version`
                                                 # and `cargo release tag` in Phase 1)
```

Publishing runs through `scripts/publish-stable.sh` (or `just publish-stable`),
not bare `cargo release`: the per-profile `rustflags` keys in the root
`Cargo.toml` (the `-Z…` frontend flags plus the repo-wide `-C target-cpu=native`)
require the nightly-only `profile-rustflags` cargo feature, and they ride along
into the uploaded `.crate`. A published manifest that still contains
`[profile.*] rustflags` **hard-breaks stable `cargo install`** (verified: stable
cargo errors with "The package requires the Cargo feature called
`profile-rustflags`") — which would kill the crates.io install route this SOP
verifies in Phase 5. The wrapper strips exactly those keys (plus the
`[unstable]` config opt-in) while `cargo release` runs and restores them on
exit, so the published source builds at the target's default CPU on stable,
exactly like the dist binaries.

The wrapper always appends `--exclude choreo-gui`: cargo-release 1.1.5 does
not honor the GUI crate's `publish = false` when selecting with
`--workspace` (verified: its plan includes `choreo-gui`, and a real publish
would then fail on cargo's own refusal for a publish=false crate). The wrapper
also owns the dirty-tree gate for the publish: it refuses to
start on an uncommitted tree by default — the `.crate` is built from the
working tree (cargo package), so unreviewed uncommitted code must never ship.
Pass `--allow-dirty` to skip that gate (e.g. to dry-run a plan from a dirty
tree; it does NOT make an execute-publish work on a dirty tree — see below).
The strip itself dirties the tree, but cargo-release 1.1.5 enforces an
**unconditional** clean-tree check on the publish step (there is no
`--allow-dirty` flag or config key in this version — `verify_git_is_clean` in
cargo-release's `src/steps/mod.rs` hard-fails on any dirty tree in execute
mode), so the wrapper masks exactly the two files it modifies with
`git update-index --skip-worktree` for the duration of the run (libgit2 then
reports them clean), and clears the masks on exit. Net effect: an actual
(non-dry-run) publish still requires a clean — committed — tree for every
file except the wrapper's own two masked files, which is exactly the hygiene
the gate exists for.

`--workspace` is **mandatory**. cargo-release ≥ 1.0 selects only the current
package by default: a bare `cargo release publish` plans just `choreographr`,
marks every workspace member as `disabled by user, skipping`, and then dies
with `error: choreographr 0.1.0 depends on unpublished workspace package
choreo-*` — the root's deps are neither in the publish set nor on crates.io
yet. `--workspace` puts all 14 publish-set members in the set; cargo-release
hands them to a single `cargo publish` call and cargo uploads them in
dependency order. `choreo-gui` is kept out by the wrapper's explicit
`--exclude choreo-gui` — cargo-release 1.1.5 does *not* honor `publish = false`
in `--workspace` selection, despite the GUI crate's manifest flag.

- `[workspace.metadata.release]` sets `dependent-version = "fix"`, so
  cross-crate requirements (`choreo-tui = "0.1"`, …) stay in lockstep across
  the whole publish set — `cargo release version` already rewrote them when
  it bumped. Exact subcommand/flags vary by cargo-release version —
  `cargo release --help` for the installed one.
- Push the bump commit and the `vX.Y.Z` tag created in Phase 1:
  `git push origin master --tags`. **This tag push is also the CI build
  trigger** — see Phase 3.
- Verify the published suite installs cleanly from source in a scratch
  `CARGO_HOME` (needs `zig` on PATH — zlob's `build.rs`):

```bash
export CARGO_HOME=$(mktemp -d)
cargo install choreographr --locked
~/.cargo/bin/choreographr --version    # must print X.Y.Z
```

#### New-crate rate limit

crates.io throttles **new-crate creation** per account to a burst of **5** with
refill of **1 every 10 minutes** (a token bucket; updates to existing crates
get burst 30/minute). cargo-release mirrors this via
`rate-limit-new-packages` in `[workspace.metadata.release]` (default 5) and
refuses upfront when a plan would publish more new crates than the burst:

```
error: attempting to publish N new crates which is above the rate limit: 5
error: dry-run failed, resolve the above errors and try again.
```

The 0.1.0 first release had **12 new crates** (see the batched staging plan
below for how that was done). The next release publishes **12 updates plus two
new crates — `choreo-blockchain`** (the blockchain-tools crate added since
0.1.0, referenced by `choreo-daemon`'s optional `blockchain` feature) **and
`choreo-sanitize`** (the shared string-safety leaf, previously unpublished —
it must ship because `choreo-blockchain` and the other members depend on it,
and cargo-release's verification rejects unpublished workspace deps). Two
new crates fit easily in the burst, so no batching is needed; if a future
release ever introduces several new crates at once, stage them in
≤ 5-crate batches:

1. **Ask crates.io for a burst override** on the publishing account (the
   crates.io team raises the per-user burst in `publish_rate_overrides`). Then
   set `rate-limit-new-packages` to match and publish in one shot:
   `cargo release publish --workspace -x`.
2. **Stage the first release in dependency-closed batches of ≤ 5 new crates**
   using `-p` selection, waiting ~10 minutes (one token refill) between
   batches. Every workspace dependency of a batched crate is either in the
   batch or already published:
   - Batch 1: `./scripts/publish-stable.sh publish -p choreo-proto -p choreo-keystore -p choreo-markdown -p choreo-mcp -p choreo-transport -x`
   - Batch 2: `./scripts/publish-stable.sh publish -p choreo-blockchain -p choreo-acp -p choreo-ai-protocols -p choreo-client-core -x`
   - Batch 3: `./scripts/publish-stable.sh publish -p choreo-daemon -p choreo-im -p choreo-tui -x`
   - Batch 4: `./scripts/publish-stable.sh publish -p choreographr -x`

   Dry-run each batch first (omit `-x`) and confirm it plans only that
   batch's crates. Once all 14 exist on crates.io, later releases are
   *updates* and go in a single `./scripts/publish-stable.sh publish --workspace -x`.

**Gate:** 14 crates published, `cargo install choreographr --locked` works in
a scratch CARGO_HOME, tag `vX.Y.Z` pushed.

---

## Phase 3 — CI build & GitHub release (tag push)

The `vX.Y.Z` tag pushed in Phase 2 is the build trigger. Nothing to run
locally — `.github/workflows/release.yml` builds every platform, smoke-tests
each artifact, and creates the GitHub release automatically (job table and
details in [CI builds](#ci-builds-github-actions)).

Conductor duties while the workflow runs:

1. **Watch the run** (`gh run watch` on the `release` workflow) — the four
   build jobs must all go green. The `ios-build` job may report its
   (non-blocking) smoke result; investigate a failure in the log, but it
   does not hold the release.
2. **Verify the release page** once the `release` job completes:
   - the tag on the release matches `vX.Y.Z` and the manifest version
     (the job guards this too — a guard failure means a Phase 1/2 mistake);
   - all assets are present: three tarballs (musl, macOS, Android Termux),
     the Windows `.zip`, the desktop `.deb` and `.rpm`, the Termux-native
     `.deb`, and the combined `SHA256SUMS`;
   - each asset downloads.

**Gate:** workflow green, release page lists all assets + `SHA256SUMS`,
assets download.

---

## Phase 4 — Channel updates

### Homebrew tap (`choreographr/homebrew-choreographr`)

Run the tap updater from a `dist/` holding the release's tarballs — with the
CI path, download them from the release first (the updater hashes the exact
artifacts that were uploaded; it does not re-download to compare):

```bash
gh release download vX.Y.Z -p 'choreographr-*.tar.gz' -D dist/
scripts/update-homebrew-tap.sh            # dry-run: shows the diff, pushes nothing
scripts/update-homebrew-tap.sh --push     # commit + push to the tap repo
```

`scripts/update-homebrew-tap.sh` reads the version from `Cargo.toml`,
recomputes both `sha256` digests from the `dist/` tarballs, rewrites
`Formula/choreographr.rb` in `choreographr/homebrew-choreographr` (version,
both `url` lines, both digests), validates the result (exact-count rewrite
checks, no stale version/placeholder, `ruby -c` syntax check when ruby is
present), and prints the diff. `--push` commits and pushes to the tap repo's
default branch. The x86_64 branch is left untouched when no
`choreographr-<V>-x86_64-apple-darwin.tar.gz` is in `dist/` (Intel macOS is
not shipped yet — the branch stays a placeholder).

The one step that stays manual, on a Mac (Homebrew is macOS-only):

```bash
brew install ./choreographr.rb && choreographr --version
```

…then commit the mirrored-formula drift in this repo
(`packaging/homebrew/choreographr.rb`) during Phase 5.

Manual fallback (what the script automates — only when the script cannot be
run):

1. Bump `version` to `X.Y.Z` in `Formula/choreographr.rb` (mirrored in this
   repo at `packaging/homebrew/choreographr.rb`).
2. Update both `url` lines — tag, filename, and embedded version.
3. Recompute the digests: `curl -fL -O <url> && shasum -a 256 <downloaded>.tar.gz`.
4. Sanity-check: `brew install ./choreographr.rb && choreographr --version`.
5. Commit + push to the **tap repo** (not this repo).

### AUR (`choreographr-bin`)

Edit `packaging/aur/PKGBUILD`:

1. Bump `pkgver` to `X.Y.Z`, reset `pkgrel` to `1`.
2. Update the `source` URL and `sha256sums` (take the digest from the combined
   `SHA256SUMS` — the tarball is `choreographr-<V>-x86_64-unknown-linux-musl.tar.gz`).
3. Regenerate and push:
   ```bash
   cd packaging/aur && makepkg --printsrcinfo > .SRCINFO && git add PKGBUILD .SRCINFO
   ```

### choreographr.com (static hosting)

1. Publish `scripts/install.sh` (or a per-version
   `install/vX.Y.Z.sh` and repoint `install.sh` — keep the versioned URL
   scheme from day one).
2. Add `/download/vX.Y.Z/…` 302 redirects for each asset (tarballs, Windows
   `.zip`, `.deb`, `.rpm`, Termux `.deb`) → the GitHub release URLs.
3. Publish `/releases/SHA256SUMS` (the combined file).

**Gate:** every channel's `--version` reports `X.Y.Z`.

---

## Phase 5 — Post-release verification

Exercise every install route from a clean environment:

| Route | Command | Expect |
|---|---|---|
| crates.io (source) | `cargo install choreographr --locked` (with zig) | builds, `--version` = X.Y.Z |
| binstall (prebuilt) | `cargo binstall choreographr` | fetches tarball, no toolchain |
| Homebrew | `brew tap choreographr/choreographr && brew install choreographr` | no quarantine friction |
| AUR | `choreographr-bin` | installs, `choreographr --version` |
| curl installer | `curl -fsSL https://choreographr.com/install.sh \| sh` | sha256-verified extract |
| .deb / .rpm | `dpkg -i` / `dnf install` on clean distro VMs | installs; unit present, **not enabled** |
| Termux | `dpkg -i` the Termux-native `.deb` on a device | installs; binaries run under Termux's $PREFIX |

Confirm the service policy held everywhere: the systemd unit / launchd agent
is installed but **never auto-enabled** — `systemctl --user enable --now
choreographr` / `launchctl load …` remain explicit user actions.

Finally, commit any post-release doc/version drift in this repo and push.

---

## Hotfix / rollback

- **Bad crates.io publish:** yanking is a last resort (breaks `--locked`
  installs). Prefer publishing an immediate patch (Phases 1–5) — crates.io
  treats versions as immutable, so the patch **is** the fix.
- **Bad GitHub release:** `gh release delete vX.Y.Z` then re-create after
  fixing; assets are immutable once uploaded, so re-create with corrected
  artifacts (re-pushing the tag re-triggers the CI build — the `release` job
  fails on an existing release, so delete the release first).
- **Channel rollback:** Homebrew — revert the tap commit; AUR — bump `pkgrel`
  (`pkgrel=2`) or revert and push; choreographr.com — point redirects at the
  previous version (the versioned URL scheme makes this a one-line change).
- Hotfixes still run the full SOP; `--allow-dirty` is only for CI-style
  staged-but-uncommitted trees, never a substitute for the quality gate.

---

## Quick checklist (condensed)

- [ ] `just ci` green; tree clean; master pulled
- [ ] MSRV sync: `cargo metadata --format-version 1 | jq -r '[.packages[].rust_version | select(. != null)] | sort_by(split(".") | map(tonumber)) | last'` → update `rust-version` in `[workspace.package]` (with `Cargo.lock`) if changed
- [ ] `CHANGELOG.md`: move entries from `[Unreleased]` into a new `## [X.Y.Z] - YYYY-MM-DD` section (fresh empty `[Unreleased]` + compare link above it)
- [ ] `cargo release version <level> -x` (level from Phase 1) → bump committed with doc updates; `cargo release tag -x` → `vX.Y.Z`
- [ ] `./scripts/publish-stable.sh publish --workspace` → 14 crates on crates.io; `cargo install --locked` verified
- [ ] First release only: 12 new crates staged in ≤5-crate batches (or crates.io burst override) — see Phase 2; the next release adds `choreo-blockchain` and `choreo-sanitize` as new crates
- [ ] Push the bump commit + `vX.Y.Z` tag → CI builds all platforms and creates the GitHub release; verify the release page lists every asset + `SHA256SUMS` and they download
- [ ] `gh release download vX.Y.Z -p 'choreographr-*.tar.gz' -D dist/`, then `scripts/update-homebrew-tap.sh --push`; `brew install` verified on a Mac
- [ ] AUR `pkgver`/`sha256sums` bumped, `.SRCINFO` regenerated, pushed
- [ ] choreographr.com: `install.sh`, `/download/vX.Y.Z/` redirects, `/releases/SHA256SUMS`
- [ ] All install routes verified (`cargo install`/`binstall`, brew, AUR, curl, .deb, .rpm, Termux)
- [ ] Service policy confirmed: installed, never auto-enabled

---

## Appendix — manual build & upload fallback

Only for releases from machines without push access to trigger CI (the
normal path is [Phase 3](#phase-3--ci-build--github-release-tag-push)). Both
desktop machines run `scripts/release.sh`, which:

- builds the shipped binaries on **stable** Rust (each cargo build runs through
  `scripts/build-stable.sh`, reproducible and matching the crates.io/MSRV
  story; see the README build notes) under the workspace's dedicated
  `[profile.dist]` profile — `--profile dist`, not `--release` — so the
  shipped artifacts land in `target/<triple>/dist/`, separate from any local
  `cargo build --release` output the packaging steps could otherwise pick up
  by mistake (see root `Cargo.toml`),
- builds every artifact at an explicit **CPU floor per target** via
  `RUSTFLAGS="-C target-cpu=…"` (see ARCHITECTURE.md "Release & packaging"):
  x86-64-v2 for the musl tarball, the target default (`apple-a14`) for macOS,
  baseline for the `.deb`/`.rpm` — the local `-C target-cpu=native` profile
  flags (and the nightly `-Z…` flags) are additionally stripped by
  `scripts/build-stable.sh` before each stable build, so the build machine's
  CPU can never leak into a shipped artifact,
- reads the version from `Cargo.toml`,
- guards against a dirty tree,
- builds with `--features metrics,blockchain`,
- writes the tarball + `SHA256SUMS` (covering everything already in `dist/`
  for this version) into `dist/`,
- builds `.deb`/`.rpm` best-effort (Linux only, host glibc, no mimalloc),
- prints the `gh release create` command and the post-publish checklist.

The manual flow needs one **Linux x86_64 box** (musl tarball — static,
mimalloc — plus `.deb`/`.rpm`; needs `cargo-zigbuild`, optional
`dpkg-deb`/`rpmbuild`) and one **M1 MacBook** (native aarch64 tarball).
Artifacts are staged and uploaded from the Linux box — the macOS tarball is
copied there before upload. Windows and Android/Termux artifacts have no
manual path; if CI is unavailable for them, skip those assets for the
release or wait for CI (a re-pushed tag after `gh release delete` re-triggers
it).

### Linux x86_64 box

```bash
just release            # dry-run: musl tarball + SHA256SUMS + .deb + .rpm
just smoke-test         # extract tarball; verify 4 binaries, --version, --help
```

Confirm `dist/` contains the musl tarball, `.deb`, `.rpm`, and `SHA256SUMS`.

### M1 MacBook

```bash
just release            # dry-run: aarch64 tarball + SHA256SUMS (no .deb/.rpm)
just smoke-test
```

Then the **manual daemon smoke test** (the tarball smoke test only checks
`--version`/`--help`; CI's `scripts/daemon-smoke.sh` covers this normally):

1. Extract the tarball, run `./choreographr` — confirm the socket
   (`/tmp/Choreographr.sock`) and keystore initialize.
2. Load the bundled `com.choreographr.daemon.plist` in a throwaway launch
   agents dir; confirm the daemon starts and logs to `/tmp/choreographr.log`.
3. Run `./choreo-tui` and complete one round-trip with a configured account.

### Assemble & upload (Linux box)

GitHub uploads happen **once, from the Linux box**, so all assets land in one
release:

```bash
scp macbook:…/choreographr-<V>-aarch64-apple-darwin.tar.gz dist/
just smoke-test         # re-validate on the Linux box for good measure
just release-upload     # regenerates a combined SHA256SUMS over ALL dist/ artifacts
                        # (host tarball + staged macOS tarball + .deb/.rpm) and
                        # uploads every tarball it finds + SHA256SUMS + .deb/.rpm
```

`scripts/release.sh` regenerates `SHA256SUMS` from the `choreographr-<V>-*`
glob **after** the `.deb`/`.rpm` step and assembles the upload list from every
tarball present in `dist/` — so staging the macOS tarball first is what makes
the uploaded checksum file complete and the macOS asset appear in the release.

Equivalent manual form (what `--upload` assembles):

```bash
gh release create vX.Y.Z \
  dist/choreographr-X.Y.Z-x86_64-unknown-linux-musl.tar.gz \
  dist/choreographr-X.Y.Z-aarch64-apple-darwin.tar.gz \
  dist/choreographr-X.Y.Z-x86_64.deb \
  dist/choreographr-X.Y.Z-x86_64.rpm \
  dist/SHA256SUMS \
  --title "choreographr X.Y.Z" \
  --notes-file <(awk -v ver="X.Y.Z" '$0 == "## [" ver "]" {f=1; next} f && /^## /{exit} f{print}' CHANGELOG.md) \
  --generate-notes
```

**Gate:** release page lists the five manual-flow assets + `SHA256SUMS`;
assets download.
