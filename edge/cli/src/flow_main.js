// flow: post-bootstrap installer for the additive `FlowRuntime` HOST global.
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
//        - main isolate / host  -> `FlowRuntime` (userWorkers, ...)  [here]
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
  op_eszip_bundle,
  op_eszip_unbundle_next,
  op_eszip_unbundle_open,
  op_flow_events_accept,
  op_flow_events_cancel,
  op_flow_events_claim,
  op_flow_events_release,
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
// `readableStreamForRid(rid)` wraps a deno_core byte-stream resource into a
// ReadableStream<Uint8Array> (the standard resource-backed stream helper).
const { readableStreamForRid } = core.loadExtScript(
  "ext:deno_web/06_streams.js",
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
// the extra port via `FlowRuntime.onparentport` / `FlowRuntime.parentPorts`
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

// `FlowRuntime.events`: a single-consumer async iterable over user-worker
// events (Log/Boot/BootFailure/UncaughtException/Shutdown). The yielded shape
// `{timestamp, event_type, event, metadata}` matches edge/trex's EventManager
// (edge/ext/event_worker/event_worker.js), so their event-manager examples
// port as-is. While nobody iterates, the Rust relay prints worker output with
// stdio-inherit semantics (see edge/cli/src/flow_events.rs); the first next()
// claims the stream, and breaking out of the loop (iterator return()) hands
// it back. A second concurrent consumer gets a claim error from the op.
function unwrapEvent(raw) {
  // RawEvent::Event serde shape: { Event: { event: { <Type>: {...} },
  // metadata: {...} } }
  const withMeta = raw.Event;
  const eventType = Object.keys(withMeta.event)[0];
  return {
    timestamp: new Date().toISOString(),
    event_type: eventType,
    event: withMeta.event[eventType],
    metadata: withMeta.metadata,
  };
}

const events = {
  [Symbol.asyncIterator]() {
    let claimed = false;
    let done = false;
    // Serialize next()/return(): a return() racing a pending accept would
    // find the receiver checked out of op_state and silently skip the
    // release, wedging the stream. Chaining removes the race.
    let chain = Promise.resolve();
    const enqueue = (fn) => {
      const step = chain.then(fn);
      chain = step.then(() => {}, () => {});
      return step;
    };

    return {
      next: () =>
        enqueue(async () => {
          if (done) {
            return { value: undefined, done: true };
          }
          if (!claimed) {
            await op_flow_events_claim();
            claimed = true;
          }
          const raw = await op_flow_events_accept();
          if (raw === "Done") {
            // Worker pool shut down. Release the (now dead) receiver so the
            // relay task can observe the closure and exit.
            done = true;
            claimed = false;
            op_flow_events_release();
            return { value: undefined, done: true };
          }
          return { value: unwrapEvent(raw), done: false };
        }),
      return: (value) => {
        // Interrupt a pending accept right away (outside the queue) so a
        // consumer blocked on next() can stop; the queued step below then
        // hands the stream back in order.
        if (claimed) {
          op_flow_events_cancel();
        }
        return enqueue(() => {
          if (claimed) {
            op_flow_events_release();
            claimed = false;
          }
          done = true;
          return { value, done: true };
        });
      },
      [Symbol.asyncIterator]() {
        return this;
      },
    };
  },
};

// ---------------------------------------------------------------------------
// `FlowRuntime.bundle` / `FlowRuntime.unbundle`: the programmatic twin of the
// `flow eszip bundle/unbundle` CLI (see edge/cli/src/flow_eszip.rs).

// Collects a bytes-ish input (Uint8Array, ArrayBuffer, or
// ReadableStream<Uint8Array>) into a single Uint8Array.
async function collectBytes(input) {
  if (input instanceof Uint8Array) {
    return input;
  }
  if (input instanceof ArrayBuffer) {
    return new Uint8Array(input);
  }
  if (input instanceof ReadableStream) {
    const chunks = [];
    let total = 0;
    for await (const chunk of input) {
      if (!(chunk instanceof Uint8Array)) {
        throw new TypeError("expected a ReadableStream of Uint8Array chunks");
      }
      chunks.push(chunk);
      total += chunk.byteLength;
    }
    const out = new Uint8Array(total);
    let offset = 0;
    for (const chunk of chunks) {
      out.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return out;
  }
  throw new TypeError(
    "expected a string path, Uint8Array, ArrayBuffer, or ReadableStream",
  );
}

// Bundles an entrypoint into an eszip, returned as a
// ReadableStream<Uint8Array>. `entrypoint` is either a path on disk (string)
// or the entry module's SOURCE CODE (Uint8Array/ArrayBuffer/ReadableStream —
// imports are resolved against the current working directory). Bundling runs
// on a dedicated thread; failures surface as stream errors.
function bundle(entrypoint, options = {}) {
  const opOptions = {
    staticPatterns: options.staticPatterns ?? [],
    checksum: options.checksum,
    timeoutMs: options.timeoutMs,
    noModuleCache: options.noModuleCache ?? false,
    importMapPath: options.importMapPath,
  };

  if (typeof entrypoint === "string") {
    const rid = op_eszip_bundle({ ...opOptions, entrypoint });
    return readableStreamForRid(rid);
  }

  // Source-code form: collecting the input may be async, so serve the
  // op-backed stream through a wrapper to keep bundle() synchronous.
  let reader;
  return new ReadableStream({
    async start() {
      const code = new TextDecoder().decode(await collectBytes(entrypoint));
      const rid = op_eszip_bundle({ ...opOptions, moduleCode: code });
      reader = readableStreamForRid(rid).getReader();
    },
    async pull(controller) {
      const { value, done } = await reader.read();
      if (done) {
        controller.close();
      } else {
        controller.enqueue(value);
      }
    },
    cancel(reason) {
      return reader?.cancel(reason);
    },
  });
}

// Unbundles an eszip (path, bytes, or stream). Returns an emitter:
//
//   unbundled.on("file", (metadata, stream) => { ... })  // one per file
//   unbundled.on("finish", () => { ... })  // all files emitted
//   unbundled.on("error", (err) => { ... })
//   await unbundled.done  // resolves on finish, rejects on error
//
// When `output` (a directory path) is given, every file is also written to
// disk under it — through Deno's filesystem APIs, so `flow run`'s permission
// model applies (needs `--allow-write` on the output directory).
//
// "finish" means every entry was emitted, not that the per-file streams were
// consumed; each stream carries a single in-memory chunk, so piping it cannot
// stall. A "file" listener that throws aborts the job with "error".
function unbundle(eszip, output) {
  const listeners = { file: [], finish: [], error: [] };
  let resolveDone, rejectDone;
  const done = new Promise((resolve, reject) => {
    resolveDone = resolve;
    rejectDone = reject;
  });

  const emitter = {
    on(event, fn) {
      if (!listeners[event]) {
        throw new TypeError(`unknown unbundle event: ${event}`);
      }
      listeners[event].push(fn);
      return emitter;
    },
    off(event, fn) {
      const arr = listeners[event] ?? [];
      const at = arr.indexOf(fn);
      if (at >= 0) {
        arr.splice(at, 1);
      }
      return emitter;
    },
    done,
  };

  const emit = (event, ...args) => {
    for (const fn of [...listeners[event]]) {
      fn(...args);
    }
  };

  // The pump starts with an await, so listeners attached synchronously after
  // unbundle() returns are always registered before the first event fires.
  (async () => {
    const rid = typeof eszip === "string"
      ? await op_eszip_unbundle_open({ path: eszip })
      : await op_eszip_unbundle_open({ data: await collectBytes(eszip) });
    try {
      for (;;) {
        const entry = await op_eszip_unbundle_next(rid);
        if (entry == null) {
          break;
        }
        const { specifier, path, kind, data } = entry;
        if (output !== undefined) {
          const dest = `${output}/${path}`;
          await Deno.mkdir(dest.slice(0, dest.lastIndexOf("/")), {
            recursive: true,
          });
          await Deno.writeFile(dest, data);
        }
        emit(
          "file",
          { specifier, path, kind, size: data.byteLength },
          new ReadableStream({
            start(controller) {
              controller.enqueue(data);
              controller.close();
            },
          }),
        );
      }
    } finally {
      core.close(rid);
    }
    emit("finish");
    resolveDone();
  })().catch((err) => {
    if (listeners.error.length > 0) {
      // Handled via the event; keep `done` from also surfacing as an
      // unhandled rejection for event-only consumers.
      done.catch(() => {});
      rejectDone(err);
      emit("error", err);
    } else {
      rejectDone(err);
    }
  });

  return emitter;
}

// Host surface. The user-worker pool sender is injected into op_state by the
// post-bootstrap hook before this runs, so `create` is functional immediately.
define("FlowRuntime", {
  userWorkers: {
    create: createUserWorker,
    tryCleanupIdleWorkers,
  },
  events,
  bundle,
  unbundle,
});
