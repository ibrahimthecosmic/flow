const flowRuntime = (globalThis as unknown as {
  FlowRuntime: {
    context: Record<string, unknown>;
    scheduleTermination?: unknown;
  };
}).FlowRuntime;

const ctx = flowRuntime.context;

if (ctx.flavor !== "meow") {
  throw new Error("FlowRuntime.context is missing an embedder context value");
}

const nested = ctx.nested as { a: number[] };
if (nested.a[1] !== 2) {
  throw new Error("FlowRuntime.context is missing a nested context value");
}

if ("terminationRequestToken" in ctx) {
  throw new Error("FlowRuntime.context leaked a runtime-owned key");
}

if (!Object.isFrozen(ctx) || !Object.isFrozen(nested) || !Object.isFrozen(nested.a)) {
  throw new Error("FlowRuntime.context is not deep-frozen");
}

// memoized: same object on every access
if (
  ctx !== (globalThis as unknown as {
    FlowRuntime: { context: Record<string, unknown> };
  }).FlowRuntime.context
) {
  throw new Error("FlowRuntime.context is not memoized");
}

// The self-termination hook must be present (a worker's only graceful
// self-exit; regression guard for it being dropped from the namespace).
if (typeof flowRuntime.scheduleTermination !== "function") {
  throw new Error("FlowRuntime.scheduleTermination is missing");
}
