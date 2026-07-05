# Inside a user worker

A user worker is a hardened Deno-like runtime: standard web platform APIs plus a
restricted `Deno` namespace, Node.js compatibility, and a few flow-specific
globals. This page documents what differs from plain Deno.

## Flow globals

### `FlowRuntime`

| Member                           | Meaning                                                                                                                                                                    |
| -------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `FlowRuntime.parentPort`         | `MessagePort` to the host handle that created this worker                                                                                                                  |
| `FlowRuntime.parentPorts`        | Array of every parent port delivered so far (first one included)                                                                                                           |
| `FlowRuntime.onparentport`       | Assignable callback `(port: MessagePort) => void`, invoked when a reused `create()` delivers an additional channel (see [user-workers.md](./user-workers.md#worker-reuse)) |
| `FlowRuntime.waitUntil(promise)` | Keep the worker alive until the promise settles (useful for background work after replying)                                                                                |

```ts
FlowRuntime.parentPort.onmessage = (e) => {
  const result = handle(e.data);
  FlowRuntime.parentPort.postMessage(result);
  // fire-and-forget work that must still complete:
  FlowRuntime.waitUntil(flushMetrics());
};
```

### Version globals

| Global         | Example                           |
| -------------- | --------------------------------- |
| `FLOW_VERSION` | flow's own version string         |
| `DENO_VERSION` | the Deno version flow is built on |

`Deno.version.deno` inside a worker reports
`flow-runtime-<flow> (compatible with Deno v<deno>)`, and the user agent is
`Deno/<deno> (variant; FlowRuntime/<flow>)`.

### `Trex`

A legacy compatibility alias (trex-runtime lineage) exposing `waitUntil`. Prefer
`FlowRuntime.waitUntil`.

## Sandbox behavior

User workers are designed to run untrusted code. Compared to plain Deno:

- **Filesystem**: unless the worker was created with `allowHostFsAccess: true`,
  the `Deno` file APIs (`open`, `stat`, `readFile`, `writeFile`, `mkdir`,
  `readDir`, `remove`, `cwd`, temp files, …) are denied. Module loading still
  works normally (it goes through the module loader, not these APIs).
- **Process control**: `Deno.exit`, `Deno.kill`, `Deno.addSignalListener`,
  `Deno.removeSignalListener` are mocked (no-ops) — a worker cannot take down or
  signal the host process.
- **Environment**: `Deno.env` (and Node's `process.env`) contain exactly the
  `envVars` passed to `create()` — nothing is inherited from the host.
- **Host info hiding**: `Deno.args` is `[]`, `Deno.pid`/`process.pid` are
  undefined, `os.hostname()` reports `localhost`, `os.loadavg()` reports zeros,
  CPU count reports 1.
- **No shared memory**: `SharedArrayBuffer` is aliased to `ArrayBuffer`, and
  creating shared WebAssembly memory
  (`new WebAssembly.Memory({ shared: true })`) throws. (The _host_ can still
  deliberately share memory over the port — see the caveat in
  [user-workers.md](./user-workers.md).)
- **No web workers**: workers cannot spawn nested workers.
- **Console**: worker `console.*` output is routed through the host process's
  logging (each line attributed to the worker), so worker logs appear in flow's
  output.

## Node.js / npm compatibility

Workers support the Node compatibility layer:

```ts
// service/index.ts
import { Buffer } from "node:buffer";
import { createHash } from "node:crypto";
import isEven from "npm:is-even";

FlowRuntime.parentPort.onmessage = (e) => {
  FlowRuntime.parentPort.postMessage({
    hash: createHash("sha256").update(String(e.data)).digest("hex"),
    b64: Buffer.from("flow").toString("base64"),
    even: isEven(42),
  });
};
```

- `node:*` builtins work via static or dynamic import.
- npm packages (CJS and ESM) work via `npm:` specifiers.
- The Node globals `process`, `Buffer`, `setImmediate`, and `clearImmediate` are
  installed lazily — touching them (or importing any `node:`/npm module)
  initializes the Node layer on demand, so workers that don't use Node pay
  nothing.
- **Native addons (N-API / `.node` files) are not supported** — requiring one
  fails with a clear error. This is a deliberate sandbox boundary.
- `process.env` mirrors the worker's `envVars`; `process.pid` is undefined (host
  info hiding).

> Importing a large `node:`/npm dependency graph costs CPU time at first use;
> with the tight default CPU limits (50 ms soft / 100 ms hard) this can
> terminate the worker mid-import. Raise `cpuTimeSoftLimitMs` /
> `cpuTimeHardLimitMs` in `create()` for Node-heavy services.

## Lifecycle, limits, and events

Each worker runs on a dedicated OS thread in its own V8 isolate, watched by a
supervisor enforcing the limits from `create()`:

| Limit                | Behavior when hit                                                                                                                                                     |
| -------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `memoryLimitMb`      | Heap is capped; a memory check (including WASM memories, and malloc'd memory when `FLOW_INCLUDE_MALLOCED_MEMORY_ON_MEMCHECK` is set) retires the worker when exceeded |
| `cpuTimeSoftLimitMs` | Worker is flagged for early termination (finishes current work, no new work)                                                                                          |
| `cpuTimeHardLimitMs` | Worker is forcibly terminated                                                                                                                                         |
| `workerTimeoutMs`    | Wall-clock retirement                                                                                                                                                 |

A terminated worker never harms the host: the main isolate and all other workers
keep running; the dead worker's ports go silent.

### Lifecycle events

Workers receive DOM events on `globalThis`:

- **`beforeunload`** — a `CustomEvent` dispatched _before_ the supervisor
  terminates the worker, when the `--dispatch-beforeunload-*-ratio` thresholds
  are configured (see [cli.md](./cli.md)). `event.detail.reason` is one of
  `"cpu"`, `"memory"`, `"wall_clock"`, `"early_drop"`, `"termination"`.
- **`unload`** — dispatched at termination.
- **`drain`** — dispatched when the pool asks the worker to wind down.

```ts
globalThis.addEventListener("beforeunload", (e) => {
  console.log("about to be retired:", (e as CustomEvent).detail?.reason);
  // last-chance cleanup / state flush
});
```

Example: get an early warning at 90% of the CPU budget:

```console
$ flow run -A --dispatch-beforeunload-cpu-ratio 90 main.ts
```

## A note on `export default { fetch }`

The worker runtime recognizes a default-exported fetch handler (edge-runtime
lineage) and registers it as a declarative server — but flow currently has **no
host-side HTTP ingress to workers**: the legacy request-passing ops are
deliberately not exposed in the main isolate. Communicate with workers over the
`MessagePort` channel instead — it needs no ports, no HTTP framing, and supports
zero-copy binary transfer.
