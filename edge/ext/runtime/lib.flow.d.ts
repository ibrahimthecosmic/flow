// flow: ambient type declarations for the additive flow globals.
//
// This file is the source of truth for flow's TypeScript surface. It is
// appended to `flow types` output (via the `deno::embed::register_extra_types`
// seam), so `flow types > flow.d.ts` produces the complete, always-current
// ambient environment. Keep it in sync with:
//   - edge/cli/src/flow_main.js          (host FlowRuntime surface)
//   - edge/ext/runtime/js/namespaces.js  (worker Flow / FlowRuntime surfaces)
//   - edge/ext/workers/lib.rs            (UserWorkerCreateOptions)
//   - edge/ext/event_worker/events.rs    (FlowRuntime.events shapes)
//   - edge/docs/httpfs-protocol.md       (HttpFS mount config, §7)

/** JSON-serializable value: what `context` and `FlowRuntime.context` may carry. */
declare type FlowJsonValue =
  | string
  | number
  | boolean
  | null
  | FlowJsonValue[]
  | { [key: string]: FlowJsonValue };

/**
 * AWS-style credentials for an S3 filesystem mount. Static only: the mount
 * uses exactly these values — no ambient provider chain (env vars, profile,
 * IMDS) is consulted.
 */
declare interface FlowS3FsCredentials {
  accessKeyId: string;
  secretAccessKey: string;
  /** STS session token, when using temporary credentials. */
  sessionToken?: string;
  /** Credential expiry, seconds since the Unix epoch. */
  expiresAfter?: number;
}

/**
 * One S3 filesystem mount for a user worker.
 *
 * Pass a single object (mounted at `/s3`) or an array of objects, each with
 * its own `mountPoint`. Mount points must be absolute, must not be `/`, and
 * must not equal or nest inside `/tmp` or one another.
 */
declare interface FlowS3FsConfig {
  /** Application id attached to requests (AWS SDK `app_name`, reported in
   * the user agent). Must be a valid AWS app name or `create()` rejects. */
  appName?: string;
  /** Endpoint for S3-compatible stores (MinIO, R2, localstack, …), e.g.
   * `http://localhost:9000`. Default: the AWS S3 endpoint for `region`. */
  endpointUrl?: string;
  /** AWS region the client signs for. */
  region?: string;
  /** Required; static credentials only. */
  credentials: FlowS3FsCredentials;
  /** Path-style addressing (`endpoint/bucket/key`); required for most
   * S3-compatible stores that don't resolve virtual-host bucket names. */
  forcePathStyle?: boolean;
  /** Retry tuning for the mount's S3 client. When the object is given,
   * `mode` is required; the rest fall back to the AWS SDK defaults noted
   * below. */
  retryConfig?: {
    /** Retry strategy: `"standard"` = exponential backoff with jitter,
     * `"adaptive"` = standard plus client-side rate limiting. */
    mode: "standard" | "adaptive";
    /** Total attempts including the first (`1` = no retries). Default: 3. */
    maxAttempts?: number;
    /** Backoff before the first retry, in seconds. Default: 1. */
    initialBackoffSec?: number;
    /** Backoff ceiling, in seconds. Default: 20. */
    maxBackoffSec?: number;
    /** `"reconnect_on_transient_error"` discards the connection after a
     * transient failure; `"reuse_all_connections"` keeps it pooled.
     * Default: `"reconnect_on_transient_error"`. */
    reconnectMode?: "reconnect_on_transient_error" | "reuse_all_connections";
    /** Deterministic exponential backoff instead of random jitter (mainly
     * for tests). Default: `false`. */
    useStaticExponentialBase?: boolean;
  };
  /** Where the bucket tree appears inside the worker. Default: `/s3`. */
  mountPoint?: string;
}

/**
 * Ephemeral `/tmp` filesystem tuning for a user worker. A fresh directory is
 * created on the host per worker and removed with it; `/tmp` is always
 * mounted inside the worker, with or without this config.
 */
declare interface FlowTmpFsConfig {
  /** Host directory the per-worker temp dir is created under.
   * Default: the OS temp dir. */
  base?: string;
  /** Prefix of the generated directory name. */
  prefix?: string;
  /** Suffix of the generated directory name. */
  suffix?: string;
  /** Number of random characters in the generated directory name. */
  randomLen?: number;
  /** Max bytes the worker may write under `/tmp`; writes beyond it fail. */
  quota?: number;
}

/**
 * One HttpFS mount: a virtual filesystem backed by any HTTP API implementing
 * the HttpFS Protocol v1 (see edge/docs/httpfs-protocol.md).
 *
 * Unknown keys are ignored, so a typo (`header:` for `headers:`) silently
 * yields a mount with no credentials rather than an error — double-check the
 * field names when auth doesn't take.
 */
declare interface FlowHttpFsConfig {
  /** Where the tree appears inside the worker (e.g. `/objects`). Required.
   * Same collision rules as S3 mount points. */
  mountPoint: string;
  /** Protocol base URL, including any path prefix
   * (e.g. `https://api.example.com/fs/v1`). With `socketPath` set, this stays an
   * http(s) URL but the host is a placeholder (e.g. `http://localhost/fs/v1`):
   * it only supplies the path prefix, the `Host` header, and the origin used to
   * scope credentials across redirects. */
  baseUrl: string;
  /** Custom headers attached to every request (e.g. `Authorization`,
   * `X-CSRF-Token`). Auth/scoping/revocation are the server's concern; the
   * runtime just forwards what you configure. Omitted on cross-origin redirect
   * targets (e.g. presigned URLs). */
  headers?: Record<string, string>;
  /** Custom query params appended to every request (same cross-origin rule as
   * `headers`). Avoid protocol-reserved keys (`path`, `cursor`, `uploadId`, …). */
  query?: Record<string, string>;
  /** Connect over this AF_UNIX socket instead of TCP — for a server sharing the
   * host (e.g. a sidecar). `baseUrl` still supplies the path prefix / `Host` /
   * origin. Cross-origin redirect (presigned) targets are still fetched over
   * TCP. */
  socketPath?: string;
}

/**
 * Deno-style permission set for a user worker (keys are snake_case, matching
 * Deno's `PermissionsOptions`). When the whole object is omitted the worker
 * gets flow's all-allowed user-worker defaults — the effective sandbox then
 * comes from the op-layer restrictions (no servers, no host fs, mocked
 * process control; see edge/docs/worker-runtime.md). There is no prompting:
 * a denied permission throws.
 *
 * When the object IS given, each `allow_*` key follows Deno CLI-flag
 * semantics: key omitted = that capability is fully denied; `[]` = blanket
 * allow (like a bare `--allow-net`); a non-empty list = allow only the
 * listed targets. Each `deny_*` list carves exceptions out of the
 * corresponding allow and wins on overlap.
 */
declare interface FlowUserWorkerPermissions {
  /** Accepted for edge-runtime compatibility; currently IGNORED — omit
   * `permissions` entirely for all-allowed, or grant per capability. */
  allow_all?: boolean;
  /** Environment variable names readable via `Deno.env`/`process.env`
   * (which only ever contain the create() `envVars`). */
  allow_env?: string[];
  deny_env?: string[];
  /** Outbound network targets, `host` or `host:port` (no scheme) — governs
   * `fetch`, `Deno.connect`, WebSocket clients, … */
  allow_net?: string[];
  deny_net?: string[];
  /** Dynamic-library paths for FFI. Of limited use in workers: native
   * Node addons are rejected regardless (see edge/docs/worker-runtime.md). */
  allow_ffi?: string[];
  deny_ffi?: string[];
  /** Readable paths, as seen by the worker (sandbox mounts like `/tmp`,
   * `/s3`, or real host paths with `allowHostFsAccess`). */
  allow_read?: string[];
  deny_read?: string[];
  /** Executables for subprocess spawning. */
  allow_run?: string[];
  deny_run?: string[];
  /** `Deno.systemInfo`-style probes (`hostname`, `osRelease`, …); much of
   * this is mocked in workers anyway (host info hiding). */
  allow_sys?: string[];
  deny_sys?: string[];
  /** Writable paths (same path semantics as `allow_read`). */
  allow_write?: string[];
  deny_write?: string[];
  /** Hosts allowed for remote (`https:`) imports at runtime. */
  allow_import?: string[];
}

/**
 * OpenTelemetry options for a user worker (per-worker mirror of Deno's
 * `OTEL_DENO_*` surface). Everything defaults to off.
 *
 * CAUTION: unlike every other create option, these keys are snake_case and
 * the string values are PascalCase (they deserialize straight into Deno's
 * telemetry config). Misspelled/camelCase keys are silently ignored and
 * leave telemetry off.
 */
declare interface FlowOtelConfig {
  /** Emit spans for the worker. Default: `false`. */
  tracing_enabled?: boolean;
  /** Emit runtime metrics. Default: `false`. */
  metrics_enabled?: boolean;
  /** What `console.*` does when telemetry is on: `"Ignore"` leaves console
   * output alone (default), `"Capture"` exports log records and still prints,
   * `"Replace"` exports instead of printing. */
  console?: "Ignore" | "Capture" | "Replace";
  /** Context propagators to install. Default: none. */
  propagators?: ("TraceContext" | "Baggage" | "None")[];
}

declare interface FlowUserWorkerCreateOptions {
  /** Directory containing the worker's entrypoint
   * (`index.{ts,tsx,js,mjs,jsx}`, unless `maybeEntrypoint` names another
   * module). Doubles as the worker's POOL KEY: under the default
   * `per_worker` policy, a later `create()` with the same `servicePath`
   * returns the already-running worker (see `forceCreate`). Required unless
   * `maybeEszip` is provided — then it has no directory meaning and only
   * serves as the pool key (defaults to `""`). */
  servicePath?: string;
  /** The worker's ENTIRE environment (`Deno.env` / `process.env`) as
   * `[key, value]` pairs; nothing is inherited from the host. Default: `[]`. */
  envVars?: [string, string][];
  /** Build the module graph without the local Deno module cache (remote
   * imports are re-downloaded). `servicePath`/`maybeModuleCode` builds only —
   * an eszip boot never fetches. Default: `false`. */
  noModuleCache?: boolean;
  /** Forbid `npm:` resolution while building the module graph
   * (`servicePath` builds only). Default: npm allowed. */
  noNpm?: boolean;
  /** Never reuse a running worker for this `servicePath`; always boot a
   * fresh one (implied by the `oneshot` pool policy). Default: `false`. */
  forceCreate?: boolean;
  /** Allow `https:` imports while building the module graph
   * (`servicePath` builds only). Default: `true`. */
  allowRemoteModules?: boolean;
  /** Accepted for edge-runtime compatibility; currently unused. */
  customModuleRoot?: string;
  /** Permission set for the worker. Omitted: all-allowed defaults (the
   * op-layer sandbox still applies). */
  permissions?: FlowUserWorkerPermissions;

  /** Precompiled eszip payload (alternative to `servicePath`; the artifact
   * carries its own entrypoint in its metadata).
   *
   * All forms end up **file-backed** — only the archive header is parsed
   * into memory; module sources, npm packages, and static assets are read
   * from disk on demand (OS page cache only, nothing pins), so worker RSS
   * scales with the touched working set instead of the bundle size:
   * - `Uint8Array`: the bytes are spilled into the runtime's
   *   content-addressed bundle cache (`$FLOW_BUNDLE_CACHE_DIR`, defaulting
   *   to `<tmpdir>/flow-bundles`; entries are swept after
   *   `$FLOW_BUNDLE_CACHE_TTL_SECS`, default 7 days). Identical bundles
   *   converge on one cache file.
   * - `string`: path of an `.eszip` file on disk, used in place (never
   *   copied into the cache). Concurrent/later creates for the same
   *   canonical path share one parsed header and file handle.
   * - `ReadableStream<Uint8Array>`: streamed into the bundle cache chunk by
   *   chunk without ever materializing the whole bundle in memory.
   *
   * Bundles built with a checksum (`flow eszip bundle --checksum`) are
   * verified on every module read; corruption fails the worker's module
   * init with an `invalid source hash` `BootFailure` event. Old bundle
   * formats (pre-flow-2.0) are rejected with a re-bundle error —
   * `FlowRuntime.unbundle` still reads them. Module-graph failures happen
   * after `create()` resolves and surface as `BootFailure` on
   * `FlowRuntime.events`. */
  maybeEszip?: Uint8Array | string | ReadableStream<Uint8Array> | null;
  /** Entrypoint override for `servicePath` builds: a path resolved against
   * `servicePath`, or a full URL. NOT an override for eszip boots — a
   * current-format bundle always carries an entrypoint key in its metadata,
   * and that key wins; only a legacy (migrated) bundle without one consults
   * this option (a full URL is then required). */
  maybeEntrypoint?: string | null;
  /** Inline module source. Still requires a `servicePath`, which acts as the
   * worker's pool key and base directory. */
  maybeModuleCode?: string | null;

  /** V8 heap cap in MiB; the supervisor retires the worker when exceeded.
   * Default: `$FLOW_USER_WORKER_MAX_HEAP_SIZE_MIB`, or 512. */
  memoryLimitMb?: number;
  /** When V8 signals it is near the heap cap, the limit is temporarily
   * raised to `current × this factor` so the supervisor can retire the
   * worker gracefully instead of the OOM taking down the whole flow
   * process. Default: 5. */
  lowMemoryMultiplier?: number;
  /** Wall-clock lifetime in ms; the worker is retired after this long
   * regardless of activity. Default: 60000. */
  workerTimeoutMs?: number;
  /** CPU-time budget in ms after which the worker is flagged for early
   * retirement (finishes current work, receives no new work). Default: 50. */
  cpuTimeSoftLimitMs?: number;
  /** CPU-time budget in ms after which the worker is forcibly terminated.
   * Default: 100. The tight defaults suit small handlers; module-heavy
   * startup (large `node:`/npm graphs) can exceed them — raise both for such
   * services. */
  cpuTimeHardLimitMs?: number;

  /** S3 filesystem mounts: one config (mounted at `/s3`) or an array with
   * per-entry `mountPoint`s. */
  s3FsConfig?: FlowS3FsConfig | FlowS3FsConfig[];
  /** Ephemeral `/tmp` tuning. `/tmp` is always mounted, config or not. */
  tmpFsConfig?: FlowTmpFsConfig;
  /** HttpFS protocol mounts: one config or an array. */
  httpFs?: FlowHttpFsConfig | FlowHttpFsConfig[];
  /** Per-worker OpenTelemetry tracing/metrics. Default: all off. Mind the
   * key/value casing — see `FlowOtelConfig`. */
  otelConfig?: FlowOtelConfig;

  /**
   * Arbitrary JSON handed to the worker; the worker reads it back via the
   * deep-frozen `FlowRuntime.context` global. Runtime-owned keys (e.g.
   * `terminationRequestToken`) are stripped from what the worker sees.
   *
   * A few keys are ALSO read by the runtime itself while booting the worker:
   * - `importMapPath` (string): import map applied when building the module
   *   graph of a `servicePath` build (and when migrating a legacy eszip).
   *   Current-format eszip boots carry their import map inside the bundle
   *   and ignore this.
   * - `unstableSloppyImports` (boolean): enable sloppy imports for
   *   `servicePath` builds.
   * - `sourceMap` (boolean): load source maps so worker stack traces map
   *   back to the original sources (implied when the user-worker inspector
   *   is enabled).
   */
  context?: { [key: string]: FlowJsonValue };
  /** Glob patterns (resolved against the host's CWD) of static files baked
   * into the worker's sandbox filesystem, for `servicePath` builds. Eszip
   * boots ignore this — their static assets were fixed at
   * `flow eszip bundle --static` time. */
  staticPatterns?: string[];
  /** Give the worker real host-filesystem access instead of the sandbox.
   * This also lets module loads that miss the module graph fall back to
   * host files. Rejected when the runtime runs with `--restrict-host-fs`.
   * Default: `false`. */
  allowHostFsAccess?: boolean;
}

/** A running (or reused) user worker, as seen from the host isolate. */
declare interface FlowUserWorker {
  /** The worker's identity: a UUID assigned at boot. Handles returned by a
   * pool-reusing `create()` share the same `key`, and the worker's events
   * carry it as `metadata.execution_id`. (Not the `servicePath` pool key.) */
  readonly key: string;
  /**
   * Duplex `MessagePort` to the worker (structured clone; transferable
   * ArrayBuffers move zero-copy). `null` only when a reused worker was torn
   * down before the new channel could be delivered.
   */
  readonly port: MessagePort | null;
  /**
   * DevTools WebSocket URL for debugging this worker. Throws when the
   * user-worker inspector wasn't enabled (`--user-worker-inspect` /
   * `FLOW_USER_WORKER_INSPECTOR_ADDRESS`).
   */
  inspect(): string;
}

/**
 * Why a worker was shut down (the `reason` of a `Shutdown` event):
 * - `"EventLoopCompleted"` — the event loop emptied: entrypoint done, no
 *   pending timers/IO, nothing held via `FlowRuntime.waitUntil`.
 * - `"WallClockTime"` — `workerTimeoutMs` elapsed.
 * - `"CPUTime"` — `cpuTimeHardLimitMs` was exhausted.
 * - `"Memory"` — `memoryLimitMb` was exceeded.
 * - `"EarlyDrop"` — retired early after being flagged (soft CPU limit,
 *   wall-clock warning, `tryCleanupIdleWorkers`) once pending work drained.
 * - `"TerminationRequested"` — the worker called
 *   `FlowRuntime.scheduleTermination()`.
 */
declare type FlowShutdownReason =
  | "EventLoopCompleted"
  | "WallClockTime"
  | "CPUTime"
  | "Memory"
  | "EarlyDrop"
  | "TerminationRequested";

/** `Boot` event payload: the worker booted successfully. */
declare interface FlowBootEvent {
  /** Milliseconds from `create()` to the entrypoint being ready to run. */
  boot_time: number;
}

/** `BootFailure` event payload: building/evaluating the worker's module
 * graph failed (missing module, eszip checksum mismatch, throw during
 * evaluation, …). Follows the `Boot` event; `create()` has already
 * resolved, and the worker never serves. */
declare interface FlowBootFailureEvent {
  msg: string;
}

/** `UncaughtException` event payload. */
declare interface FlowUncaughtExceptionEvent {
  /** Rendered exception (message and stack). */
  exception: string;
  /** CPU milliseconds the worker had used when it threw. */
  cpu_time_used: number;
}

/** `Log` event payload: one worker `console.*` call. */
declare interface FlowLogEvent {
  msg: string;
  /** `console.debug` → `"Debug"`, `log`/`info` → `"Info"`,
   * `warn` → `"Warning"`, `error` → `"Error"`. */
  level: "Debug" | "Info" | "Warning" | "Error";
}

/** Memory snapshot inside a `Shutdown` event. All byte counts. */
declare interface FlowWorkerMemoryUsed {
  /** `heap + external`. */
  total: number;
  /** Used V8 heap bytes. */
  heap: number;
  /** External (off-V8-heap) bytes attributed to the isolate. */
  external: number;
  /** Last snapshot taken by the supervisor's periodic memory checker. */
  mem_check_captured: {
    current: {
      totalHeapSize: number;
      totalHeapSizeExecutable: number;
      totalPhysicalSize: number;
      totalAvailableSize: number;
      totalGlobalHandlesSize: number;
      usedGlobalHandlesSize: number;
      usedHeapSize: number;
      mallocedMemory: number;
      externalMemory: number;
      peakMallocedMemory: number;
    };
    /** Whether the checker had already seen the limit exceeded. */
    exceeded: boolean;
  };
}

/** `Shutdown` event payload — always a worker's final event. */
declare interface FlowShutdownEvent {
  reason: FlowShutdownReason;
  /** CPU milliseconds used over the worker's lifetime. */
  cpu_time_used: number;
  memory_used: FlowWorkerMemoryUsed;
}

/** Worker identity attached to every event. */
declare interface FlowWorkerEventMetadata {
  /** The `servicePath` the worker was created with (pool key; empty string
   * for eszip-only creates that omitted it). */
  service_path: string | null;
  /** The worker's UUID — equals the corresponding `FlowUserWorker.key`. */
  execution_id: string | null;
  /** `edge_runtime.worker.kind: "user"` plus every key of the create()
   * `context.otel` object, stringified. */
  otel_attributes: { [key: string]: string } | null;
}

/** Envelope common to every event yielded by `FlowRuntime.events`. */
declare interface FlowWorkerEventEnvelope<Type extends string, Payload> {
  /** ISO-8601 time the event was observed on the host. */
  timestamp: string;
  event_type: Type;
  event: Payload;
  metadata: FlowWorkerEventMetadata;
}

/**
 * A user-worker lifecycle/log event yielded by `FlowRuntime.events`.
 * Discriminate on `event_type` to narrow `event`. Per worker, the stream is
 * `Boot` (possibly followed by `BootFailure`), then any number of `Log` /
 * `UncaughtException`, then a final `Shutdown`.
 */
declare type FlowWorkerEvent =
  | FlowWorkerEventEnvelope<"Log", FlowLogEvent>
  | FlowWorkerEventEnvelope<"Boot", FlowBootEvent>
  | FlowWorkerEventEnvelope<"BootFailure", FlowBootFailureEvent>
  | FlowWorkerEventEnvelope<"UncaughtException", FlowUncaughtExceptionEvent>
  | FlowWorkerEventEnvelope<"Shutdown", FlowShutdownEvent>;

/** Options for `FlowRuntime.bundle` (the programmatic twin of
 * `flow eszip bundle`). */
declare interface FlowBundleOptions {
  /** Glob patterns (resolved against the CWD) of static files to bundle
   * alongside the module graph; workers read them back through the sandbox
   * filesystem. Same as `flow eszip bundle --static`. */
  staticPatterns?: string[];
  /** Integrity checksum baked into the eszip: every whole-entry read at boot
   * and import time is then verified against its stored hash, and corruption
   * fails the worker with an `invalid source hash` `BootFailure` instead of
   * running altered code. Recommended for artifacts that cross a network or
   * shared storage. Default: none (no verification). */
  checksum?: "none" | "sha256" | "xxhash3";
  /** Abort bundling after this many milliseconds; surfaces as an error on
   * the returned stream. Default: no timeout. */
  timeoutMs?: number;
  /** Re-download remote modules instead of using the local module cache.
   * Default: `false`. */
  noModuleCache?: boolean;
  /**
   * Path to an import map (JSON file, resolved against the CWD) applied
   * while building the module graph: bare and aliased specifiers in the
   * bundled code resolve through it. The map is also serialized into the
   * artifact's workspace resolver, so the same mappings hold inside a worker
   * booted from the bundle (including for dynamic imports). Without it, the
   * workspace configuration discovered for the entrypoint (`deno.json`
   * `imports`, `package.json`) applies as usual.
   *
   * (The `flow eszip bundle` CLI has no matching flag — it relies on
   * workspace discovery; use this API to apply an explicit map.)
   */
  importMapPath?: string;
  /**
   * Module specifiers or globs to leave OUT of the bundle. Each excluded module
   * is emitted as a bare import to be resolved at runtime (e.g. a centrally
   * maintained built-in service), rather than baked into the archive.
   *
   * - A specifier (e.g. `"#services/shopify/mod.ts"`, a path, or a `file://`
   *   URL) excludes that exact module; its dependency subtree is pruned only
   *   where reachable *solely* through excluded modules.
   * - A glob (e.g. `"services/shopify/**"`) excludes every matching module,
   *   regardless of how it is imported.
   *
   * A dependency also reachable from a non-excluded module stays bundled, so
   * its identity at runtime follows normal ESM resolution.
   *
   * The worker must be able to resolve the excluded imports at boot: today
   * that requires `allowHostFsAccess: true` so the loader's host-filesystem
   * fallback can read them from disk (an eszip-only sandboxed worker fails
   * such imports with `Module not found`). Patterns that match no module are
   * silently ignored.
   */
  exclude?: string[];
}

/** Per-file metadata emitted by a `FlowRuntime.unbundle` "file" event. */
declare interface FlowUnbundledFile {
  /** Module specifier the file had inside the eszip. */
  specifier: string;
  /** Destination path relative to the extraction root. */
  path: string;
  /** "module" = graph module, "static" = static asset, "vfs" = a file of the
   * bundled `node_modules` virtual filesystem. */
  kind: "module" | "static" | "vfs";
  /** File size in bytes. */
  size: number;
}

/**
 * An unbundle job returned by `FlowRuntime.unbundle`. "finish" fires after
 * every file was emitted (and, when an output directory was given, written
 * to disk); a "file" listener that throws aborts the job with "error".
 */
declare interface FlowUnbundled {
  on(
    event: "file",
    listener: (
      metadata: FlowUnbundledFile,
      stream: ReadableStream<Uint8Array>,
    ) => void,
  ): FlowUnbundled;
  on(event: "finish", listener: () => void): FlowUnbundled;
  on(event: "error", listener: (err: Error) => void): FlowUnbundled;
  off(event: "file" | "finish" | "error", listener: unknown): FlowUnbundled;
  /** Resolves on "finish", rejects on "error". */
  readonly done: Promise<void>;
}

/**
 * The flow runtime surface. `FlowRuntime` is the single flow global in both the
 * MAIN isolate (`flow run ...`) and USER workers, but the members differ by
 * context: the host exposes the pool/tooling below (`userWorkers`, `events`,
 * `bundle`, …), while a worker exposes the worker-side helpers (`parentPort`,
 * `waitUntil`, `scheduleTermination`). `context` is available in both.
 */
declare const FlowRuntime: {
  userWorkers: {
    /** Boot (or reuse) a user worker. */
    create(opts: FlowUserWorkerCreateOptions): Promise<FlowUserWorker>;
    /**
     * Request teardown of every worker with no in-flight work, waiting up to
     * `timeoutMs` for each to acknowledge. `timeoutMs` is NOT an idle-age
     * threshold; a worker that is idle right now is torn down. Resolves with
     * the number of workers that acknowledged the drop.
     */
    tryCleanupIdleWorkers(timeoutMs: number): Promise<number>;
  };
  /**
   * Single-consumer async iterable over user-worker events. While nobody
   * iterates, worker output is relayed to the host's stdio; the first
   * `next()` claims the stream and breaking out of the loop hands it back.
   */
  events: AsyncIterable<FlowWorkerEvent>;

  /**
   * Bundles an entrypoint into an eszip, returned as a byte stream (pipe it
   * to a file, or pass the collected bytes to `userWorkers.create`'s
   * `maybeEszip`). `entrypoint` is either a path on disk (string) or the
   * entry module's source code (bytes/stream; the module is bundled under a
   * synthetic `/src/index.ts` specifier and its imports resolve against the
   * current working directory). Bundling runs on a dedicated thread;
   * failures surface as stream errors. See `FlowBundleOptions` for import
   * maps, checksums, static assets, and exclusions.
   */
  bundle(
    entrypoint: string | Uint8Array | ArrayBuffer | ReadableStream<Uint8Array>,
    options?: FlowBundleOptions,
  ): ReadableStream<Uint8Array>;
  /**
   * Extracts an eszip (path on disk, bytes, or stream). Each contained file
   * fires a "file" event; pass `output` to also write the tree under that
   * directory (via Deno's filesystem APIs, so `--allow-write` applies).
   */
  unbundle(
    eszip: string | Uint8Array | ArrayBuffer | ReadableStream<Uint8Array>,
    output?: string,
  ): FlowUnbundled;

  /**
   * The JSON `context` this worker/isolate was created with (deep-frozen,
   * memoized); runtime-owned bootstrap keys are stripped. Empty `{}` when none
   * was provided. (Formerly the separate `Flow.context` global.)
   */
  readonly context: Readonly<{ [key: string]: FlowJsonValue }>;

  /** USER workers: duplex `MessagePort` to the host handle that created this
   * worker. */
  parentPort?: MessagePort;
  /** USER workers: every parent port delivered so far (pool reuse delivers
   * additional channels, SharedWorker-style — the first is included). */
  parentPorts?: MessagePort[];
  /** USER workers: assignable callback invoked with each additional parent port
   * delivered by a reused `create()`. */
  onparentport?: (port: MessagePort) => void;
  /** USER workers: keep the worker alive until `promise` settles. */
  waitUntil?<T>(promise: Promise<T>): Promise<T>;
  /** USER workers: request graceful self-termination. This is a worker's only
   * self-exit — `Deno.exit` is a no-op in the sandbox. */
  scheduleTermination?(): void;
};

/** flow runtime version string. (Installed in USER workers; the host isolate
 * is plain Deno and does not define this global.) */
declare const FLOW_VERSION: string;
/** Version of the Deno runtime flow is built on. (Installed in USER workers;
 * on the host use `Deno.version.deno`.) */
declare const DENO_VERSION: string;
