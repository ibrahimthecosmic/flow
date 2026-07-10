// Exercises the FlowRuntime.userWorkers MessagePort comm surface end to end;
// prints "ALL TESTS PASSED" on success.

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

// 1. claim the worker events stream before anything boots
const seenEvents = [];
let stopCollecting;
const collectingDone = (async () => {
  for await (const ev of FlowRuntime.events) {
    seenEvents.push(ev);
    if (stopCollecting) break;
  }
})();

// 2. create a worker and do a structured-clone echo round trip
const worker = await FlowRuntime.userWorkers.create({
  servicePath: "./service",
  context: { flavor: "meow" },
  envVars: [["WORKER_SECRET", "s3cr3t"]],
});
assert(worker.port, "worker has a MessagePort");

const echoed = await rpc(worker.port, {
  kind: "echo",
  payload: { n: 42, arr: [1, 2, 3] },
});
assert(echoed.kind === "echo", "echo reply kind");
assert(echoed.payload.n === 42, "echo payload survives structured clone");
assert(echoed.payload.arr.length === 3, "echo array survives");

// 3. FlowRuntime.context inside the worker reflects create({ context })
const ctxReply = await rpc(worker.port, { kind: "context" });
assert(ctxReply.context.flavor === "meow", "context passed through");
assert(
  ctxReply.context.terminationRequestToken === undefined,
  "runtime-owned context keys are stripped",
);

// 4. envVars land in the worker env (and nothing is inherited)
const envReply = await rpc(worker.port, { kind: "env" });
assert(envReply.value === "s3cr3t", "envVars are visible in the worker");

// 5. sandbox posture: no HTTP serving, Deno.exit is a no-op
const sandbox = await rpc(worker.port, { kind: "sandbox" });
assert(sandbox.serveDenied, "Deno.serve is denied in workers");
assert(sandbox.exitIsNoop, "Deno.exit is a no-op in workers");
assert(sandbox.argsLength === 0, "Deno.args is empty in workers");

// 6. zero-copy ArrayBuffer transfer both ways
const buf = new Uint8Array([0, 1, 2, 250]).buffer;
const bytesReply = await rpc(worker.port, { kind: "bytes", buf }, [buf]);
assert(buf.byteLength === 0, "transferred buffer is detached on the host");
const roundTripped = new Uint8Array(bytesReply.buf);
assert(
  roundTripped[0] === 255 && roundTripped[3] === 5,
  `worker transformed the bytes in place (got: ${[...roundTripped]})`,
);

// 7. pool reuse: a second create() for the same servicePath attaches an
//    extra channel to the SAME worker (SharedWorker-style)
const worker2 = await FlowRuntime.userWorkers.create({
  servicePath: "./service",
});
assert(worker2.port, "reused worker create() still hands out a port");
const echoed2 = await rpc(worker2.port, { kind: "echo", payload: "second" });
assert(echoed2.payload === "second", "second channel echoes");

// 8. the events stream saw the worker's console output. Ask the worker to
//    emit one more log so the collector observes an event after the stop
//    flag is raised and exits its loop (releasing the events claim).
stopCollecting = true;
await rpc(worker.port, { kind: "log", payload: "collector-stop" });
await collectingDone;
assert(
  seenEvents.some((ev) =>
    ev.event?.Log && String(ev.event.Log.msg).includes("worker booted")
  ),
  `events stream saw the boot log (got: ${JSON.stringify(seenEvents)})`,
);

console.log("ALL TESTS PASSED");
