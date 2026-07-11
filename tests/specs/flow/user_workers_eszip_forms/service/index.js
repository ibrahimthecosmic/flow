// Minimal echo worker booted from an eszip by the eszip-forms spec.
FlowRuntime.parentPort.onmessage = (e) => {
  FlowRuntime.parentPort.postMessage({ kind: "echo", payload: e.data.payload });
};
console.log("eszip forms worker booted");
