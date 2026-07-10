globalThis.addEventListener("beforeunload", (ev) => {
  console.log(`triggered ${(ev as CustomEvent).detail?.reason}`);
});

// Pin the worker as busy so the half-time retire cannot early-drop it before
// the wall-clock beforeunload alert fires and is observed.
FlowRuntime.waitUntil(new Promise(() => {}));

await new Promise((res) => setTimeout(res, 60_000));
