globalThis.addEventListener("beforeunload", (ev) => {
  console.log(`triggered ${(ev as CustomEvent).detail?.reason}`);
});

// Pin the worker as busy so the memory-pressure alert cannot early-drop it
// before the beforeunload threshold is crossed and observed.
FlowRuntime.waitUntil(new Promise(() => {}));

const arr = [];
while (true) {
  arr.push(new Uint8Array(1024 * 1024));
  // yield so the event loop can deliver beforeunload
  await new Promise((res) => setTimeout(res, 10));
}
