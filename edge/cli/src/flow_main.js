// flow: post-bootstrap installer for the additive `EdgeRuntime` HOST global.
//
// Loaded by the `deno::embed` post-bootstrap hook (cli/embed.rs) once
// `edge/cli/src/main.rs` registers it, i.e. on every `flow run`. See memory
// `flow-runtime-architecture` (Phase C).
//
// DESIGN CONSTRAINTS learned the hard way:
//   1. This runs AFTER Deno's `bootstrapMainRuntime`, so `globalThis.Deno` and
//      the snapshot `ext:` modules (deno_web, deno_webidl, ...) are already live.
//   2. We must NOT add an ESM-carrying edge *extension* on top of Deno's CLI
//      snapshot (that panics: snapshotted `ext:` modules can't link against a
//      freshly added ESM extension). The edge ops are therefore embedded
//      OPS-ONLY (see ext_workers `user_workers_ops`). This file is evaluated via
//      `lazy_load_es_module_with_code` (the same path Deno uses for its post-
//      bootstrap test modules), so it MAY `import` snapshotted `ext:` modules
//      like `ext:core/mod.js` — but it must not import un-registered edge ESM.
//      (`globalThis.Deno.core` is NOT available post-bootstrap; Deno hides it.)
//   3. Namespace split (confirmed via trex examples + Supabase docs):
//        - main isolate / host  -> `EdgeRuntime` (userWorkers, ...)  [here]
//        - user workers         -> `Supabase`/`Flow` (ai.Session) — installed by
//          the edge runtime when it spawns user workers, NOT here.

import { core } from "ext:core/mod.js";

// The flow ops are registered at runtime ON TOP of the CLI snapshot, so they
// are not importable from the snapshot-baked `ext:core/ops` module (its export
// list is frozen at snapshot build). They ARE rebound onto the `core.ops`
// object at boot (`skip_op_registration` is disabled for flow) — the same
// pattern upstream's 40_test.js uses for its runtime-added ops. NOTE: any op
// destructured here must ALSO be in the `NOT_IMPORTED_OPS` allowlist in
// runtime/js/99_main.js, or `removeImportedOps()` scrubs it from `core.ops`
// during bootstrap (before this file runs).
const {
  op_user_worker_cleanup_idle_workers,
  op_user_worker_create,
  op_user_worker_inspect,
} = core.ops;

// `createMessagePort(rid)` wraps a MessagePort resource id into a JS MessagePort.
// op_user_worker_create returns the rid of the MAIN-side half of the duplex
// channel to the spawned worker.
const { createMessagePort } = core.loadExtScript(
  "ext:deno_web/13_message_port.js",
);

function define(name, value) {
  Object.defineProperty(globalThis, name, {
    value,
    writable: true,
    enumerable: false,
    configurable: true,
  });
}

// A handle to a spawned user worker. `port` is a duplex MessagePort to the
// worker (structured-clone messaging). Each create() gets its OWN channel,
// including when the pool reuses an already-running worker - the worker sees
// the extra port via `EdgeRuntime.onparentport` / `EdgeRuntime.parentPorts`
// (SharedWorker-style). `port` is `null` only in the rare race where the
// reused worker was torn down before the new channel could be delivered.
class UserWorker {
  constructor(key, port) {
    this.key = key;
    this.port = port;
  }

  // Returns a DevTools WebSocket URL (ws://host/ws/<id>) for debugging THIS
  // worker, or throws if the user-worker inspector wasn't enabled at startup
  // (via `--user-worker-inspect <addr>` or FLOW_USER_WORKER_INSPECTOR_ADDRESS).
  // The main isolate is debugged separately via Deno's own `--inspect`.
  inspect() {
    const url = op_user_worker_inspect(this.key);
    if (!url) {
      throw new Error(
        "flow: user-worker inspector is not enabled; start flow with " +
          "--user-worker-inspect <host:port> (or set " +
          "FLOW_USER_WORKER_INSPECTOR_ADDRESS)",
      );
    }
    return url;
  }
}

// Mirrors the edge `UserWorker.create` option defaults, minus the eszip/HTTP
// request-passing surface (that path is being replaced by MessagePort comms).
async function createUserWorker(opts) {
  const readyOptions = {
    noModuleCache: false,
    envVars: [],
    forceCreate: false,
    allowRemoteModules: true,
    ...opts,
  };

  const { servicePath, maybeEszip } = readyOptions;
  if (!maybeEszip && (!servicePath || servicePath === "")) {
    throw new TypeError("service path must be defined");
  }

  const [key, _reused, mainPortRid] = await op_user_worker_create(readyOptions);
  const port = mainPortRid != null ? createMessagePort(mainPortRid) : null;
  return new UserWorker(key, port);
}

async function tryCleanupIdleWorkers(timeoutMs) {
  return await op_user_worker_cleanup_idle_workers(timeoutMs);
}

// Host surface. The user-worker pool sender is injected into op_state by the
// post-bootstrap hook before this runs, so `create` is functional immediately.
define("EdgeRuntime", {
  userWorkers: {
    create: createUserWorker,
    tryCleanupIdleWorkers,
  },
});
