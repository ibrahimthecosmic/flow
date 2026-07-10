// For CPU time regulation testing only (top-level synchronous mode)

// Pin the worker as busy: with no pending tasks the soft-CPU alert
// early-drops the worker (reason EarlyDrop) before the hard limit can
// deliver the CPUTime kill this fixture exists to trigger.
FlowRuntime.waitUntil(new Promise(() => {}));

function mySlowFunction(baseNumber) {
  const iterations = Math.pow(baseNumber, 7);
  let result = 0;
  for (var i = iterations; i >= 0; i--) {
    result += Math.atan(i) * Math.tan(i);
  }
  return result;
}

mySlowFunction(19);

console.log("cpu-sync finished");
