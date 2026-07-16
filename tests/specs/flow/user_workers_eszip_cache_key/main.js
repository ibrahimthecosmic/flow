// Exercises the `cacheKey` option of the `{ url, ... }` form of
// `maybeEszip`: rotating (presigned-style) urls converge on one cached
// download, recorded validators revalidate across different urls, the
// cacheKey namespaces never collide with plain-url entries, and
// `cacheKey ?? url` is the pool-key default. Prints "ALL TESTS PASSED".

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

// Object-store stand-in: serves by pathname (the rotating "signature" query
// is ignored, like a presigned url's), counts requests per pathname, and
// answers If-None-Match with 304.
const requests = new Map(); // pathname -> [{headers, search}]
const ETAG = '"bundle-v1"';
const server = Deno.serve({ port: 0, onListen() {} }, (req) => {
  const { pathname, search } = new URL(req.url);
  const seen = requests.get(pathname) ?? [];
  seen.push({ headers: req.headers, search });
  requests.set(pathname, seen);

  switch (pathname) {
    case "/one.eszip":
      if (req.headers.get("if-none-match") === ETAG) {
        return new Response(null, { status: 304, headers: { etag: ETAG } });
      }
      return new Response(bundleOne, { headers: { etag: ETAG } });
    case "/two.eszip":
      return new Response(bundleTwo, { headers: { etag: '"two-v1"' } });
    default:
      return new Response("unknown path", { status: 400 });
  }
});
const base = `http://localhost:${server.addr.port}`;
const count = (path) => (requests.get(path) ?? []).length;
// Every call yields a different url for the same object, like presign().
let sig = 0;
const presign = (path) => `${base}${path}?sig=${++sig}`;

// 1. version pin + cacheKey: three creates through three DIFFERENT urls
// download exactly once
for (let i = 0; i < 3; i++) {
  const worker = await FlowRuntime.userWorkers.create({
    maybeEszip: {
      url: presign("/one.eszip"),
      cacheKey: "flows/1/one.eszip",
      version: "v1",
    },
    forceCreate: true,
  });
  await expectEcho(worker, `pinned-${i}`, "one");
}
assert(
  count("/one.eszip") === 1,
  `rotating urls with a pinned cacheKey download once (got ${
    count("/one.eszip")
  })`,
);

// 2. unversioned cacheKey: the recorded ETag revalidates a DIFFERENT url
// (the validators belong to the resource, not the url)
const unversioned = { cacheKey: "flows/1/one-unversioned.eszip" };
const firstUrl = presign("/one.eszip");
const viaFirst = await FlowRuntime.userWorkers.create({
  maybeEszip: { url: firstUrl, ...unversioned },
  forceCreate: true,
});
await expectEcho(viaFirst, "unversioned-1", "one");
const before = count("/one.eszip");
const viaSecond = await FlowRuntime.userWorkers.create({
  maybeEszip: { url: presign("/one.eszip"), ...unversioned },
  forceCreate: true,
});
await expectEcho(viaSecond, "unversioned-2", "one");
assert(
  count("/one.eszip") === before + 1,
  "the second unversioned create revalidates (one conditional request)",
);
const revalidation = requests.get("/one.eszip").at(-1);
assert(
  revalidation.headers.get("if-none-match") === ETAG,
  "revalidation reused the validators recorded under the cacheKey",
);

// 3. namespaces: a plain-url create for the SAME url string as an existing
// cacheKey uses its own manifest entry (downloads despite the cacheKey hit
// above using different urls). The url here has a fixed query so the string
// is stable and provably distinct from the cacheKey namespace.
const collisionKey = "collision-check";
const stableUrl = `${base}/two.eszip?stable=1`;
const viaKeyed = await FlowRuntime.userWorkers.create({
  maybeEszip: { url: stableUrl, cacheKey: collisionKey, version: "v1" },
  forceCreate: true,
});
await expectEcho(viaKeyed, "keyed", "two");
// A cacheKey EQUAL to that url string is still its own namespace: this
// entry was never recorded, so it must download, not hit the keyed entry.
const viaKeyEqualUrl = await FlowRuntime.userWorkers.create({
  maybeEszip: { url: stableUrl, cacheKey: stableUrl, version: "v1" },
  forceCreate: true,
});
await expectEcho(viaKeyEqualUrl, "key-equal-url", "two");
assert(
  count("/two.eszip") === 2,
  `a cacheKey equal to a url string is a distinct cache entry (got ${
    count("/two.eszip")
  })`,
);

// 4. pool identity: with no servicePath and no forceCreate, the same
// cacheKey through rotating urls reuses ONE pooled worker...
const pooledA = await FlowRuntime.userWorkers.create({
  maybeEszip: {
    url: presign("/one.eszip"),
    cacheKey: "pool/one.eszip",
    version: "v1",
  },
});
const pooledB = await FlowRuntime.userWorkers.create({
  maybeEszip: {
    url: presign("/one.eszip"),
    cacheKey: "pool/one.eszip",
    version: "v1",
  },
});
assert(
  pooledA.key === pooledB.key,
  "a stable cacheKey keeps one pool identity across rotating urls",
);
await expectEcho(pooledA, "pool-a", "one");

// ...and a different cacheKey is a different pool identity.
const pooledTwo = await FlowRuntime.userWorkers.create({
  maybeEszip: {
    url: presign("/two.eszip"),
    cacheKey: "pool/two.eszip",
    version: "v1",
  },
});
assert(
  pooledTwo.key !== pooledA.key,
  "distinct cacheKeys get distinct pooled workers",
);
await expectEcho(pooledTwo, "pool-two", "two");

console.log("ALL TESTS PASSED");
await server.shutdown();
Deno.exit(0);
