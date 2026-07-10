// Blocks module evaluation on a long timer; used to exercise the wall-clock
// limit (the supervisor must kill the worker long before this resolves).
await new Promise((res) => setTimeout(res, 60_000));

console.log("sleep-top-level finished (should never be reached)");
