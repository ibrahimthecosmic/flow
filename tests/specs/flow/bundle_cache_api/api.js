// Exercises `FlowRuntime.bundleCache`: `put` seeding (path = move, bytes =
// spill) so URL boots need zero network, `evict` dropping an entry so the
// next boot re-downloads, and `stats` accounting. Prints "ALL TESTS PASSED".

function assert(cond, msg) {
  if (!cond) {
    throw new Error(`assertion failed: ${msg}`);
  }
}

function rpc(port, msg) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`rpc timed out: ${msg.kind}`)),
      30_000,
    );
    port.onmessage = (e) => {
      clearTimeout(timer);
      resolve(e.data);
    };
    port.postMessage(msg);
  });
}

async function expectEcho(worker, payload, tag) {
  const reply = await rpc(worker.port, { kind: "echo", payload });
  assert(reply.payload === payload, `worker echoes (${payload})`);
  if (tag !== undefined) {
    assert(reply.tag === tag, `worker is "${tag}" (got "${reply.tag}")`);
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
  assert(bytes.byteLength > 0, `bundle produced an eszip (${entrypoint})`);
  return bytes;
}

const bundleOne = await collectBundle("service_one/index.js");
const bundleTwo = await collectBundle("service_two/index.js");

// /a.eszip always 500s: a seeded, version-pinned boot must never reach it.
// /b.eszip serves for the post-evict re-download.
const requests = new Map();
const server = Deno.serve({ port: 0, onListen() {} }, (req) => {
  const { pathname } = new URL(req.url);
  requests.set(pathname, (requests.get(pathname) ?? 0) + 1);
  switch (pathname) {
    case "/a.eszip":
      return new Response("must not be fetched", { status: 500 });
    case "/b.eszip":
      return new Response(bundleTwo, { headers: { etag: '"b-v1"' } });
    default:
      return new Response("unknown path", { status: 400 });
  }
});
const base = `http://localhost:${server.addr.port}`;
const count = (path) => requests.get(path) ?? 0;

// 1. put(path): the source file is MOVED into the cache...
const seedPath = "./seed-a.eszip";
await Deno.writeFile(seedPath, bundleOne);
const { bundlePath } = await FlowRuntime.bundleCache.put(seedPath, {
  cacheKey: "seed/a",
  version: "1",
});
assert(
  typeof bundlePath === "string" && bundlePath.length > 0,
  "put returns the blob path",
);
assert(
  !(await Deno.stat(seedPath).then(() => true, () => false)),
  "put(path) consumed the source file",
);
assert(
  await Deno.stat(bundlePath).then((s) => s.isFile, () => false),
  "the blob landed in the cache",
);

// ...and a version-pinned URL boot against the seed needs no network at all
// (the server would 500 it).
const viaSeed = await FlowRuntime.userWorkers.create({
  maybeEszip: { url: `${base}/a.eszip`, cacheKey: "seed/a", version: "1" },
  forceCreate: true,
});
await expectEcho(viaSeed, "seeded-path", "one");
assert(count("/a.eszip") === 0, "the seeded boot made zero requests");

// 2. put(bytes) seeds the same way.
await FlowRuntime.bundleCache.put(bundleTwo, {
  cacheKey: "seed/b",
  version: "1",
});
const viaBytes = await FlowRuntime.userWorkers.create({
  maybeEszip: { url: `${base}/b.eszip`, cacheKey: "seed/b", version: "1" },
  forceCreate: true,
});
await expectEcho(viaBytes, "seeded-bytes", "two");
assert(count("/b.eszip") === 0, "the bytes-seeded boot made zero requests");

// 3. stats: two distinct bundles are cached; live workers pin their blobs.
const stats = await FlowRuntime.bundleCache.stats();
assert(stats.entryCount === 2, `two blobs cached (got ${stats.entryCount})`);
assert(stats.totalBytes > 0, "totalBytes is accounted");
assert(stats.maxBytes === null, "no cap configured");
assert(
  stats.pinnedBytes > 0 && stats.pinnedBytes <= stats.totalBytes,
  `live workers pin their blobs (pinned ${stats.pinnedBytes} of ${stats.totalBytes})`,
);

// 4. evict is exact and idempotent...
assert(
  (await FlowRuntime.bundleCache.evict({
    cacheKey: "seed/b",
    version: "1",
  })) === true,
  "evict removes the entry",
);
assert(
  (await FlowRuntime.bundleCache.evict({
    cacheKey: "seed/b",
    version: "1",
  })) === false,
  "a second evict finds nothing",
);

// ...the running worker keeps serving...
await expectEcho(viaBytes, "post-evict", "two");

// ...and the next create for the key misses and re-downloads.
const reFetched = await FlowRuntime.userWorkers.create({
  maybeEszip: { url: `${base}/b.eszip`, cacheKey: "seed/b", version: "1" },
  forceCreate: true,
});
await expectEcho(reFetched, "refetched", "two");
assert(count("/b.eszip") === 1, "the post-evict boot re-downloaded");

console.log("ALL TESTS PASSED");
await server.shutdown();
Deno.exit(0);
