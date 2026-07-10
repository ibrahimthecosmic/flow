// Booted from an eszip by the surface_api spec: minimal parentPort echo.
FlowRuntime.parentPort.onmessage = (e) => {
  FlowRuntime.parentPort.postMessage({ kind: "echo", payload: e.data.payload });
};
console.log("eszip worker booted");
