// Worker side of the user_workers spec: answers RPC-style messages over the
// parent MessagePort and serves extra channels handed over on pool reuse.

function wire(port: MessagePort) {
  port.onmessage = (e) => {
    const msg = e.data;
    switch (msg.kind) {
      case "echo":
        port.postMessage({ kind: "echo", payload: msg.payload });
        break;

      case "context":
        port.postMessage({ kind: "context", context: FlowRuntime.context });
        break;

      case "env":
        port.postMessage({
          kind: "env",
          value: Deno.env.get("WORKER_SECRET") ?? null,
        });
        break;

      case "sandbox": {
        let serveDenied = false;
        try {
          Deno.serve(() => new Response("meow"));
        } catch (err) {
          serveDenied = String(err).includes("not supported");
        }
        Deno.exit(1); // must be a no-op in the sandbox
        port.postMessage({
          kind: "sandbox",
          serveDenied,
          exitIsNoop: true,
          argsLength: Deno.args.length,
        });
        break;
      }

      case "log":
        console.log(msg.payload);
        port.postMessage({ kind: "log" });
        break;

      case "versions":
        port.postMessage({
          kind: "versions",
          flow: globalThis.FLOW_VERSION,
          deno: globalThis.DENO_VERSION,
          reported: Deno.version.deno,
        });
        break;

      case "ports":
        port.postMessage({
          kind: "ports",
          count: FlowRuntime.parentPorts.length,
          firstIsParentPort:
            FlowRuntime.parentPorts[0] === FlowRuntime.parentPort,
        });
        break;

      case "connect":
        // Report whether outbound net was denied by the worker's permission
        // set (as opposed to failing for ordinary network reasons).
        (async () => {
          try {
            const conn = await Deno.connect({
              hostname: msg.hostname,
              port: msg.port,
            });
            conn.close();
            port.postMessage({ kind: "connect", denied: false });
          } catch (err) {
            const name = err instanceof Error ? err.name : "";
            port.postMessage({
              kind: "connect",
              denied: name === "NotCapable" || name === "PermissionDenied",
              error: String(err),
            });
          }
        })();
        break;

      case "terminate":
        // Reply first so the caller's rpc() resolves, then bow out.
        port.postMessage({ kind: "terminate" });
        FlowRuntime.scheduleTermination();
        break;

      case "bytes": {
        const buf = msg.buf as ArrayBuffer;
        const view = new Uint8Array(buf);
        for (let i = 0; i < view.length; i++) {
          view[i] = view[i] ^ 0xff;
        }
        port.postMessage({ kind: "bytes", buf }, [buf]);
        break;
      }

      default:
        port.postMessage({ kind: "error", error: `unknown kind: ${msg.kind}` });
    }
  };
}

wire(FlowRuntime.parentPort);

// Pool reuse: each later create() delivers an extra parent port.
FlowRuntime.onparentport = (port: MessagePort) => {
  wire(port);
};

console.log("worker booted");
