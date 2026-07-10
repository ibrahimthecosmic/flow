---
name: debug-user-workers
description: Diagnose flow user-worker failures fast — failure-signature lookup (throw undefined, unreachable esm, ASCII assert, Module not found, EarlyDrop kills, hangs) plus a probe/bisect technique using FlowRuntime.events and maybeModuleCode.
user-invocable: true
---

# Debugging flow user workers

Every signature below cost real hours once. Check the table before reading
runtime code.

## Failure signatures → root causes

**JS caught `undefined` instead of an error (`throw undefined`)**
An op errored with a JS error class that the worker never registered.
deno_core's `buildCustomError` returns undefined for unknown class names.
Every class surfaced to workers must be in
`edge/ext/runtime/js/errors.js` (`core.registerErrorClass`). Rust side: the
`#[class("...")]` attr on the error enum variant is the class name. Precedent:
`RuntimeError::Runtime` → class "Runtime" (unregistered) made `Deno.listen()`
throw undefined; fixed by throwing `JsErrorBox::new("NotSupported", ...)`.

**`Failed to initialize a JsRuntime: Following modules were not evaluated ... ext:X`**
A module declared in an extension's `esm = [...]` list is not reachable by
import from its `esm_entry_point`. Don't just delete it from the list — check
what side effects it has first (e.g. `ext:os/exit.js` publishes
`internals.flowOsExit` for the `30_os.js` lazy *script*, which cannot import
ESM). Fix: a side-effect `import "ext:X";` from the entry point.

**Panic `buffer.is_ascii() && ...` in rusty_v8 `string.rs` at worker boot**
An embedded extension source contains a non-ASCII byte (usually an em dash in
a comment). deno_core hands extension sources to v8 as external one-byte
strings and asserts 7-bit ASCII on EVERY source variant — there is no
non-ASCII fallback, don't build one. `edge/crates/base/build.rs`
(`assert_ascii_extension_source`) fails the build with file:line; if you see
the v8 assert instead, the guard was bypassed or the binary is stale.

**`Module not found: file:///var/tmp/shared/...` (or any /var/tmp path)**
The import resolved outside the eszip's relative base. The base is the
workspace root Deno DISCOVERS by walking up from the entrypoint looking for
deno.json(c)/package.json (`generate_binary_eszip` in
`edge/crates/deno_facade/eszip/mod.rs`). `edge/crates/base/test_cases/deno.jsonc`
exists solely to anchor discovery for fixtures — an empty config file like it
is NOT dead code. `/var/tmp/sb-compile-trex/` is the runtime's virtual root
(`module_loader/standalone.rs`).

**Worker dies seconds into a test / shutdown reason is `EarlyDrop`**
Two defaults + one design rule:
- Pool defaults: CPU soft/hard = 50/100 ms, wall clock 60 s. Test drivers must
  pass generous `cpuTimeSoftLimitMs`/`cpuTimeHardLimitMs`/`workerTimeoutMs`.
- Design: an **idle** worker (all reqs acked, all `waitUntil` promises
  resolved) is gracefully retired with reason `EarlyDrop` at the FIRST
  resource alert — wall-clock T/2, CPU soft, or memory-half. The hard reasons
  (`WallClockTime` at T, `CPUTime` at hard, `Memory`) only fire while busy.
- "Busy" means `FlowRuntime.waitUntil`-registered pending promises —
  PromiseMetrics does NOT count arbitrary pending promises or timers. To pin a
  fixture busy: `FlowRuntime.waitUntil(new Promise(() => {}))`; release the
  pin when done if the test later host-terminates (waitUntil blocks graceful
  termination by design).
Supervisor logic: `edge/crates/base/src/worker/supervisor/strategy_per_worker.rs`.

**`scheduleTermination()` / termination behavior**
Worker-side `FlowRuntime.scheduleTermination()` cancels the
termination-request token; the supervisor's graceful arm picks it up →
shutdown reason `TerminationRequested` (same as a host-side
`TerminationToken`). If it hangs again, check the `Tokens::termination_request`
arm in strategy_per_worker.rs.

**`TerminationToken::cancel_and_wait()` hangs**
The `outbound` ack: the pool acks pooled workers when processing the Shutdown
message; for direct `WorkerSurface` workers the supervisor acks after runtime
disposal (end of `create_supervisor`'s task in `supervisor/mod.rs`). If it
hangs, one of those paths regressed.

**Events channel closed before an expected event**
The event never matched — read the harness panic's "saw:" list (the
integration TestWorker dumps every observed event). Usually a wrong expected
shutdown reason (see EarlyDrop above) or the worker crashed earlier
(UncaughtException event carries the real error).

## Probe technique: drive workers from a host script

Fastest loop for worker behavior (no cargo, seconds per iteration) — run a
host script with the built binary:

```js
// probe.js — run: ./target/debug/flow run -A probe.js
(async () => {
  for await (const ev of FlowRuntime.events) {
    if (ev.event_type === "Log") console.log("WLOG:", ev.event.msg.trim());
    if (ev.event_type === "UncaughtException") console.log("WEXC:", ev.event.exception);
    if (ev.event_type === "Shutdown") console.log("WSHUT:", ev.event.reason);
  }
})();
await FlowRuntime.userWorkers.create({
  servicePath: "./some-fixture-dir",        // or a key + maybeModuleCode
  cpuTimeSoftLimitMs: 10_000, cpuTimeHardLimitMs: 20_000, workerTimeoutMs: 30_000,
  maybeModuleCode: `console.log("probe:", typeof Deno.serve);`, // optional
});
setTimeout(() => Deno.exit(0), 8_000);
```

- Bisect a failing fixture by pasting shrinking variants into
  `maybeModuleCode` (servicePath stays the pool key; it need not match the
  code).
- Worker `console.log` goes to the **events stream** (Log events), never to
  the flow process stdout. `ev.metadata.execution_id` is the worker key.
- Don't trust JS stack line numbers from transpiled fixtures blindly — the
  frame can point at the wrong statement. Bisect instead (a "line 15" frame
  once pointed at `Deno.serve` when the thrower was `Deno.listen` two calls
  later).

## Layout of the worker machinery

- `edge/crates/base/src/runtime/mod.rs` — DenoRuntime::new (eszip build,
  bootstrap context incl. `terminationRequestToken`), run_event_loop.
- `edge/crates/base/src/worker/` — pool.rs (keyed by servicePath; forceCreate
  boots a distinct worker), worker_inner.rs (thread + event send),
  supervisor/{mod,strategy_per_worker}.rs (limits, shutdown reasons, acks).
- `edge/ext/runtime/js/` — bootstrap.js (Deno surface assembly, denied APIs),
  denoOverrides.js (serve/listen/fs surface), namespaces.js (FlowRuntime),
  errors.js (registered error classes).
- `edge/ext/workers/lib.rs` — op_user_worker_create options
  (`UserWorkerCreateOptions`, camelCase serde) — the source of truth for
  `create()` option names.
- `edge/ext/runtime/lib.flow.d.ts` — the declared API surface;
  `tests/specs/flow/user_workers/` exercises it end-to-end.
