const ctx = (globalThis as unknown as {
  Flow: { context: Record<string, unknown> };
}).Flow.context;

if (ctx.flavor !== "meow") {
  throw new Error("Flow.context is missing an embedder context value");
}

const nested = ctx.nested as { a: number[] };
if (nested.a[1] !== 2) {
  throw new Error("Flow.context is missing a nested context value");
}

if ("terminationRequestToken" in ctx) {
  throw new Error("Flow.context leaked a runtime-owned key");
}

if (!Object.isFrozen(ctx) || !Object.isFrozen(nested) || !Object.isFrozen(nested.a)) {
  throw new Error("Flow.context is not deep-frozen");
}

// memoized: same object on every access
if (ctx !== (globalThis as unknown as {
    Flow: { context: Record<string, unknown> };
  }).Flow.context) {
  throw new Error("Flow.context is not memoized");
}
