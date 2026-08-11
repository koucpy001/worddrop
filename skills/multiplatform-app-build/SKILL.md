---
name: multiplatform-app-build
description: Use when building or extending a cross-platform app that pairs a Rust core with a Flutter GUI via flutter_rust_bridge/cargokit (desktop + Android), or when a multi-target build, dependency resolution, or native-bridge step fails. Triggers - cross-platform app, Flutter + Rust bridge, flutter_rust_bridge, cargokit, iroh transfer, multi-ABI APK, desktop + Android from one core, "build fails on one platform", dependency version mismatch, native cdylib, AGP/Gradle incompatibility, binary too large, redb/store lock hang.
---

# Multi-platform app build (Rust core + Flutter GUI)

Lessons distilled from building **my-croc** (WordDrop): a 4-platform WAN file-transfer app
(Linux CLI + Windows/macOS GUI + Android) on a shared Rust core, wired to Flutter through
flutter_rust_bridge (FRB) + cargokit. Every entry below is a real failure hit and corrected
during that build — collected so the next multi-platform build skips them.

**Leading words:** _pin-and-verify_ dependencies, _pre-flight_ the toolchain, _split the secret_
from the server, _fail-fast_ on hostile inputs, _record honestly_ instead of claiming.

## When to use
- Starting a cross-platform app with a Rust core + Flutter (or any FFI bridge) front-end.
- A build/bridge/dependency step fails on one platform but not another.
- Choosing dependency versions, security model, or test strategy for a multi-target codebase.

## Pre-flight checklist (do BEFORE the first build)
Run these checks first — each one prevented a class of failure that has no mid-build mitigation.

1. **System build deps present.** `which cc cmake nasm` (and the platform GUI toolchain:
   clang/ninja/pkg-config/libgtk for Linux desktop; NDK for Android). Rust crypto stacks
   (e.g. aws-lc-rs via rustls) need a C toolchain + cmake + nasm. Install before building.
2. **Know the host limits.** Low RAM (≤4GB) → `cargo build -j 2`, Gradle `org.gradle.jvmargs=-Xmx1g`.
   Track a disk budget (Android SDK ~3G + Flutter ~2G + cargo target). Record actual limits.
3. **Pin-and-verify every load-bearing version.** Copy the exact version PAIR from a reference
   project's `Cargo.lock` that is known to build — not from its `Cargo.toml` alone. Two crates
   must be API-compatible generations (see F1 below). Confirm the pair resolves before writing code.
4. **Mirror/registry reachability.** If crates.io is blocked (403), configure a sparse mirror
   (e.g. rsproxy) in `~/.cargo/config.toml` BEFORE the first fetch; some deps are not on the mirror.
5. **Decide the test strategy up front.** TDD for core logic (crypto, state machines, protocol);
   tests-after for UI/bridge wiring. Match test depth to risk, not uniformly.

## Failure → Diagnosis → Fix catalog

### Dependencies & versions
- **Symptom:** compile errors that make no sense against the docs; API "doesn't exist".
  **Root cause:** version generation mismatch — a reference project pinned an OLDER generation
  (my-croc: drift pins iroh-blobs 0.99; the working pair is iroh 1.0.3 + iroh-blobs 0.103, an
  incompatible API). Copying one crate's version without its partner breaks the pair.
  **Fix:** pin the verified PAIR from a building reference's `Cargo.lock`; port code from a
  reference that uses the SAME generation (sendme, not drift, in this case).
  **Prevention:** record the chosen pair + the reference it came from in the plan/learnings.

- **Symptom:** a newly-added crate fails to resolve or pulls a duplicate version.
  **Root cause:** adding a dep that is already in the lockfile transitively, at a different semver.
  **Fix:** prefer the version already in the tree; only add a direct edge when you actually call it.
  **Prevention:** `cargo tree -p <crate>` before adding; keep the dep set minimal and justified.

### Security model (the load-bearing design)
- **Principle — split the secret from the server.** The pairing secret (the word-code) must NEVER
  reach the rendezvous/relay server. Send only a server-visible nameplate (a number); keep the words
  client-side as the PAKE (SPAKE2) password, exchanged over the already-encrypted transport. This is
  the magic-wormhole model and is what makes an untrusted/compromised server unable to MITM.
  **Prevention:** add an explicit test that the server never receives word-shaped data; audit request
  paths/bodies/logs for the secret.

- **Symptom:** session key recoverable from a memory dump of a compromised host.
  **Root cause:** ephemeral session key not zeroized on drop.
  **Fix:** `#[derive(ZeroizeOnDrop)]` on the session-key type (parity with the transport's SecretKey).

- **Symptom:** identity key file silently corrupted with no detection.
  **Root cause:** raw 32-byte key with no checksum — same-length corruption is undetectable (ed25519
  accepts any 32-byte seed).
  **Fix:** accept the honest contract (document it) OR add a checksum; write keys atomically
  (temp file + rename, set 0600 BEFORE writing bytes, `sync_all`, retry-once on stale tmp).

### Native / runtime pitfalls (found only by running)
- **Symptom:** second process opening the store HANGS (no error) instead of failing.
  **Root cause:** iroh-blobs' redb store (`blobs.db`) is single-process exclusive; a concurrent open
  blocks on the file lock rather than returning `DatabaseAlreadyOpen`.
  **Fix:** role-split data dirs (sender and receiver each get their own dir); never share one store
  across processes/roles. Document the exclusivity.

- **Symptom:** engine startup blocks forever.
  **Root cause:** `FsStore::load(root)` blocks indefinitely when `root` exists but is a FILE.
  **Fix:** fail-fast guard — check the data dir is a directory BEFORE calling load. Do not simplify away.

- **Symptom:** dialing a dead peer retries forever; the receive never times out.
  **Root cause:** the QUIC layer (noq/quinn) has NO connect-handshake timeout; ICMP port-unreachable
  is never surfaced.
  **Fix:** bound every dial with an explicit `CONNECT_TIMEOUT` and map it to a typed error.
  **Generalization:** assume NO network call has a default timeout; wrap every one.

- **Symptom:** Flutter FRB `StreamSink` subscription `.cancel()` never completes in the Dart VM.
  **Root cause:** known FRB/Dart-VM quirk.
  **Fix:** never `await` the cancel in tests; document the workaround. (Third-party behavior, not your bug.)

- **Symptom:** `file_picker` (or another plugin) breaks the Android build on a new AGP.
  **Root cause:** plugin × AGP version incompatibility (file_picker 11 × AGP 9).
  **Fix:** pin the last compatible AGP (8.11.1) + matching Kotlin; leave a revert note for when the
  plugin catches up.

### Testing & scoping
- **Principle — TDD the core, tests-after the wiring.** Core crypto/state/protocol get tests written
  FIRST; CLI/GUI/bridge wiring gets tests-after. This matched the real risk distribution.
- **Principle — record honestly, never claim.** Anything not actually run (Android device test,
  Win/Mac artifacts before CI) is recorded as DEFERRED with a checklist — never marked passed.
  Evidence records the deferral, not a fabricated pass.
- **Principle — accept infeasible targets honestly.** A <5MB CLI was impossible with the iroh tree;
  the honest move was to record the deviation + root cause, not chase an impossible number.

## Principles (the short version)
1. _Pin-and-verify_ dependency PAIRS from a building reference's lockfile, not manifests.
2. _Pre-flight_ system deps, host limits, and registry reachability before the first build.
3. _Split the secret_ from any server you do not fully trust (nameplate ≠ password).
4. _Fail-fast_ on hostile inputs; assume no default timeouts; assume stores are single-process.
5. _Record honestly_ — deferrals and missed targets are documented, never claimed.
