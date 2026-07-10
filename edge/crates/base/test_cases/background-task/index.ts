// FlowRuntime.waitUntil keeps the worker alive until the promise settles.
function sleep(ms: number): Promise<string> {
  return new Promise((res) => {
    setTimeout(() => {
      res("background task done");
    }, ms);
  });
}

FlowRuntime.waitUntil(sleep(1_000).then((msg) => console.log(msg)));

console.log("background-task main finished");
