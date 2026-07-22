// Regression test for reading bundled static assets from a `servicePath`
// (unbundled/on-the-fly) user worker — the konnecthub draft-run shape.
//
// Reproduces the exact conditions that broke it end to end:
//   - the servicePath workdir lives under the system temp dir (`/tmp`), which
//     collides with the worker's writable `/tmp` scratch overlay,
//   - the entrypoint is in a subdirectory (`.kh/`), so it is NOT the workdir
//     root that static assets resolve against,
//   - `staticPatterns` are absolute, workdir-rooted paths,
//   - permissions are a non-empty (NOT all-granted) object, so relative-path
//     resolution goes through the worker's cwd override.
//
// Asserts: a relative `Deno.readFile` of a static asset succeeds; a nested
// asset succeeds; an absolute path to the same asset succeeds; a non-static
// file under the workdir is NOT readable (sandbox); and a `staticPatterns`
// entry OUTSIDE the servicePath is refused (confinement).
// Prints "ALL TESTS PASSED" on success.

function assert(cond, msg) {
  if (!cond) {
    throw new Error(`assertion failed: ${msg}`);
  }
}

// Workdir under the system temp dir — the layout that collided with the tmp
// overlay. Kept off the harness temp cwd on purpose.
const workdir = await Deno.makeTempDir({ prefix: "flow-workdir-" });
await Deno.mkdir(`${workdir}/.kh`, { recursive: true });
await Deno.mkdir(`${workdir}/assets/nested`, { recursive: true });
await Deno.writeTextFile(`${workdir}/assets/top.txt`, "hello-static-top");
await Deno.writeTextFile(`${workdir}/assets/nested/deep.txt`, "deep-content");
// Present on disk but never marked static — must stay unreadable.
await Deno.writeTextFile(`${workdir}/secret.txt`, "not-static");

// A file outside the servicePath, offered as a static pattern — confinement
// must refuse to embed it, so the worker can never read it back.
const outside = await Deno.makeTempDir({ prefix: "flow-outside-" });
await Deno.writeTextFile(`${outside}/escape.txt`, "should-not-embed");

const worker = `
FlowRuntime.parentPort.onmessage = async (e) => {
  try {
    const bytes = await Deno.readFile(e.data.path);
    FlowRuntime.parentPort.postMessage({
      ok: true,
      content: new TextDecoder().decode(bytes),
    });
  } catch (err) {
    FlowRuntime.parentPort.postMessage({ ok: false, error: String(err) });
  }
};
`;
await Deno.writeTextFile(`${workdir}/.kh/orchestrator.ts`, worker);

const LIMITS = {
  workerTimeoutMs: 120_000,
  cpuTimeSoftLimitMs: 10_000,
  cpuTimeHardLimitMs: 20_000,
};

const w = await FlowRuntime.userWorkers.create({
  servicePath: workdir,
  maybeEntrypoint: ".kh/orchestrator.ts",
  forceCreate: true,
  staticPatterns: [
    `${workdir}/assets/top.txt`,
    `${workdir}/assets/nested/deep.txt`,
    `${outside}/escape.txt`, // outside servicePath -> refused by confinement
  ],
  // Non-empty (NOT all-granted) permissions with blanket read, so relative
  // reads resolve through the worker cwd override rather than short-circuiting.
  permissions: { allow_read: [], allow_env: [], allow_net: [] },
  context: { mode: "execute", sourceMap: true },
  ...LIMITS,
});

function read(path) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`rpc timed out reading ${path}`)),
      30_000,
    );
    w.port.onmessage = (e) => {
      clearTimeout(timer);
      resolve(e.data);
    };
    w.port.postMessage({ path });
  });
}

// 1. relative read of a static asset
const top = await read("assets/top.txt");
assert(
  top.ok && top.content === "hello-static-top",
  `relative static read failed: ${JSON.stringify(top)}`,
);

// 2. nested static asset
const deep = await read("assets/nested/deep.txt");
assert(
  deep.ok && deep.content === "deep-content",
  `nested static read failed: ${JSON.stringify(deep)}`,
);

// 3. absolute path to the same asset also resolves
const abs = await read(`${workdir}/assets/top.txt`);
assert(
  abs.ok && abs.content === "hello-static-top",
  `absolute static read failed: ${JSON.stringify(abs)}`,
);

// 4. a non-static file under the workdir is NOT readable (sandbox boundary)
const secret = await read("secret.txt");
assert(
  !secret.ok,
  `non-static file must not be readable: ${JSON.stringify(secret)}`,
);

// 5. confinement: an out-of-servicePath staticPattern is never embedded, so
//    the worker cannot read it back
const escaped = await read(`${outside}/escape.txt`);
assert(
  !escaped.ok,
  `out-of-servicePath asset must not be readable: ${JSON.stringify(escaped)}`,
);

console.log("ALL TESTS PASSED");
Deno.exit(0);
