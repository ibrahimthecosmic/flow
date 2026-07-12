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

| Option            | Type                                                 | Notes                                                                                                                                                                                                                      |
| ----------------- | ---------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `servicePath`     | `string`                                             | Directory containing `index.{ts,tsx,js,mjs,jsx}`. Doubles as the **pool key** — under the default `per_worker` policy a later `create()` with the same path returns the running worker (see [Worker reuse](#worker-reuse)) |
| `maybeEszip`      | `Uint8Array \| string \| ReadableStream<Uint8Array>` | Boot from an eszip artifact (see [below](#booting-from-an-eszip-maybeeszip)); `servicePath` optional (kept only as the pool key)                                                                                           |
| `maybeEntrypoint` | `string`                                             | Entrypoint override for `servicePath` builds: a path resolved against `servicePath`, or a full URL. **Not** an override for eszip boots — a current-format bundle's own entrypoint key always wins                         |
| `maybeModuleCode` | `string`                                             | Inline module source; still needs a `servicePath` (pool key / base directory)                                                                                                                                              |

Resource limits (per worker):

| Option                | Default                                     | Meaning                                                                                                                         |
| --------------------- | ------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `memoryLimitMb`       | `FLOW_USER_WORKER_MAX_HEAP_SIZE_MIB` or 512 | V8 heap cap; the worker is terminated when it exceeds it                                                                        |
| `lowMemoryMultiplier` | 5                                           | Near-heap-limit allowance: V8's limit is raised `current × N` so the worker is retired gracefully instead of OOMing the process |
| `workerTimeoutMs`     | 60000                                       | Wall-clock lifetime; the supervisor retires the worker after this                                                               |
| `cpuTimeSoftLimitMs`  | 50                                          | CPU budget before the worker is flagged for early termination                                                                   |
| `cpuTimeHardLimitMs`  | 100                                         | CPU budget before the worker is forcibly killed                                                                                 |

> The CPU defaults are tuned for small request handlers. Module-heavy startup
> (e.g. importing many `node:*`/npm modules) can exceed 50 ms of CPU in debug
> builds — raise the limits for development:
> `{ cpuTimeSoftLimitMs: 5000, cpuTimeHardLimitMs: 10000 }`.

Environment & module loading:

| Option               | Default | Meaning                                                                                                              |
| -------------------- | ------- | -------------------------------------------------------------------------------------------------------------------- |
| `envVars`            | `[]`    | `[key, value]` pairs — the worker's **entire** environment (`Deno.env` / `process.env`). Host env is never inherited |
| `noModuleCache`      | `false` | Build the module graph without the local Deno module cache (remote imports are re-downloaded)                        |
| `noNpm`              | unset   | Forbid `npm:` resolution while building the module graph                                                             |
| `allowRemoteModules` | `true`  | Allow `https:` imports while building the module graph                                                               |
| `customModuleRoot`   | unset   | Accepted for edge-runtime compatibility; currently unused                                                            |
| `staticPatterns`     | `[]`    | Glob patterns (resolved against the host's CWD) of static files baked into the worker's sandbox filesystem           |

> The four graph-building options (`noModuleCache`, `noNpm`,
> `allowRemoteModules`, `staticPatterns`) apply to `servicePath` /
> `maybeModuleCode` builds only. An eszip boot never builds a graph — its
> modules, npm packages, and static assets were fixed at bundle time.

Sandbox & platform:

| Option                      | Default              | Meaning                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| --------------------------- | -------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `allowHostFsAccess`         | `false`              | When `true`, the worker's `Deno` fs APIs work against the real filesystem (and module loads that miss the graph/bundle may fall back to host files); otherwise they are denied (see [worker-runtime.md](./worker-runtime.md#sandbox-behavior)). Rejected when flow runs with `--restrict-host-fs`                                                                                                                                                                                                                                           |
| `permissions`               | all-allowed defaults | Deno-style permission set; keys are snake_case: `allow_env`, `deny_env`, `allow_net`, `deny_net`, `allow_read`, `deny_read`, `allow_write`, `deny_write`, `allow_run`, `deny_run`, `allow_sys`, `deny_sys`, `allow_ffi`, `deny_ffi`, `allow_import`. When the object is given, Deno flag semantics apply per key: omitted = denied, `[]` = blanket allow, non-empty = only the listed targets; `deny_*` carves exceptions out. No prompting — denials throw. (`allow_all` is accepted for edge-runtime compatibility but currently ignored) |
| `forceCreate`               | `false`              | Never reuse a running worker for this `servicePath`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `context`                   | unset                | Arbitrary JSON merged into the worker's bootstrap context; the worker reads it back via the deep-frozen `FlowRuntime.context` global (runtime-owned keys such as `terminationRequestToken` are stripped). A few keys are also read by the runtime itself — see [below](#runtime-recognized-context-keys)                                                                                                                                                                                                                                    |
| `s3FsConfig`, `tmpFsConfig` | unset                | Alternative filesystem backends (S3 / temp fs). `s3FsConfig` takes one config object (mounted at `/s3`) or an array of config objects, each with its own `mountPoint` (default `/s3`); mount points must be absolute, non-`/`, and must not equal or nest inside `/tmp` or one another. Full field reference (`credentials`, `endpointUrl`, `forcePathStyle`, `retryConfig`, …): `FlowS3FsConfig` / `FlowTmpFsConfig` in [`flow types`](./cli.md#flow-types)                                                                                |
| `httpFs`                    | unset                | HttpFS mounts: one config or an array of `{ mountPoint, baseUrl, headers?, query?, socketPath? }`, each backed by an HTTP API implementing the [HttpFS Protocol v1](./httpfs-protocol.md). `mountPoint` is required per entry and follows the same collision rules as the S3 mount points. `socketPath` routes the mount over an AF_UNIX socket instead of TCP                                                                                                                                                                              |
| `otelConfig`                | unset                | OpenTelemetry tracing/metrics for the worker: `{ tracing_enabled?, metrics_enabled?, console?: "Ignore" \| "Capture" \| "Replace", propagators?: ("TraceContext" \| "Baggage")[] }` — everything defaults to off. **Mind the casing**: unlike the other options these keys are snake_case and the values PascalCase; misspelled keys are silently ignored                                                                                                                                                                                   |

### Runtime-recognized `context` keys

`context` is passed through to the worker verbatim, but the runtime itself reads
a few keys out of it while booting:

| Key                     | Type      | Effect                                                                                                                                                                       |
| ----------------------- | --------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `importMapPath`         | `string`  | Import map applied when building the module graph of a `servicePath` build (and when migrating a legacy eszip). Current-format eszip boots carry their own map and ignore it |
| `unstableSloppyImports` | `boolean` | Enable sloppy imports for `servicePath` builds                                                                                                                               |
| `sourceMap`             | `boolean` | Load source maps so worker stack traces map back to original sources (implied when the user-worker inspector is enabled)                                                     |

```ts
await FlowRuntime.userWorkers.create({
  servicePath: "./svc",
  context: {
    importMapPath: "./svc/import_map.json",
    sourceMap: true,
    tenantId: "acme", // everything else is yours; the worker reads it back
  },
});
```

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

`servicePath` is optional for eszip boots: the artifact carries its own
entrypoint in its metadata, and that key always wins for current-format bundles
(`maybeEntrypoint` is only consulted for migrated legacy artifacts whose
metadata lacks one). When given, `servicePath` still serves as the pool key.

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

## Bundling programmatically: `FlowRuntime.bundle` / `FlowRuntime.unbundle`

The `flow eszip` CLI has a programmatic twin on the host isolate:

```ts
FlowRuntime.bundle(entrypoint, options?): ReadableStream<Uint8Array>
FlowRuntime.unbundle(eszip, output?): FlowUnbundled
```

### `FlowRuntime.bundle`

`entrypoint` is either a **path on disk** (string) or the entry module's
**source code** (`Uint8Array`/`ArrayBuffer`/`ReadableStream` — bundled under a
synthetic `/src/index.ts` specifier, imports resolved against the CWD). Bundling
runs on a dedicated thread; failures surface as errors on the returned stream.

```ts
// Bundle to a file…
const artifact = FlowRuntime.bundle("./service/index.ts", {
  checksum: "xxhash3",
  staticPatterns: ["./service/assets/**/*.html"],
});
const file = await Deno.open("service.eszip", { write: true, create: true });
await artifact.pipeTo(file.writable);

// …or straight into a worker (collect the stream, or spill it to disk first)
const worker = await FlowRuntime.userWorkers.create({
  maybeEszip: FlowRuntime.bundle("./service/index.ts"),
});
```

Options (`FlowBundleOptions`):

| Option           | Default | Meaning                                                                                                                                                                                                                                                                                       |
| ---------------- | ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `importMapPath`  | unset   | Path to an import map (resolved against the CWD) applied while building the module graph — and serialized into the artifact, so the same mappings hold inside a worker booted from it. Without it, the entrypoint's workspace config (`deno.json` `imports`, `package.json`) applies as usual |
| `staticPatterns` | `[]`    | Glob patterns (CWD-relative) of static files to include; same as `--static`                                                                                                                                                                                                                   |
| `checksum`       | none    | `"sha256"` \| `"xxhash3"` — bake per-entry integrity hashes into the artifact, verified on every read at boot/import time (see [Integrity checking](#integrity-checking))                                                                                                                     |
| `noModuleCache`  | `false` | Re-download remote modules instead of using the local module cache                                                                                                                                                                                                                            |
| `timeoutMs`      | none    | Abort bundling after this long (stream errors)                                                                                                                                                                                                                                                |
| `exclude`        | `[]`    | Specifiers or globs whose module subtree is left out of the bundle; same as `--exclude` (see [cli.md](./cli.md#flow-eszip-bundle))                                                                                                                                                            |

The import map is the piece the CLI does **not** expose — `flow eszip bundle`
relies on workspace discovery, so an explicit map requires this API:

```ts
// import_map.json: { "imports": { "#lib/": "./shared/lib/" } }
const bytes = FlowRuntime.bundle("./tenant/index.ts", {
  importMapPath: "./import_map.json",
});
// `import x from "#lib/util.ts"` now resolves at bundle time AND inside the
// booted worker (the map ships in the artifact's workspace resolver).
```

### `FlowRuntime.unbundle`

Extracts an eszip (path, bytes, or stream) — including legacy formats that can
no longer boot workers. Each contained file fires a `"file"` event; pass
`output` (a directory) to also write the tree to disk through Deno's filesystem
APIs (so `--allow-write` applies):

```ts
const job = FlowRuntime.unbundle("service.eszip", "./extracted");
job.on("file", ({ specifier, path, kind, size }) => {
  // kind: "module" | "static" | "vfs" (a bundled node_modules file)
  console.log(kind, specifier, "→", path, `(${size}B)`);
});
await job.done; // resolves on "finish", rejects on "error"
```

`"finish"` fires after every file was emitted (and written, when `output` was
given); a `"file"` listener that throws aborts the job with `"error"`.

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
  // ev = { timestamp, event_type, event, metadata } — a discriminated
  // union; narrowing on event_type types the payload (`FlowWorkerEvent`)
  switch (ev.event_type) {
    case "Log": // every console.* call in a worker: { msg, level }
    case "Boot": // { boot_time }
    case "BootFailure": // { msg }
    case "UncaughtException": // { exception, cpu_time_used }
    case "Shutdown": // { reason, cpu_time_used, memory_used }
  }
  // ev.metadata = { service_path, execution_id, otel_attributes }
}
```

Per worker, the stream is `Boot` (possibly followed by `BootFailure`), then any
number of `Log`/`UncaughtException`, then a final `Shutdown`. The payloads
(exact types in [`flow types`](./cli.md#flow-types) output):

| `event_type`        | Payload                                                                                                                                                                                                                                                                                       |
| ------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Boot`              | `boot_time` — milliseconds from `create()` to the entrypoint being ready                                                                                                                                                                                                                      |
| `BootFailure`       | `msg` — building/evaluating the module graph failed (missing module, checksum mismatch, throw during evaluation); the worker never serves                                                                                                                                                     |
| `Log`               | `msg` and `level`: `console.debug` → `"Debug"`, `log`/`info` → `"Info"`, `warn` → `"Warning"`, `error` → `"Error"`                                                                                                                                                                            |
| `UncaughtException` | `exception` (rendered message + stack), `cpu_time_used` (ms)                                                                                                                                                                                                                                  |
| `Shutdown`          | `reason`, `cpu_time_used` (lifetime CPU ms), `memory_used` (`{ total, heap, external, mem_check_captured }`, bytes). `reason`: `EventLoopCompleted`, `WallClockTime`, `CPUTime`, `Memory`, `EarlyDrop` (early retirement / idle cleanup), or `TerminationRequested` (`scheduleTermination()`) |

`metadata` identifies the worker on every event: `execution_id` is the worker's
UUID (the same value as the `FlowUserWorker.key` returned by `create()`),
`service_path` is its pool key, and `otel_attributes` carries
`edge_runtime.worker.kind: "user"` plus anything passed under `context.otel` —
route events per tenant on any of these.

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
