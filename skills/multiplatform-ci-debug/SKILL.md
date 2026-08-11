---
name: multiplatform-ci-debug
description: Use when debugging a CI workflow that builds/tests on multiple operating systems (Linux + Windows + macOS runners), or when a platform-specific job fails with cryptic build errors. Triggers - CI fails on one OS, workflow debugging, GitHub Actions Windows job, MSB8066, cargokit build failure, runner infrastructure failure, Actions quota, private repo minutes, flutter build windows verbose, platform-specific path/env differences, flaky CI test timeout.
---

# Multi-platform CI workflow debugging

Lessons from making a 3-OS GitHub Actions workflow (Linux + Windows + macOS) go green for
**my-croc** (WordDrop) — **20 CI runs and 14 fix commits** (5 explicit `fix(...)`, the rest
test/ci/debug commits) before first green, with 7 distinct root causes corrected along the
way. The core discipline:
_distinguish infra failures from code failures_, _surface the swallowed error_, and
_fix one root cause per commit_.

**Leading words:** _triage first_, _surface the error_, _one root cause per commit_, _push early_.

## When to use
- A CI job fails on one OS but passes on others (or fails at runner start).
- A native build step (cargokit/CMake/MSBuild) dies with a terse code and no message.
- You are setting up or expanding a multi-OS workflow and want to avoid the same 7 traps.

## Core discipline

### 1. Push early, verify in real CI
Nothing is "CI-tested" until it has run on the actual runner images. Local builds on a Linux
host prove NOTHING about Windows/macOS builds. Record `UNVERIFIED-UNTIL-PUSH` in evidence and
push as soon as the workflow file exists — the first real run is where the platform bugs live.

### 2. Triage before debugging: infra failure vs code failure
- **Infra failure signature:** ALL jobs fail with EMPTY logs (no step ran) or fail at
  "Set up job" with no failed step. Causes: private-repo Actions quota exhausted (2000
  min/month — jobs fail at runner start; switch repo to PUBLIC for unlimited minutes),
  runner infrastructure hiccup (retry once, then look at quota), log-blob unavailable
  (retry — often transient).
- **Code failure signature:** a specific step fails with a specific error. This is the
  only kind worth debugging as code.
- **Action:** classify FIRST, before touching any source. A quota failure is not a bug.

### 3. Surface the swallowed error
Native build wrappers (MSBuild/CMake/cargokit) often report a terse code (e.g. `MSB8066:
Custom build exited with code -1`) and swallow the real error. Techniques, in order:
- **Run the failing tool verbose:** `flutter build windows --release --verbose` exposed the
  real `PathNotFoundException` that plain `flutter build` hid inside MSB8066.
- **Replicate the tool call manually in a CI debug step:** run the underlying build-tool
  command with the SAME environment variables (CARGOKIT_*, etc.) and `2>&1 | Tee-Object`
  to a log, then print the log in a follow-up step. This turned a silent failure into a
  full Dart stack trace.
- Check the docs of the tool for its error-formatting switch before grepping build logs.

### 4. One root cause per commit
The my-croc Windows job had FOUR distinct causes that all surfaced as the same terse
failure (MSB8066 and friends), one after another: relay-binary path lookup, IPv6 V6ONLY
bind, MSVC linker OOM, and a port-rebind race — and the wider 20-run saga had 7 root
causes total including the cargokit output-dir and manifest-symlink issues. Fix ONE, push,
re-run — do not batch changes, or you cannot tell what fixed what.
Each fix is its own commit with the symptom + root cause in the message.

## Failure → Diagnosis → Fix catalog

### Windows-specific (runner images differ from Linux/macOS)
- **Symptom:** test helper can't find a binary on Windows.
  **Root cause:** Windows runners have no `HOME` (use `USERPROFILE`), and `cargo install`
  writes `.exe`-suffixed binaries. A `~/.cargo/bin/<name>` check always misses.
  **Fix:** resolve `HOME` else `USERPROFILE`, and add the `.exe` suffix under `cfg!(windows)`.

- **Symptom:** relay/server binds `[::]:port` and a `127.0.0.1` readiness probe never succeeds
  — only on Windows.
  **Root cause:** Windows IPv6 sockets default to V6ONLY, so an IPv6-only bind is NOT reachable
  via IPv4 loopback.
  **Fix:** write a config pinning the bind address to `0.0.0.0:<port>` and pass `--config-path`.

- **Symptom:** MSVC link fails with `MSB8066 ... exited with code -1` on a large native build.
  **Root cause:** `ThinLTO + codegen-units=1` over a large dependency tree OOMs the MSVC linker
  on resource-limited runners.
  **Fix:** relax the release profile for that target: `codegen-units = 8`, `lto = false`
  (keep `opt-level = "z"`, `strip = "symbols"` for size).

- **Symptom:** a test that binds an ephemeral port then rebinds it fails intermittently on
  Windows only.
  **Root cause:** TCP TIME_WAIT: a just-closed port can reject immediate rebinding on Windows.
  **Fix:** bind ONCE and pass the listener into the server (serve_on(listener) pattern) —
  eliminate the bind/drop/rebind entirely.

- **Symptom:** end-to-end test flakily times out on Windows CI but passes locally.
  **Root cause:** slow runners; the first endpoints racing a freshly spawned relay can exceed a
  15s online-wait.
  **Fix:** match the wait bound to the service-start timeout (15s → 30s).

### cargokit / Flutter-Rust bridge specifics
- **Symptom:** cargokit build fails "Cannot open file ... Cargo.toml" only on Windows.
  **Root cause:** cargokit computes the manifest path from `CMAKE_CURRENT_SOURCE_DIR`, which
  keeps Flutter's `.plugin_symlinks/<plugin>` path on Windows; the relative `../../rust`
  resolves to the WRONG directory (symlink not followed).
  **Fix:** resolve the plugin dir to its REAL path first (`get_filename_component(... REALPATH)`)
  before computing the manifest path — path the manifest through the real cmake root.

- **Symptom:** cargokit build_tool fails copying the built dylib: `PathNotFoundException`
  (directory does not exist).
  **Root cause:** the CMake `$<CONFIG>` output directory may not exist yet when the custom
  command runs ahead of the target that creates it.
  **Fix:** create the output directory (`createSync(recursive: true)`) before copying.

- **Symptom:** `Unknown toolchain` / version-pin attempt rejected by cargokit.
  **Root cause:** cargokit's Toolchain enum only accepts `stable|beta|nightly`; it cannot pin a
  specific version like "1.97.1". Adding a custom `toolchain:` to cargokit.yaml throws.
  **Fix:** either use `stable` and let rust-toolchain.toml rule the workspace, or pre-install
  the channel. Do not fight the enum.

### Cross-cutting
- **Symptom:** artifact or toolchain works locally, missing on the runner.
  **Root cause:** runner images differ per OS AND over time; nothing you install locally
  persists there.
  **Fix:** cache per-OS (`actions/cache` keyed on runner OS + version pin); install required
  tools explicitly in the workflow (e.g. dtolnay/rust-toolchain with a pinned version).

## Principles (the short version)
1. _Triage first_ — infra failure (empty logs, quota) is not a code bug; retry/quota before debugging.
2. _Surface the error_ — run the failing tool verbose or replicate its call with full output capture.
3. _One root cause per commit_ — same terse error can have multiple causes; fix, push, observe each.
4. _Push early_ — platform bugs only surface on real runner images; say UNVERIFIED-UNTIL-PUSH.
5. Windows ≠ Linux: check HOME/USERPROFILE, .exe suffix, IPv6 V6ONLY, TIME_WAIT, MSVC memory.
