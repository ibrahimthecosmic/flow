import { core, primordials } from "ext:core/mod.js";

import { FLOW_USER_WORKERS } from "ext:user_workers/user_workers.js";
import { applyFlowTag } from "ext:runtime/http.js";
import { waitUntil } from "ext:runtime/async_hook.js";
const {
  builtinTracer,
  enterSpan,
  METRICS_ENABLED,
  TRACING_ENABLED,
} = core.loadExtScript("ext:deno_telemetry/telemetry.ts");
import { exit as osExit } from "ext:os/exit.js";

const ops = core.ops;
const {
  ArrayIsArray,
  JSONParse,
  JSONStringify,
  ObjectDefineProperty,
  ObjectFreeze,
  ObjectValues,
} = primordials;

// Bootstrap-context keys owned by the runtime, not the embedder; they never
// show up in `Flow.context`.
const RUNTIME_CONTEXT_KEYS = ["terminationRequestToken"];

function deepFreeze(value) {
  if (value !== null && (typeof value === "object")) {
    const values = ArrayIsArray(value) ? value : ObjectValues(value);
    for (const inner of values) {
      deepFreeze(inner);
    }
    ObjectFreeze(value);
  }
  return value;
}

let trexMod;
function loadTrex() {
  if (trexMod === undefined) {
    try {
      trexMod = ops.op_lazy_load_esm("ext:trex/trex_lib.js");
    } catch {
      trexMod = null;
    }
  }
  return trexMod;
}

/**
 * @param {"user" | "main" | "event"} kind
 * @param {number} terminationRequestTokenRid
 */
function installTrexNamespace(kind, terminationRequestTokenRid) {
  /** TREX */

  const mod = loadTrex();

  let propsTrex = {
    scheduleTermination: () =>
      ops.op_cancel_drop_token(terminationRequestTokenRid),
  };

  switch (kind) {
    case "main":
      propsTrex = {
        userWorkers: FLOW_USER_WORKERS,
        getRuntimeMetrics: () => /* async */ ops.op_runtime_metrics(),
        applyFlowTag: (src, dest) => applyFlowTag(src, dest),
        systemMemoryInfo: () => ops.op_system_memory_info(),
        raiseSegfault: () => ops.op_raise_segfault(),
        ...(mod
          ? {
            PluginManager: mod.PluginManager,
            DatabaseManager: mod.DatabaseManager,
            userDatabaseManager: () => {
              return new mod.UserDatabaseManager(FLOW_USER_WORKERS);
            },
            TrexDB: mod.TrexDB,
            req: mod.req,
            createRequestListener: mod.createRequestListener,
            httpClient: (service) => {
              return new mod.TrexHttpClient(service);
            },
          }
          : {}),
        exit: (c) => osExit(c),
        ...propsTrex,
      };
      break;

    case "event":
      propsTrex = {
        ...propsTrex,
      };
      break;

    case "user":
      propsTrex = {
        waitUntil,
        ...(mod
          ? {
            req: mod.req,
            httpClient: (service) => {
              return new mod.TrexHttpClient(service);
            },
            tokioChannel: (service) => {
              return new mod.TrexHttpClient(service);
            },
            databaseManager: () => {
              return new mod.UserDatabaseManager(FLOW_USER_WORKERS);
            },
          }
          : {}),
      };
      break;
  }

  if (propsTrex === void 0) {
    return;
  }

  ObjectDefineProperty(globalThis, "Trex", {
    get() {
      return propsTrex;
    },
    configurable: true,
  });
}

/*
 * @param {"user" | "main" | "event"} kind
 * @param {number} terminationRequestTokenRid
 */
function installEdgeRuntimeNamespace(kind, terminationRequestTokenRid) {
  let props = {
    scheduleTermination: () =>
      ops.op_cancel_drop_token(terminationRequestTokenRid),
  };

  switch (kind) {
    case "main":
      props = {
        userWorkers: FLOW_USER_WORKERS,
        getRuntimeMetrics: () => /* async */ ops.op_runtime_metrics(),
        applyFlowTag: (src, dest) => applyFlowTag(src, dest),
        systemMemoryInfo: () => ops.op_system_memory_info(),
        raiseSegfault: () => ops.op_raise_segfault(),
        ...props,
      };
      break;

    case "event":
      props = {
        builtinTracer,
        enterSpan,
        METRICS_ENABLED,
        TRACING_ENABLED,
        ...props,
      };
      break;

    case "user":
      props = {
        waitUntil,
      };
      break;
  }

  if (props === void 0) {
    return;
  }

  ObjectDefineProperty(globalThis, "FlowRuntime", {
    get() {
      return props;
    },
    configurable: true,
  });
}

/**
 * @param {"user" | "main" | "event"} _kind
 * @param {object | undefined} bootstrapContext the merged bootstrap context
 *   (embedder extra context + the `context` object passed to
 *   `userWorkers.create`, plus runtime-owned keys, which are stripped)
 */
function installFlowNamespace(_kind, bootstrapContext) {
  let frozenContext;
  const props = {
    get context() {
      if (frozenContext === undefined) {
        // JSON round-trip: the context is JSON-derived by construction, and
        // this detaches Flow.context from the internal bootstrap object.
        const clone = JSONParse(JSONStringify(bootstrapContext ?? {}));
        for (const key of RUNTIME_CONTEXT_KEYS) {
          delete clone[key];
        }
        frozenContext = deepFreeze(clone);
      }
      return frozenContext;
    },
  };

  ObjectDefineProperty(globalThis, "Flow", {
    get() {
      return props;
    },
    configurable: true,
  });
}

export {
  installEdgeRuntimeNamespace,
  installFlowNamespace,
  installTrexNamespace,
};
