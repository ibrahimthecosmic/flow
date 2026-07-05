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

/** JSON-serializable value: what `context` and `Flow.context` may carry. */
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
 * Auth transport for an HttpFS mount: where the opaque token goes on every
 * request. Default: `{ header: "Authorization", scheme: "Bearer" }`.
 */
declare type FlowHttpFsAuth =
  | { header: string; scheme?: string }
  | { query: string };

/**
 * One HttpFS mount: a virtual filesystem backed by any HTTP API implementing
 * the HttpFS Protocol v1 (see edge/docs/httpfs-protocol.md).
 */
declare interface FlowHttpFsConfig {
  /** Where the tree appears inside the worker (e.g. `/objects`). Required.
   * Same collision rules as S3 mount points. */
  mountPoint: string;
  /** Protocol base URL, including any path prefix
   * (e.g. `https://api.example.com/fs/v1`). */
  baseUrl: string;
  /** Opaque credential attached to every request. Scoping/revocation is the
   * server's concern. */
  token: string;
  auth?: FlowHttpFsAuth;
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
   * deep-frozen `Flow.context` global. Runtime-owned keys (e.g.
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
    | (string & {});
  /** Event payload; shape depends on `event_type`. */
  event: { [key: string]: FlowJsonValue };
  /** Worker identity: service path, execution id, and similar. */
  metadata: { [key: string]: FlowJsonValue };
}

/**
 * Host-side flow surface, available in the MAIN isolate (`flow run ...`).
 *
 * In USER workers, `FlowRuntime` instead exposes the worker-side helpers
 * (`waitUntil`, `scheduleTermination`).
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

  /** USER workers only: keep the worker alive until `promise` settles. */
  waitUntil?<T>(promise: Promise<T>): Promise<T>;
  /** USER workers only: request supervisor-driven termination. */
  scheduleTermination?(): void;
};

/**
 * Worker-side flow surface (available in all worker kinds; primarily useful
 * inside USER workers).
 */
declare const Flow: {
  /** flow AI APIs (e.g. `new Flow.ai.Session(...)`). */
  readonly ai: unknown;
  /**
   * The JSON `context` this worker was created with (deep-frozen, memoized).
   * Runtime-owned bootstrap keys are stripped.
   */
  readonly context: Readonly<{ [key: string]: FlowJsonValue }>;
};

/** flow runtime version string. */
declare const FLOW_VERSION: string;
/** Version of the Deno runtime flow is built on. */
declare const DENO_VERSION: string;
