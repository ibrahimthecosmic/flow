// Pin the worker as busy so the soft-CPU alert cannot early-drop it before
// the beforeunload threshold is crossed and observed. waitUntil means "keep
// the worker alive until settled", so release the pin once beforeunload has
// fired - the host-side terminate in the test would otherwise wait for it.
let releasePin: () => void;
FlowRuntime.waitUntil(new Promise<void>((res) => (releasePin = res)));

globalThis.addEventListener("beforeunload", (ev) => {
  console.log(`triggered ${(ev as CustomEvent).detail?.reason}`);
  releasePin();
});

// Burn a bounded amount of CPU: past the beforeunload threshold (50% of the
// 4s hard limit = 2s) but safely under the hard limit itself.
function burnCpuMs(ms: number) {
  const start = Date.now();
  let result = 0;
  while (Date.now() - start < ms) {
    for (let i = 0; i < 100_000; i++) {
      result += Math.atan(i) * Math.tan(i);
    }
  }
  return result;
}

burnCpuMs(2_500);

// Idle so the event loop can deliver beforeunload.
await new Promise((res) => setTimeout(res, 5_000));
