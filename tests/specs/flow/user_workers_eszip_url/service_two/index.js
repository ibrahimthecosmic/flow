// Echo worker "two" — distinct tag from service_one; a pool-key collision
// between two different URLs would hand back a "one"-tagged worker.
FlowRuntime.parentPort.onmessage = (e) => {
  FlowRuntime.parentPort.postMessage({
    kind: "echo",
    tag: "two",
    payload: e.data.payload,
  });
};
console.log("eszip url worker booted (two)");
