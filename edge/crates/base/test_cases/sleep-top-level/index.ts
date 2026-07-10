// Blocks module evaluation on a long timer; used to exercise the wall-clock
// limit (the supervisor must kill the worker long before this resolves).
// The unresolved waitUntil pins the worker as busy: an idle worker is
// gracefully retired at half the wall clock (reason EarlyDrop), and the
// WallClockTime kill path this fixture exists for would never fire.
FlowRuntime.waitUntil(new Promise(() => {}));

await new Promise((res) => setTimeout(res, 60_000));

console.log("sleep-top-level finished (should never be reached)");
