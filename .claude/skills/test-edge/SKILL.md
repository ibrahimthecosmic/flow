---
name: test-edge
description: Run the edge-layer test suites (base lib + integration, fs, flow specs) correctly on this machine. Use when verifying edge/ changes, before merging an edge branch, or when asked to "run the edge tests".
user-invocable: true
---

# Run the edge test suites

Four suites cover `edge/`. Run them ALL for runtime changes — `cargo check`
proves nothing about module-graph reachability, config-by-presence files, or
fixture wiring (the dead-code sweep shipped 4 runtime bugs that only these
suites caught).

Every cargo invocation on this machine: `CARGO_PROFILE_DEV_DEBUG=line-tables-only`
and `-j 2` (13GB RAM box; uncapped builds have OOM-crashed it — see the
avoid-heavy-builds memory).

## 1. base unit tests (~1 min run after ~2 min build)

```sh
CARGO_PROFILE_DEV_DEBUG=line-tables-only cargo test -j 2 -p base --lib -- --test-threads=2
```

**Known red (since the 2.9.0 port, verified identical on pre-sweep main,
2026-07-11):** `runtime::test::test_entrypoint_resolution` — eszip relative
base = discovered workspace root, so `main_module_url` loses path segments.
35/36 = green in practice. If OTHER tests fail, that's a real regression.

## 2. base integration tests (~1 min run)

```sh
CARGO_PROFILE_DEV_DEBUG=line-tables-only cargo test -j 2 -p base --test integration_tests -- --test-threads=2
```

**Run this target EXPLICITLY.** A bare `cargo test -p base` stops after the
lib target's known failure and silently skips the integration binary — it went
unexecuted for the sweep's whole lifetime this way. Expected: 18/18.

The `TestWorker` harness observes workers via the events channel and panics
with every event it saw — read that list before theorizing.

## 3. fs tests (~1 min)

```sh
CARGO_PROFILE_DEV_DEBUG=line-tables-only cargo test -j 2 -p fs
```

Green = 2×23 httpfs conformance (TCP + AF_UNIX) pass; the 7
`integration_tests` are `#[cfg_attr(not(dotenv), ignore)]`, gated on real S3
creds in `edge/crates/fs/tests/.env` — 7 ignored locally is correct, not a
regression.

## 4. flow spec tests (needs binaries first)

```sh
CARGO_PROFILE_DEV_DEBUG=line-tables-only cargo build -j 2                                  # flow binary (~10 min cold, ~2 min warm)
CARGO_PROFILE_DEV_DEBUG=line-tables-only cargo build -j 2 -p test_server --bin test_server
CARGO_PROFILE_DEV_DEBUG=line-tables-only cargo test  -j 2 -p specs_tests flow
```

Traps:
- **Wrong filter silently passes.** `spec::flow` matches zero tests and exits
  0 with no output. The working filter is plain `flow`. Confirm you see
  "N tests passed" in the output.
- **Missing binaries panic** with "Test server not found. Please cargo build".
- Expected: 6 passed (user_workers ×2, eszip_api ×2, types, +1).
- To iterate on a spec driver without the harness, run it directly — much
  faster: `cd tests/specs/flow/user_workers && ../../../../target/debug/flow run -A surface.js`.

## Long-running builds: session-teardown discipline

Background jobs — even `setsid nohup ... & disown` — are killed when the
Claude Code session ends (the sandbox kills the process group; this killed
mid-link builds 5+ times across sessions). Rules:

- Prefer a **synchronous** Bash call with `timeout: 600000` for anything ≤10
  min (every suite above fits when warm).
- If detaching anyway: `dangerouslyDisableSandbox: true`, redirect to a log
  file, append `echo "CARGO_EXIT=$?"` and `touch DONE` — the harness's own
  "exit code 0" only reflects the trailing echo. Never pipe a background build
  through `tail`; nothing is written until it exits.
- Cargo resumes incrementally after a kill; just relaunch.

## Tooling needs `deno` on PATH

`./x`, `tools/format.js`, and `tools/lint.js` shell out to `deno`, which is
not installed. Symlink the built binary once per session:

```sh
ln -sf "$PWD/target/debug/flow" "$SCRATCHPAD/bin/deno"
PATH="$SCRATCHPAD/bin:$PATH" ./target/debug/flow run -A tools/format.js
```

## When the suites disagree with you

Fix root causes, not assertions. Precedent: fixtures pinned busy with
`FlowRuntime.waitUntil` because idle workers are early-dropped by design
(see /debug-user-workers for the semantics), NOT by weakening the expected
shutdown reasons.
