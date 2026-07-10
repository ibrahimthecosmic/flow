// Exercises the rest of the declared FlowRuntime surface (see
// edge/ext/runtime/lib.flow.d.ts): host-side member shapes and `context`,
// worker keys and pool reuse vs `forceCreate`, worker-side version globals and
// `parentPorts`, the `permissions` create option, `maybeModuleCode` and
// `maybeEszip` boots, `inspect()` without an inspector, events-stream claiming
// and lifecycle event types, and `scheduleTermination`.
// Prints "ALL TESTS PASSED" on success.

function assert(cond, msg) {
  if (!cond) {
    throw new Error(`assertion failed: ${msg}`);
  }
}

function rpc(port, msg, transfer = []) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`rpc timed out: ${msg.kind}`)),
      30_000,
    );
    port.onmessage = (e) => {
      clearTimeout(timer);
      resolve(e.data);
    };
    port.postMessage(msg, transfer);
  });
}

// 1. host-side surface shape
assert(
  typeof FlowRuntime.userWorkers.create === "function",
  "userWorkers.create is a function",
);
assert(
  typeof FlowRuntime.userWorkers.tryCleanupIdleWorkers === "function",
  "userWorkers.tryCleanupIdleWorkers is a function",
);
assert(typeof FlowRuntime.bundle === "function", "bundle is a function");
assert(typeof FlowRuntime.unbundle === "function", "unbundle is a function");
assert(
  typeof FlowRuntime.events[Symbol.asyncIterator] === "function",
  "events is async-iterable",
);

// 2. host-side context: empty and frozen (the main isolate was not created
//    via create(), but the accessor reads the same way as in workers)
assert(
  typeof FlowRuntime.context === "object" && FlowRuntime.context !== null,
  "host FlowRuntime.context is an object",
);
assert(Object.isFrozen(FlowRuntime.context), "host context is frozen");
assert(
  Object.keys(FlowRuntime.context).length === 0,
  "host context is empty",
);

// 3. claim the events stream before booting anything
const seenEvents = [];
let stopCollecting;
const collectingDone = (async () => {
  for await (const ev of FlowRuntime.events) {
    seenEvents.push(ev);
    if (stopCollecting) break;
  }
})();

// 4. worker keys: pool reuse hands back the same worker, forceCreate does not
const a = await FlowRuntime.userWorkers.create({ servicePath: "./service" });
assert(
  typeof a.key === "string" && a.key.length > 0,
  "worker.key is a non-empty string",
);
const b = await FlowRuntime.userWorkers.create({ servicePath: "./service" });
assert(b.key === a.key, "same servicePath reuses the worker (same key)");
const c = await FlowRuntime.userWorkers.create({
  servicePath: "./service",
  forceCreate: true,
});
assert(c.key !== a.key, "forceCreate boots a distinct worker");

// exercise b's channel so the extra-port delivery is observable on the worker
const viaB = await rpc(b.port, { kind: "echo", payload: "via-b" });
assert(viaB.payload === "via-b", "reused worker's own channel works");

// 5. worker-side parentPorts bookkeeping
const portsA = await rpc(a.port, { kind: "ports" });
assert(
  portsA.count >= 2,
  `reused worker collected the extra port (got ${portsA.count})`,
);
assert(portsA.firstIsParentPort, "parentPorts[0] is parentPort");
const portsC = await rpc(c.port, { kind: "ports" });
assert(portsC.count === 1, "fresh worker has exactly its first port");

// 6. version globals inside the worker
const versions = await rpc(c.port, { kind: "versions" });
assert(
  typeof versions.flow === "string" && versions.flow.length > 0,
  "FLOW_VERSION is set in workers",
);
assert(
  typeof versions.deno === "string" && versions.deno.length > 0,
  "DENO_VERSION is set in workers",
);
assert(
  String(versions.reported).includes("flow-runtime-"),
  `worker Deno.version.deno reports the flow variant (got: ${versions.reported})`,
);

// 7. the events stream is single-consumer while claimed
let dualClaimRejected = false;
await FlowRuntime.events[Symbol.asyncIterator]().next().then(
  () => {},
  (err) => {
    dualClaimRejected = String(err).includes("claimed");
  },
);
assert(dualClaimRejected, "second events consumer rejects with a claim error");

// 8. inspect() explains itself when the inspector is off
let inspectThrew = false;
try {
  a.inspect();
} catch (err) {
  inspectThrew = String(err).includes("inspector is not enabled");
}
assert(inspectThrew, "inspect() throws with instructions");

// 9. a failed create() rejects and leaves the pool healthy
let createRejected = false;
await FlowRuntime.userWorkers.create({ servicePath: "./no-such-dir" }).catch(
  () => {
    createRejected = true;
  },
);
assert(createRejected, "create() rejects for a missing servicePath");
const afterReject = await rpc(a.port, { kind: "echo", payload: "still-alive" });
assert(
  afterReject.payload === "still-alive",
  "pool stays healthy after a failed create",
);

// 10. the permissions option applies (deny_net blocks outbound connect)
const p = await FlowRuntime.userWorkers.create({
  servicePath: "./service",
  forceCreate: true,
  permissions: { allow_all: true, deny_net: ["127.0.0.1"] },
});
const conn = await rpc(p.port, {
  kind: "connect",
  hostname: "127.0.0.1",
  port: 1,
});
assert(
  conn.denied,
  `deny_net blocks Deno.connect (got: ${conn.error ?? "connected"})`,
);

// 11. maybeModuleCode boots from inline source (servicePath is the pool key)
const m = await FlowRuntime.userWorkers.create({
  servicePath: "./service",
  forceCreate: true,
  maybeModuleCode: "FlowRuntime.parentPort.onmessage = (e) => {" +
    " FlowRuntime.parentPort.postMessage({ kind: 'echo', payload: e.data.payload });" +
    " };",
});
const viaM = await rpc(m.port, { kind: "echo", payload: "module-code" });
assert(viaM.payload === "module-code", "maybeModuleCode worker echoes");

// 12. bundle() -> create({ maybeEszip }) round trip, with no servicePath
const chunks = [];
for await (const chunk of FlowRuntime.bundle("eszip_service/index.js")) {
  chunks.push(chunk);
}
let total = 0;
for (const chunk of chunks) total += chunk.byteLength;
const eszipBytes = new Uint8Array(total);
{
  let offset = 0;
  for (const chunk of chunks) {
    eszipBytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
}
assert(eszipBytes.byteLength > 0, "bundle produced an eszip");
const z = await FlowRuntime.userWorkers.create({ maybeEszip: eszipBytes });
const viaZ = await rpc(z.port, { kind: "echo", payload: "eszip" });
assert(viaZ.payload === "eszip", "maybeEszip worker echoes");

// 13. tryCleanupIdleWorkers resolves (nothing is idle for 10 minutes)
await FlowRuntime.userWorkers.tryCleanupIdleWorkers(600_000);

// 14. scheduleTermination() surfaces as a TerminationRequested shutdown
const shutdownsBefore =
  seenEvents.filter((ev) => ev.event_type === "Shutdown").length;
await rpc(c.port, { kind: "terminate" });
let sawTermination = false;
const deadline = Date.now() + 30_000;
while (Date.now() < deadline) {
  sawTermination = seenEvents
    .filter((ev) => ev.event_type === "Shutdown")
    .slice(shutdownsBefore)
    .some((ev) => ev.event.reason === "TerminationRequested");
  if (sawTermination) break;
  await new Promise((r) => setTimeout(r, 250));
}
assert(
  sawTermination,
  `scheduleTermination produced a TerminationRequested shutdown (shutdowns: ${
    JSON.stringify(
      seenEvents.filter((ev) => ev.event_type === "Shutdown"),
    )
  })`,
);

// 15. stop the collector and check the lifecycle event types went through
stopCollecting = true;
await rpc(a.port, { kind: "log", payload: "collector-stop" });
await collectingDone;
assert(
  seenEvents.some((ev) => ev.event_type === "Boot"),
  "Boot events observed",
);
assert(
  seenEvents.some((ev) => ev.event_type === "BootFailure"),
  "BootFailure observed for the failed create",
);
assert(
  seenEvents.some((ev) =>
    ev.event_type === "Log" && String(ev.event.msg).includes("worker booted")
  ),
  "worker logs flow through the events stream",
);

console.log("ALL TESTS PASSED");
