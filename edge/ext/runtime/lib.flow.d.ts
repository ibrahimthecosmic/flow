// flow: ambient type declarations for the additive flow globals.
//
// This file is the source of truth for flow's TypeScript surface. It is
// appended to `flow types` output (via the `deno::embed::register_extra_types`
// seam), so `flow types > flow.d.ts` produces the complete, always-current
// ambient environment. Keep it in sync with:
//   - edge/cli/src/flow_main.js          (host FlowRuntime surface)
//   - edge/ext/runtime/js/namespaces.js  (worker Flow / FlowRuntime surfaces)
//   - edge/ext/workers/lib.rs            (UserWorkerCreateOptions)
//   - edge/docs/httpfs-protocol.md       (HttpFS mount config, §7)

/** JSON-serializable value: what `context` and `FlowRuntime.context` may carry. */
declare type FlowJsonValue =
  | string
  | number
  | boolean
  | null
  | FlowJsonValue[]
  | { [key: string]: FlowJsonValue };

/** AWS-style credentials for an S3 filesystem mount. */
declare interface FlowS3FsCredentials {
  accessKeyId: string;
  secretAccessKey: string;
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
  appName?: string;
  endpointUrl?: string;
  region?: string;
  credentials: FlowS3FsCredentials;
  /** Path-style addressing (`endpoint/bucket/key`); required for most
   * S3-compatible stores that don't resolve virtual-host bucket names. */
  forcePathStyle?: boolean;
  retryConfig?: {
    mode?: "standard" | "adaptive";
    maxAttempts?: number;
    initialBackoff?: number;
    maxBackoff?: number;
    reconnectMode?: string;
    useStaticExponentialBase?: boolean;
  };
  /** Where the bucket tree appears inside the worker. Default: `/s3`. */
  mountPoint?: string;
}

/** Ephemeral `/tmp` filesystem tuning for a user worker. */
declare interface FlowTmpFsConfig {
  base?: string;
  prefix?: string;
  suffix?: string;
  randomLen?: number;
  /** Max bytes the worker may write under `/tmp`. */
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

/** Deno-style permission set for a user worker (keys are snake_case). */
declare interface FlowUserWorkerPermissions {
  allow_all?: boolean;
  allow_env?: string[];
  deny_env?: string[];
  allow_net?: string[];
  deny_net?: string[];
  allow_ffi?: string[];
  deny_ffi?: string[];
  allow_read?: string[];
  deny_read?: string[];
  allow_run?: string[];
  deny_run?: string[];
  allow_sys?: string[];
  deny_sys?: string[];
  allow_write?: string[];
  deny_write?: string[];
  allow_import?: string[];
}

/** OpenTelemetry options for a user worker. */
declare interface FlowOtelConfig {
  tracingEnabled?: boolean;
  metricsEnabled?: boolean;
  console?: "ignore" | "capture" | "replace";
  propagators?: ("traceContext" | "baggage")[];
}

declare interface FlowUserWorkerCreateOptions {
  /** Directory containing the worker's entrypoint. Required unless
   * `maybeEszip` is provided. */
  servicePath?: string;
  /** Environment variables visible to the worker. */
  envVars?: [string, string][];
  noModuleCache?: boolean;
  noNpm?: boolean;
  /** Never reuse a running worker for this `servicePath`. */
  forceCreate?: boolean;
  allowRemoteModules?: boolean;
  customModuleRoot?: string;
  permissions?: FlowUserWorkerPermissions;

  /** Precompiled eszip payload (alternative to `servicePath`). */
  maybeEszip?: Uint8Array | null;
  maybeEntrypoint?: string | null;
  maybeModuleCode?: string | null;

  memoryLimitMb?: number;
  lowMemoryMultiplier?: number;
  workerTimeoutMs?: number;
  cpuTimeSoftLimitMs?: number;
  cpuTimeHardLimitMs?: number;

  /** S3 filesystem mounts: one config (mounted at `/s3`) or an array with
   * per-entry `mountPoint`s. */
  s3FsConfig?: FlowS3FsConfig | FlowS3FsConfig[];
  /** Ephemeral `/tmp` tuning. `/tmp` is always mounted, config or not. */
  tmpFsConfig?: FlowTmpFsConfig;
  /** HttpFS protocol mounts: one config or an array. */
  httpFs?: FlowHttpFsConfig | FlowHttpFsConfig[];
  otelConfig?: FlowOtelConfig;

  /**
   * Arbitrary JSON handed to the worker; the worker reads it back via the
   * deep-frozen `FlowRuntime.context` global. Runtime-owned keys (e.g.
   * `terminationRequestToken`) are stripped from what the worker sees.
   */
  context?: { [key: string]: FlowJsonValue };
  /** Glob patterns of static files available to the worker. */
  staticPatterns?: string[];
  /** Give the worker real host-filesystem access instead of the sandbox.
   * Rejected when the runtime runs with `--restrict-host-fs`. */
  allowHostFsAccess?: boolean;
}

/** A running (or reused) user worker, as seen from the host isolate. */
declare interface FlowUserWorker {
  /** Pool key identifying the worker. */
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

/** A user-worker lifecycle/log event yielded by `FlowRuntime.events`. */
declare interface FlowWorkerEvent {
  /** ISO-8601 time the event was observed. */
  timestamp: string;
  event_type:
    | "Log"
    | "Boot"
    | "BootFailure"
    | "UncaughtException"
    | "Shutdown"
    | (string & Record<never, never>);
  /** Event payload; shape depends on `event_type`. */
  event: { [key: string]: FlowJsonValue };
  /** Worker identity: service path, execution id, and similar. */
  metadata: { [key: string]: FlowJsonValue };
}

/** Options for `FlowRuntime.bundle` (the programmatic twin of
 * `flow eszip bundle`). */
declare interface FlowBundleOptions {
  /** Glob patterns of static files to bundle alongside the module graph. */
  staticPatterns?: string[];
  /** Integrity checksum baked into the eszip. Default: none. */
  checksum?: "none" | "sha256" | "xxhash3";
  /** Abort bundling after this many milliseconds. */
  timeoutMs?: number;
  /** Re-download remote modules instead of using the local cache. */
  noModuleCache?: boolean;
  /** Path to an import map applied while building the module graph. */
  importMapPath?: string;
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
    /** Tear down workers idle longer than `timeoutMs`. */
    tryCleanupIdleWorkers(timeoutMs: number): Promise<unknown>;
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
   * entry module's source code (bytes/stream; imports then resolve against
   * the current working directory). Bundling runs on a dedicated thread;
   * failures surface as stream errors.
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

/** flow runtime version string. */
declare const FLOW_VERSION: string;
/** Version of the Deno runtime flow is built on. */
declare const DENO_VERSION: string;
