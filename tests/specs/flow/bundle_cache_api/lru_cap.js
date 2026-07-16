// Exercises FLOW_BUNDLE_CACHE_MAX_SIZE (set to 1 byte by __test__.jsonc):
// every admission is over cap, so the first put warns + admits over cap, the
// second put LRU-evicts the first, and BundleCache events for both surface
// on FlowRuntime.events. Prints "ALL TESTS PASSED".

function assert(cond, msg) {
  if (!cond) {
    throw new Error(`assertion failed: ${msg}`);
  }
}

async function collectBundle(entrypoint) {
  const chunks = [];
  let total = 0;
  for await (const chunk of FlowRuntime.bundle(entrypoint)) {
    chunks.push(chunk);
    total += chunk.byteLength;
  }
  const bytes = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return bytes;
}

const bundleOne = await collectBundle("service_one/index.js");
const bundleTwo = await collectBundle("service_two/index.js");

// Collect BundleCache events. The collector's first next() claims the
// stream; give the claim a beat to complete before cache activity starts so
// nothing is drained to stdio instead.
const seen = [];
let doneCollecting;
const collected = new Promise((resolve) => (doneCollecting = resolve));
const collector = (async () => {
  for await (const ev of FlowRuntime.events) {
    if (ev.event_type !== "BundleCache") continue;
    seen.push(ev);
    if (
      seen.some((e) => e.event.action === "overCap") &&
      seen.some((e) => e.event.action === "evicted" && e.event.path) &&
      seen.some((e) => e.event.action === "evicted" && e.event.cache_key)
    ) {
      doneCollecting();
      break; // hands the stream back
    }
  }
})();
await new Promise((r) => setTimeout(r, 100));

// 1. First put: nothing to evict, cap exceeded -> admitted over cap.
await FlowRuntime.bundleCache.put(bundleOne, {
  cacheKey: "lru/a",
  version: "1",
});

// 2. Second put: the (unpinned) first blob is the LRU victim.
await FlowRuntime.bundleCache.put(bundleTwo, {
  cacheKey: "lru/b",
  version: "1",
});

const stats = await FlowRuntime.bundleCache.stats();
assert(
  stats.entryCount === 1,
  `LRU kept only the newest blob (got ${stats.entryCount})`,
);
assert(
  stats.maxBytes === 1,
  `the cap is visible in stats (got ${stats.maxBytes})`,
);
assert(
  stats.totalBytes === bundleTwo.byteLength,
  "accounting matches the surviving blob",
);

// 3. Explicit evict emits with its cacheKey.
assert(
  (await FlowRuntime.bundleCache.evict({ cacheKey: "lru/b", version: "1" })) ===
    true,
  "explicit evict removes the survivor",
);
assert(
  (await FlowRuntime.bundleCache.stats()).entryCount === 0,
  "cache is empty",
);

// 4. All three event shapes arrived.
await Promise.race([
  collected,
  new Promise((_, reject) =>
    setTimeout(() => reject(new Error("timed out waiting for events")), 10_000)
  ),
]);
await collector;
const overCap = seen.find((e) => e.event.action === "overCap");
assert(overCap.event.max_bytes === 1, "overCap reports the cap");
assert(overCap.event.bytes > 0, "overCap reports the incoming size");
const lruEvicted = seen.find((e) =>
  e.event.action === "evicted" && e.event.path
);
assert(
  lruEvicted.event.path.endsWith(".eszip") && lruEvicted.event.bytes > 0,
  "LRU eviction names the blob and its size",
);
const explicit = seen.find((e) =>
  e.event.action === "evicted" && e.event.cache_key
);
assert(
  explicit.event.cache_key === "lru/b",
  "explicit evict names its cacheKey",
);
assert(
  explicit.metadata.service_path === null,
  "cache events carry no worker identity",
);

console.log("ALL TESTS PASSED");
Deno.exit(0);
