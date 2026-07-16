// Exercises the `{ url, headers?, version? }` form of `maybeEszip`: plain
// download, ETag revalidation (304), version pinning (zero requests on a
// hit), stale-if-error on 5xx, 4xx rejection, and the URL-as-pool-key
// default. Prints "ALL TESTS PASSED" on success.

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

// Registry stand-in: counts requests per path, records headers, answers
// If-None-Match with 304, and turns /flaky.eszip into a 500 after its first
// download (stale-if-error case).
const requests = new Map(); // path -> [{headers}]
const ETAG = '"bundle-v1"';
const server = Deno.serve({ port: 0, onListen() {} }, (req) => {
  const { pathname } = new URL(req.url);
  const seen = requests.get(pathname) ?? [];
  seen.push({ headers: req.headers });
  requests.set(pathname, seen);

  switch (pathname) {
    case "/missing.eszip":
      return new Response("not here", { status: 404 });
    case "/flaky.eszip":
      if (seen.length > 1) {
        return new Response("boom", { status: 500 });
      }
      return new Response(bundleOne, { headers: { etag: ETAG } });
    case "/app.eszip":
      if (req.headers.get("if-none-match") === ETAG) {
        return new Response(null, { status: 304, headers: { etag: ETAG } });
      }
      return new Response(bundleOne, { headers: { etag: ETAG } });
    case "/versioned.eszip":
      // Deliberately no validators: only `version` can skip the download.
      return new Response(bundleOne);
    case "/two.eszip":
      return new Response(bundleTwo, { headers: { etag: '"two-v1"' } });
    default:
      return new Response("unknown path", { status: 400 });
  }
});
const base = `http://localhost:${server.addr.port}`;
const count = (path) => (requests.get(path) ?? []).length;

// 1. plain URL create: downloads once, forwards custom headers
const appUrl = `${base}/app.eszip`;
const viaUrl = await FlowRuntime.userWorkers.create({
  maybeEszip: {
    url: appUrl,
    headers: { authorization: "Bearer test-token", "x-custom": "yes" },
  },
  forceCreate: true,
});
await expectEcho(viaUrl, "url", "one");
assert(count("/app.eszip") === 1, "first create downloads once");
const first = requests.get("/app.eszip")[0];
assert(
  first.headers.get("authorization") === "Bearer test-token" &&
    first.headers.get("x-custom") === "yes",
  "custom headers reach the server",
);

// 2. unversioned re-create: conditional request answered 304, cache reused
const viaRevalidate = await FlowRuntime.userWorkers.create({
  maybeEszip: { url: appUrl },
  forceCreate: true,
});
await expectEcho(viaRevalidate, "revalidated", "one");
assert(count("/app.eszip") === 2, "second create revalidates");
assert(
  requests.get("/app.eszip")[1].headers.get("if-none-match") === ETAG,
  "revalidation is conditional (If-None-Match)",
);

// 3. version pin: second create with the same version makes NO request
const versionedUrl = `${base}/versioned.eszip`;
for (let i = 0; i < 2; i++) {
  const worker = await FlowRuntime.userWorkers.create({
    maybeEszip: { url: versionedUrl, version: "1.0.0" },
    forceCreate: true,
  });
  await expectEcho(worker, `pinned-${i}`, "one");
}
assert(count("/versioned.eszip") === 1, "a pinned version downloads once");

// 4. a new version string re-downloads
const bumped = await FlowRuntime.userWorkers.create({
  maybeEszip: { url: versionedUrl, version: "2.0.0" },
  forceCreate: true,
});
await expectEcho(bumped, "bumped", "one");
assert(count("/versioned.eszip") === 2, "a bumped version re-downloads");

// 5. stale-if-error: revalidation 500 falls back to the cached copy
const flakyUrl = `${base}/flaky.eszip`;
const flakyFirst = await FlowRuntime.userWorkers.create({
  maybeEszip: { url: flakyUrl },
  forceCreate: true,
});
await expectEcho(flakyFirst, "flaky-1", "one");
const flakySecond = await FlowRuntime.userWorkers.create({
  maybeEszip: { url: flakyUrl },
  forceCreate: true,
});
await expectEcho(flakySecond, "flaky-2", "one");
assert(count("/flaky.eszip") === 2, "the 500 came from a real revalidation");

// 6. definitive 4xx rejects create()
let rejected = false;
try {
  await FlowRuntime.userWorkers.create({
    maybeEszip: { url: `${base}/missing.eszip` },
    forceCreate: true,
  });
} catch (e) {
  rejected = true;
  assert(
    e.message.includes("404") && e.message.includes("/missing.eszip"),
    `404 error names the URL and status (${e.message})`,
  );
}
assert(rejected, "a 404 rejects create()");

// 7. pool-key default: two URLs, no servicePath, no forceCreate -> two
// distinct workers (a collision would hand back the "one"-tagged worker)
const pooledTwo = await FlowRuntime.userWorkers.create({
  maybeEszip: { url: `${base}/two.eszip` },
});
await expectEcho(pooledTwo, "pool-two", "two");
const pooledOne = await FlowRuntime.userWorkers.create({
  maybeEszip: { url: appUrl },
});
await expectEcho(pooledOne, "pool-one", "one");

console.log("ALL TESTS PASSED");
await server.shutdown();
Deno.exit(0);
