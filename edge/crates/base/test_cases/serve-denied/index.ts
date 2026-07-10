// Serving HTTP inside a user worker is not supported: flow has no ingress
// into workers; comms run over FlowRuntime.parentPort.
function assertNotSupported(fn: () => unknown, what: string) {
  try {
    fn();
  } catch (e) {
    if (e.toString().includes("not supported")) {
      return;
    }
    throw new Error(`${what}: expected NotSupported, got: ${e}`);
  }
  throw new Error(`${what}: expected to throw`);
}

assertNotSupported(() => Deno.serve(() => new Response("meow")), "Deno.serve");
assertNotSupported(
  () => Deno.listen({ port: 9999 }),
  "Deno.listen",
);
assertNotSupported(
  () => Deno.upgradeWebSocket(new Request("http://localhost/")),
  "Deno.upgradeWebSocket",
);

console.log("serve-denied test passed");
