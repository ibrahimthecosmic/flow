# User workers — the host API

In any flow main isolate (`flow run`, `eval`, `repl`, `test`, …) the global
`FlowRuntime` object exposes the user-worker pool:

```ts
FlowRuntime.userWorkers.create(options): Promise<UserWorker>
FlowRuntime.userWorkers.tryCleanupIdleWorkers(timeoutMs): Promise<number>
```

A `UserWorker` handle:

```ts
worker.key; // string — unique worker id (UUID) in the pool
worker.port; // MessagePort — duplex channel to the worker
worker.inspect(); // string — DevTools WebSocket URL (throws if inspector off)
```

## Creating a worker

```ts
const worker = await FlowRuntime.userWorkers.create({
  servicePath: "./service",
  envVars: [["API_TOKEN", "s3cr3t"]],
  memoryLimitMb: 256,
  cpuTimeSoftLimitMs: 1000,
  cpuTimeHardLimitMs: 2000,
});
```

`servicePath` must be a **directory** containing an entrypoint named `index.ts`,
`index.tsx`, `index.js`, `index.mjs`, or `index.jsx`. `create()` resolves once
the worker has booted and evaluated its entrypoint; it rejects if boot fails
(bad path, module error, …) — the pool stays healthy and later `create()` calls
work normally.

### Options

Source of the worker:

| Option                                      | Type                                                 | Notes                                                                                                |
| ------------------------------------------- | ---------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `servicePath`                               | `string`                                             | Directory containing `index.{ts,tsx,js,mjs,jsx}`                                                     |
| `maybeEszip` (+ optional `maybeEntrypoint`) | `Uint8Array \| string \| ReadableStream<Uint8Array>` | Boot from an eszip artifact (see [below](#booting-from-an-eszip-maybeeszip)); `servicePath` optional |
| `maybeModuleCode`                           | `string`                                             | Inline module source; still needs a `servicePath` (pool key / base directory)                        |

Resource limits (per worker):

| Option                | Default                                     | Meaning                                                           |
| --------------------- | ------------------------------------------- | ----------------------------------------------------------------- |
| `memoryLimitMb`       | `FLOW_USER_WORKER_MAX_HEAP_SIZE_MIB` or 512 | V8 heap cap; the worker is terminated when it exceeds it          |
| `lowMemoryMultiplier` | 5                                           | Low-memory notification factor                                    |
| `workerTimeoutMs`     | 60000                                       | Wall-clock lifetime; the supervisor retires the worker after this |
| `cpuTimeSoftLimitMs`  | 50                                          | CPU budget before the worker is flagged for early termination     |
| `cpuTimeHardLimitMs`  | 100                                         | CPU budget before the worker is forcibly killed                   |

> The CPU defaults are tuned for small request handlers. Module-heavy startup
> (e.g. importing many `node:*`/npm modules) can exceed 50 ms of CPU in debug
> builds — raise the limits for development:
> `{ cpuTimeSoftLimitMs: 5000, cpuTimeHardLimitMs: 10000 }`.

Environment & module loading:

| Option               | Default | Meaning                                                                                                              |
| -------------------- | ------- | -------------------------------------------------------------------------------------------------------------------- |
| `envVars`            | `[]`    | `[key, value]` pairs — the worker's **entire** environment (`Deno.env` / `process.env`). Host env is never inherited |
| `noModuleCache`      | `false` | Bypass the local module cache                                                                                        |
| `noNpm`              | unset   | Disable npm support for this worker                                                                                  |
| `allowRemoteModules` | `true`  | Allow `https:` imports                                                                                               |
| `customModuleRoot`   | unset   | Root for module resolution                                                                                           |
| `staticPatterns`     | `[]`    | Glob patterns of static files available to the worker                                                                |

Sandbox & platform:

| Option                      | Default              | Meaning                                                                                                                                                                                                                                                                                                                                                        |
| --------------------------- | -------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `allowHostFsAccess`         | `false`              | When `true`, the worker's `Deno` fs APIs work against the real filesystem; otherwise they are denied (see [worker-runtime.md](./worker-runtime.md#sandbox-behavior))                                                                                                                                                                                           |
| `permissions`               | all-allowed defaults | Deno-style permission set; keys are snake_case: `allow_all`, `allow_env`, `deny_env`, `allow_net`, `deny_net`, `allow_read`, `deny_read`, `allow_write`, `deny_write`, `allow_run`, `deny_run`, `allow_sys`, `deny_sys`, `allow_ffi`, `deny_ffi`, `allow_import`                                                                                               |
| `forceCreate`               | `false`              | Never reuse a running worker for this `servicePath`                                                                                                                                                                                                                                                                                                            |
| `context`                   | unset                | Arbitrary JSON merged into the worker's bootstrap context; the worker reads it back via the deep-frozen `FlowRuntime.context` global (runtime-owned keys such as `terminationRequestToken` are stripped)                                                                                                                                                       |
| `s3FsConfig`, `tmpFsConfig` | unset                | Alternative filesystem backends (S3 / temp fs). `s3FsConfig` takes one config object (mounted at `/s3`) or an array of config objects, each with its own `mountPoint` (default `/s3`); mount points must be absolute, non-`/`, and must not equal or nest inside `/tmp` or one another                                                                         |
| `httpFs`                    | unset                | HttpFS mounts: one config or an array of `{ mountPoint, baseUrl, headers?, query?, socketPath? }`, each backed by an HTTP API implementing the [HttpFS Protocol v1](./httpfs-protocol.md). `mountPoint` is required per entry and follows the same collision rules as the S3 mount points. `socketPath` routes the mount over an AF_UNIX socket instead of TCP |
| `otelConfig`                | unset                | OpenTelemetry tracing/metrics for the worker                                                                                                                                                                                                                                                                                                                   |

## Booting from an eszip: `maybeEszip`

An eszip (built with
[`flow eszip bundle`](./cli.md#flow-eszip--deployment-artifacts) or
`FlowRuntime.bundle`) carries the worker's entire module graph — code, npm
packages, static assets, metadata — as one artifact. `maybeEszip` accepts three
forms:

```ts
// 1. bytes — e.g. just downloaded from object storage
await FlowRuntime.userWorkers.create({ maybeEszip: eszipBytes });

// 2. a path — an .eszip file already on disk, used in place
await FlowRuntime.userWorkers.create({ maybeEszip: "./service.eszip" });

// 3. a stream — spilled to disk incrementally, chunk by chunk
const { body } = await fetch("https://artifacts.example.com/service.eszip");
await FlowRuntime.userWorkers.create({ maybeEszip: body });
```

`servicePath` is optional for eszip boots (the artifact carries its own
entrypoint in its metadata); pass `maybeEntrypoint` to override it.

### File-backed loading

Whatever the input form, the bundle is served **from disk, not from memory**:

- Only the archive **header** (module index, npm-resolution snapshot, metadata)
  is parsed into memory. Module sources, npm-package files, and static assets
  are read with positional reads (`pread`) when — and each time — they are
  needed, then dropped. The OS page cache is the only cache.
- A worker's resident memory therefore scales with its **touched working set**,
  not with the bundle size: booting from a 50 MiB bundle adds a few MiB of RSS,
  where prior flow versions kept the entire bundle (plus every touched module,
  permanently) resident per bundle.
- **Streams never materialize in memory at all**: each chunk is hashed and
  appended to a temp file, which makes `ReadableStream` the right form for large
  artifacts fetched over the network.
- Cold-start cost is unchanged (within noise of an in-memory boot).

### The bundle cache

`Uint8Array` and `ReadableStream` inputs are spilled into a **content-addressed
cache directory**; `string` paths are used in place and bypass the cache
entirely.

| Environment variable         | Default                 | Meaning                                      |
| ---------------------------- | ----------------------- | -------------------------------------------- |
| `FLOW_BUNDLE_CACHE_DIR`      | `<tmpdir>/flow-bundles` | Where spilled bundles land                   |
| `FLOW_BUNDLE_CACHE_TTL_SECS` | `604800` (7 days)       | Age (by mtime) after which entries are swept |

Behaviors:

- Entries are named `<xxh3-64-of-content>.eszip`, so **identical bundles
  converge on a single file** no matter how many times (or from how many
  concurrent `create()` calls) they are submitted. A cache hit just refreshes
  the file's mtime.
- Writes are atomic (temp file + rename); a crashed or aborted spill leaves only
  a `*.tmp` file that the sweep removes after an hour.
- The sweep runs at most once per process, on first cache use, and unlinks
  `*.eszip` entries older than the TTL. Deleting a cache entry out from under a
  **running** worker is harmless on Unix — the worker holds an open file handle
  — but a later `create()` with the same bytes rewrites it.

### Sharing across workers

Bundles are deduplicated by **canonicalized path**: every concurrent or later
`create()` for the same `.eszip` file shares one parsed header and one file
handle (each worker gets its own copy of the npm-resolution snapshot). The
shared parse is dropped when the last worker using it goes away. If the file is
**replaced on disk** (different size/mtime/inode), the next `create()` reparses
it — but note the pool may still hand you the already-running worker for the
same pool key; use `forceCreate: true` to boot the new artifact.

### Integrity checking

If the bundle was built with a checksum
(`flow eszip bundle --checksum xxhash3 | sha256`), every whole-entry read —
module sources, source maps, static assets, whole-file npm reads — is verified
against its stored hash, on every read. A corrupted extent fails the worker's
module init with `invalid source hash for <specifier>` (see failure surfacing
below). Partial (ranged) reads inside npm-package files skip verification.
Bundles built without a checksum are not verified; prefer checksummed bundles
for artifacts that cross a network or shared storage.

### Old formats

Only current-format flow eszips (version `2.0`) can boot workers. Archives
produced by older flow/edge-runtime versions (v0, v1, v1.1) are rejected at
`create()` with an error asking you to re-bundle:

```
this eszip uses an unsupported format for file-backed loading; re-bundle it
with `flow eszip bundle` (old bundles can still be unpacked with `flow eszip
unbundle`)
```

`flow eszip unbundle` (and `FlowRuntime.unbundle`) still read old formats, so a
re-bundle is always possible: unbundle → bundle.

### Failure surfacing

`create()` itself rejects on malformed input: an unreadable path, a truncated or
non-eszip file, an old format, or passing both bytes and a path. Failures inside
the module graph (a corrupted extent caught by the checksum, a missing module)
happen while the freshly booted worker initializes its entrypoint, so `create()`
still resolves — the failure follows on
[`FlowRuntime.events`](#observing-workers-flowruntimeevents) as a `BootFailure`
event (right after the worker's `Boot` event), and the worker never serves:
messages posted to its port go unanswered.

```ts
for await (const ev of FlowRuntime.events) {
  if (ev.event_type === "BootFailure") {
    // e.g. "worker boot error: failed to read module source for
    //       file:///src/index.ts: invalid source hash for file:///src/index.ts"
    console.error("worker failed to start:", ev.event.msg);
  }
}
```

## Talking to a worker: `worker.port`

`worker.port` is a standard `MessagePort` — duplex, structured clone:

```ts
// host
const worker = await FlowRuntime.userWorkers.create({ servicePath: "./svc" });
worker.port.onmessage = (e) => console.log("reply:", e.data);
worker.port.postMessage({ op: "sum", nums: [1, 2, 3] });
```

```ts
// svc/index.ts (worker)
FlowRuntime.parentPort.onmessage = (e) => {
  if (e.data.op === "sum") {
    const sum = e.data.nums.reduce((a: number, b: number) => a + b, 0);
    FlowRuntime.parentPort.postMessage({ sum });
  }
};
```

Messages queue inside the port until a handler is attached on the other side, so
there is no boot-time race: you can `postMessage` immediately after `create()`
resolves.

### Raw bytes: transferable `ArrayBuffer`s

The port supports the standard transfer list. A transferred `ArrayBuffer` moves
its backing store **zero-copy** across the isolate boundary (the sender side is
detached), which is the intended path for large binary payloads:

```ts
// host — send 32 MiB without copying
const bytes = new Uint8Array(32 * 1024 * 1024);
worker.port.postMessage({ buf: bytes.buffer }, [bytes.buffer]);
console.log(bytes.buffer.byteLength); // 0 — detached, ownership moved
```

```ts
// worker — receive, process, transfer a result back
FlowRuntime.parentPort.onmessage = (e) => {
  const view = new Uint8Array(e.data.buf);
  const out = process(view); // Uint8Array
  FlowRuntime.parentPort.postMessage({ buf: out.buffer }, [out.buffer]);
};
```

Without a transfer list the buffer is copied (sender keeps its data) — normal
structured-clone semantics.

> **SharedArrayBuffer caveat**: the host _can_ post a `SharedArrayBuffer`
> through the port, creating genuinely shared memory with the worker. Worker
> code cannot create one itself (its `SharedArrayBuffer` global is aliased to
> `ArrayBuffer`), and shared memory is not attributed to the worker's memory
> limit — only do this deliberately.

## Worker reuse

Under the default `per_worker` policy, calling `create()` again with the same
`servicePath` returns a handle to the **already-running** worker (`worker.key`
is identical) — but with its **own, live port**. The running worker is handed
the new channel SharedWorker-style:

```ts
// host
const a = await FlowRuntime.userWorkers.create({ servicePath: "./svc" });
const b = await FlowRuntime.userWorkers.create({ servicePath: "./svc" });
console.log(a.key === b.key); // true  — same worker
console.log(b.port !== null); // true  — but its own channel
```

```ts
// svc/index.ts — accept extra connections
FlowRuntime.parentPort.onmessage = handle; // first connection

FlowRuntime.onparentport = (port: MessagePort) => {
  port.onmessage = handle; // each reused create() delivers a new port
};
```

Details:

- All ports delivered to a worker (including the first) are collected in
  `FlowRuntime.parentPorts`.
- Ports queue messages until a handler is attached, so a service that sets
  `onparentport` late does not lose messages.
- `worker.port` is `null` only in one rare race: the pool answered with a worker
  that was torn down before the new channel could be delivered.
- `forceCreate: true` (or the `oneshot` policy) always spawns a fresh worker.

## Cleaning up idle workers

```ts
// Tear down every worker with no in-flight work right now (timeoutMs is
// how long to wait for each worker to acknowledge, not an idle-age
// threshold). Resolves with the number of workers that acknowledged.
const cleaned = await FlowRuntime.userWorkers.tryCleanupIdleWorkers(30_000);
```

## Observing workers: `FlowRuntime.events`

`FlowRuntime.events` is an async iterable over every worker's lifecycle and
console output — the event stream edge/trex fed to their dedicated "events
worker" (removed in flow), collapsed into a host API on the main isolate:

```ts
for await (const ev of FlowRuntime.events) {
  // ev = { timestamp, event_type, event, metadata }
  switch (ev.event_type) {
    case "Log": // every console.* call in a worker: { msg, level }
    case "Boot": // { boot_time }
    case "BootFailure": // { msg }
    case "UncaughtException": // { exception, cpu_time_used }
    case "Shutdown": // { reason, cpu_time_used, memory_used }
  }
  // ev.metadata = { service_path, execution_id, ... } — route per tenant
}
```

Semantics:

- **stdio-inherit until claimed.** While nobody iterates, worker output behaves
  like a Node child with `stdio: "inherit"`: `console.log`/`info`/`debug` land
  on flow's stdout, `console.warn`/`error` and uncaught exceptions on stderr.
  `Boot`/`Shutdown` telemetry prints nothing (visible via `DENO_LOG=debug`).
- **Claiming.** The first `next()` (i.e. entering the `for await`) claims the
  stream; from then on events go to the iterator instead of stdio.
- **Single consumer.** A second concurrent iteration rejects with a claim error.
  Fan out in JS if you need multiple readers.
- **Releasing.** Breaking out of the loop (or calling `return()` on the
  iterator) hands the stream back — stdio-inherit resumes. The iterator ends
  (`done`) when the worker pool shuts down.

The stream is claimed on the main isolate. To do heavy per-tenant processing
(batching, shipping logs to third parties) without loading the main event loop,
relay events into a plain Web Worker — they are structured-clone-safe:

```ts
const shipper = new Worker(import.meta.resolve("./shipper.ts"), {
  type: "module",
});
for await (const ev of FlowRuntime.events) shipper.postMessage(ev);
```

## Debugging workers

Start flow with the user-worker inspector enabled:

```console
$ flow run -A --user-worker-inspect 127.0.0.1:9230 main.ts
```

Each worker registers as a distinct DevTools target. `worker.inspect()` returns
the WebSocket URL for **that** worker:

```ts
const worker = await FlowRuntime.userWorkers.create({ servicePath: "./svc" });
console.log(worker.inspect());
// ws://127.0.0.1:9230/ws/8b3f…  — open in chrome://inspect / DevTools
```

The targets are also listed on `http://127.0.0.1:9230/json/list`. If the
inspector was not enabled, `inspect()` throws with instructions.

The **main isolate** is debugged like any Deno process, with Deno's own
`--inspect`/`--inspect-brk` — the two inspectors are independent and can run
side by side on different ports.
