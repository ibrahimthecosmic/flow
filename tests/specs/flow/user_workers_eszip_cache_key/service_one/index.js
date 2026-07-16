// Echo worker "one" — the tag proves WHICH bundle a pooled worker booted
// from (see the pool-key assertions in main.js).
FlowRuntime.parentPort.onmessage = (e) => {
  FlowRuntime.parentPort.postMessage({
    kind: "echo",
    tag: "one",
    payload: e.data.payload,
  });
};
console.log("eszip url worker booted (one)");
