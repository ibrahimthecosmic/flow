# Flow documentation

Flow is a drop-in Deno replacement: the full Deno CLI plus an embedded,
sandboxed **user-worker** runtime (Supabase edge-runtime lineage). The main
isolate is plain Deno — every Deno subcommand, flag, and API works unchanged —
and on top of it flow adds:

- a **host API** (`FlowRuntime.userWorkers`) for spawning hardened,
  resource-limited worker isolates and talking to them over `MessagePort`s,
- a small set of **CLI flags / environment variables** tuning the worker pool,
- the **`flow eszip`** subcommand group for deployment artifacts,
- a per-worker **DevTools inspector**.

These documents cover only the flow layer. For everything else, use the regular
[Deno documentation](https://docs.deno.com/).

| Document                                 | Contents                                                                                                          |
| ---------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| [cli.md](./cli.md)                       | Flow CLI flags, environment variables, `flow types`, `flow eszip`                                                 |
| [user-workers.md](./user-workers.md)     | Host API: creating workers (all options), eszip boots, programmatic bundling, MessagePort comms, reuse, inspector |
| [worker-runtime.md](./worker-runtime.md) | Inside a user worker: APIs, sandbox behavior, Node/npm compat, lifecycle & limits                                 |

## Quick taste

```ts
// main.ts — runs in the (plain Deno) main isolate
const worker = await FlowRuntime.userWorkers.create({
  servicePath: "./service", // directory containing index.ts
});

worker.port.onmessage = (e) => console.log("worker said:", e.data);
worker.port.postMessage({ hello: "worker" });
```

```ts
// service/index.ts — runs in a sandboxed user worker
FlowRuntime.parentPort.onmessage = (e) => {
  FlowRuntime.parentPort.postMessage({ echo: e.data });
};
```

```console
$ flow run --allow-all main.ts
worker said: { echo: { hello: "worker" } }
```

## Architecture in one paragraph

Every `flow` invocation is a normal Deno process. When the main worker
bootstraps, flow installs the `FlowRuntime` global and stands up a user-worker
pool (no HTTP server — flow's ancestors served HTTP; flow exposes workers to
_your code_ instead). Each `create()` call asks the pool for a worker: each
worker runs on its own OS thread in its own V8 isolate with its own heap limit,
CPU accounting, wall-clock timeout, and a locked-down API surface. The main
isolate and each worker share a duplex `MessagePort` channel (structured clone +
zero-copy `ArrayBuffer` transfer).
