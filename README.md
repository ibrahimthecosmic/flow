# Flow

Flow is a drop-in [Deno](https://deno.com) replacement: the full Deno CLI plus a
hardened user-worker layer derived from
[Supabase edge-runtime](https://github.com/supabase/edge-runtime). One binary
gives you everything Deno does — run, test, fmt, lint, compile, npm/JSR support
— and adds a supervised worker pool (`FlowRuntime.userWorkers`) for running
untrusted JavaScript/TypeScript with per-worker resource limits and permissions.

It's built on [V8](https://v8.dev/), [Rust](https://www.rust-lang.org/), and
[Tokio](https://tokio.rs/), tracking Deno upstream (currently Deno 2.9.0).

## Installation

Flow ships prebuilt binaries for Linux x86_64 (glibc and musl/Alpine).

Shell:

```sh
curl -fsSL https://raw.githubusercontent.com/ibrahimthecosmic/flow/main/install.sh | sh
```

Install a specific version:

```sh
curl -fsSL https://raw.githubusercontent.com/ibrahimthecosmic/flow/main/install.sh | sh -s v2.9.0
```

The binary lands in `~/.flow/bin/flow` (override the prefix with
`FLOW_INSTALL`). You can also grab the `flow-<target>.zip` assets directly from
the [releases page](https://github.com/ibrahimthecosmic/flow/releases).

### Upgrading

Flow updates itself in place from GitHub releases:

```sh
flow upgrade            # latest release
flow upgrade 2.9.1      # specific version
```

Flow only publishes stable releases — there are no canary, RC, or LTS channels.

### Build and install from source

See the
[contributing instructions](.github/CONTRIBUTING.md#building-from-source) for
prerequisites, then:

```sh
cargo build --release
```

The resulting binary is at `./target/release/flow`.

## Your first Flow program

Anything that runs on Deno runs on Flow. Create `server.ts`:

```ts
Deno.serve((_req: Request) => {
  return new Response("Hello, world!");
});
```

Run it:

```sh
flow run --allow-net server.ts
```

### User workers

What sets Flow apart is the supervised worker pool for running untrusted code
from the main isolate:

```ts
const worker = await FlowRuntime.userWorkers.create({
  servicePath: "./examples/serve",
  memoryLimitMb: 128,
  workerTimeoutMs: 30_000,
});
```

Workers communicate with the host exclusively over `MessagePort` channels (with
transferable `ArrayBuffer`s as the zero-copy byte path) and can mount S3- and
HTTP-backed virtual filesystems. See the docs below for the full surface.

## Documentation

- [Flow CLI flags and `eszip` subcommands](edge/docs/cli.md)
- [User workers: pool, policies, lifecycle](edge/docs/user-workers.md)
- [Worker runtime: `Flow` namespace, mounts, limits](edge/docs/worker-runtime.md)
- [HttpFS protocol](edge/docs/httpfs-protocol.md)
- [Deno runtime documentation](https://docs.deno.com/runtime/manual) — applies
  to all upstream behavior

## Contributing

We appreciate your help! To contribute, please read our
[contributing instructions](.github/CONTRIBUTING.md).
