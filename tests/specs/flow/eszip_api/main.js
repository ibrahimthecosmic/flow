// Exercises the FlowRuntime.bundle / FlowRuntime.unbundle host API end to
// end; prints "ALL TESTS PASSED" on success.

function assert(cond, msg) {
  if (!cond) {
    throw new Error(`assertion failed: ${msg}`);
  }
}

async function collect(stream) {
  const chunks = [];
  let total = 0;
  for await (const chunk of stream) {
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

// 1. bundle an entrypoint path into eszip bytes
const eszipBytes = await collect(FlowRuntime.bundle("entry.js"));
assert(eszipBytes.byteLength > 0, "bundle produced bytes");

// 2. unbundle from bytes via the event API
const seen = new Map();
const job = FlowRuntime.unbundle(eszipBytes);
job.on("file", (meta, stream) => {
  seen.set(meta.path, { meta, bytes: collect(stream) });
});
let finished = false;
job.on("finish", () => {
  finished = true;
});
await job.done;
assert(finished, "finish event fired");
assert(seen.has("entry.js"), `entry.js was emitted (got: ${[...seen.keys()]})`);
assert(seen.has("helper.js"), "helper.js was emitted");
const helper = seen.get("helper.js");
assert(helper.meta.kind === "module", "helper.js has kind module");
assert(helper.meta.specifier.length > 0, "helper.js has a specifier");
const helperBytes = await helper.bytes;
assert(helper.meta.size === helperBytes.byteLength, "size matches the stream");
const helperText = new TextDecoder().decode(helperBytes);
assert(
  helperText.includes("export function greet"),
  `helper.js content round-tripped (got: ${helperText})`,
);

// 3. unbundle to an output directory (disk mode still emits events)
const outDir = await Deno.makeTempDir({ prefix: "flow_eszip_api_" });
try {
  const diskJob = FlowRuntime.unbundle(eszipBytes, outDir);
  let fileEvents = 0;
  diskJob.on("file", () => fileEvents++);
  await diskJob.done;
  assert(fileEvents >= 2, "output mode also emits file events");
  const entryOnDisk = await Deno.readTextFile(`${outDir}/entry.js`);
  assert(entryOnDisk.includes("greet"), "entry.js was extracted to disk");
  const helperOnDisk = await Deno.readTextFile(`${outDir}/helper.js`);
  assert(helperOnDisk === helperText, "disk and event contents agree");
} finally {
  await Deno.remove(outDir, { recursive: true });
}

// 4. bundle from in-memory source code (buffer and stream forms)
const inlineCode = new TextEncoder().encode(`console.log("inline module");`);
const eszipFromBuffer = await collect(FlowRuntime.bundle(inlineCode));
assert(eszipFromBuffer.byteLength > 0, "buffer-form bundle produced bytes");
const codeStream = new ReadableStream({
  start(controller) {
    controller.enqueue(inlineCode);
    controller.close();
  },
});
const eszipFromStream = await collect(FlowRuntime.bundle(codeStream));
assert(eszipFromStream.byteLength > 0, "stream-form bundle produced bytes");

// 5. unbundle from a path on disk
const eszipDir = await Deno.makeTempDir({ prefix: "flow_eszip_file_" });
try {
  const eszipPath = `${eszipDir}/bundle.eszip`;
  await Deno.writeFile(eszipPath, eszipBytes);
  const pathJob = FlowRuntime.unbundle(eszipPath);
  const paths = [];
  pathJob.on("file", (meta) => paths.push(meta.path));
  await pathJob.done;
  assert(paths.includes("entry.js"), "path-form unbundle works");
} finally {
  await Deno.remove(eszipDir, { recursive: true });
}

// 6. bundling a missing entrypoint errors the returned stream
let bundleErrored = false;
try {
  await collect(FlowRuntime.bundle("does-not-exist.js"));
} catch {
  bundleErrored = true;
}
assert(bundleErrored, "bundling a missing entrypoint errors the stream");

// 7. unbundling garbage fires "error" and rejects `done`
let unbundleErrored = false;
const badJob = FlowRuntime.unbundle(new Uint8Array([1, 2, 3, 4]));
badJob.on("error", () => {
  unbundleErrored = true;
});
let doneRejected = false;
await badJob.done.catch(() => {
  doneRejected = true;
});
assert(unbundleErrored, "unbundle of garbage fires the error event");
assert(doneRejected, "unbundle of garbage rejects done");

console.log("ALL TESTS PASSED");
