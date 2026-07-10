import { core, primordials } from "ext:core/mod.js";

import { waitUntil } from "ext:runtime/async_hook.js";

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
// show up in `FlowRuntime.context`.
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

/**
 * Legacy compatibility alias (trex-runtime lineage). Prefer `FlowRuntime`.
 * @param {number} terminationRequestTokenRid
 */
function installTrexNamespace(terminationRequestTokenRid) {
  const propsTrex = {
    waitUntil,
    scheduleTermination: () =>
      ops.op_cancel_drop_token(terminationRequestTokenRid),
  };

  ObjectDefineProperty(globalThis, "Trex", {
    get() {
      return propsTrex;
    },
    configurable: true,
  });
}

/*
 * The worker-side `FlowRuntime` surface. (The host-side counterpart is
 * installed by edge/cli/src/flow_main.js in the flow main isolate.)
 *
 * @param {object | undefined} ctx the merged bootstrap context (embedder extra
 *   context + the `context` passed to `userWorkers.create`, plus runtime-owned
 *   keys, which are stripped from the public `context` getter installed below)
 */
function installEdgeRuntimeNamespace(ctx) {
  const terminationRequestTokenRid = ctx?.terminationRequestToken;

  const props = {
    waitUntil,
    // A worker's sole graceful self-exit (Deno.exit is a no-op in the sandbox).
    scheduleTermination: () =>
      ops.op_cancel_drop_token(terminationRequestTokenRid),
  };

  // The JSON `context` this worker was created with — deep-frozen and
  // memoized, runtime-owned keys stripped.
  let frozenContext;
  ObjectDefineProperty(props, "context", {
    get() {
      if (frozenContext === undefined) {
        // JSON round-trip: the context is JSON-derived by construction, and
        // this detaches the public context from the internal bootstrap object.
        const clone = JSONParse(JSONStringify(ctx ?? {}));
        for (const key of RUNTIME_CONTEXT_KEYS) {
          delete clone[key];
        }
        frozenContext = deepFreeze(clone);
      }
      return frozenContext;
    },
    enumerable: true,
    configurable: true,
  });

  ObjectDefineProperty(globalThis, "FlowRuntime", {
    get() {
      return props;
    },
    configurable: true,
  });
}

export { installEdgeRuntimeNamespace, installTrexNamespace };
