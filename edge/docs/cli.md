# Flow CLI

Flow's CLI is Deno's CLI plus a handful of top-level flags, environment
variables, and the `eszip` subcommand group. Everything documented here is
flow-specific; all Deno flags and subcommands behave exactly as upstream.

## Flow flags

Flow flags are **top-level**: they are recognized anywhere on the command line
(before or after the subcommand) and are stripped out before Deno's own flag
parser runs, so they compose with any Deno subcommand:

```console
$ flow run --policy per_worker --allow-net main.ts
$ flow --max-parallelism 8 test worker_test.ts
```

Both `--flag value` and `--flag=value` forms work. Stripping stops at a bare
`--`: anything after it is passed to your script untouched, so a script can
receive an argument that happens to be named like a flow flag:

```console
$ flow run main.ts -- --policy this-goes-to-Deno.args
```

| Flag                                       | Value                                      | Default      | Meaning                                                                                                                  |
| ------------------------------------------ | ------------------------------------------ | ------------ | ------------------------------------------------------------------------------------------------------------------------ |
| `--policy`                                 | `per_worker` \| `per_request` \| `oneshot` | `per_worker` | Worker pool supervisor policy (see below)                                                                                |
| `--max-parallelism`                        | integer                                    | `4`          | Max concurrent workers **per service path** (a global cap of 32 workers also applies)                                    |
| `--request-wait-timeout`                   | milliseconds                               | `10000`      | How long a `create()` waits for a pool slot before failing                                                               |
| `--dispatch-beforeunload-cpu-ratio`        | `0`–`99`                                   | off          | Dispatch a `beforeunload` event in the worker when CPU usage reaches this % of its hard limit                            |
| `--dispatch-beforeunload-memory-ratio`     | `0`–`99`                                   | off          | …when memory usage reaches this % of `memoryLimitMb`                                                                     |
| `--dispatch-beforeunload-wall-clock-ratio` | `0`–`99`                                   | off          | …when age reaches this % of `workerTimeoutMs`                                                                            |
| `--user-worker-inspect`                    | `host:port`                                | off          | Serve a DevTools inspector for user workers on this address (see [user-workers.md](./user-workers.md#debugging-workers)) |

Parsing is lenient: an invalid value prints a `flow: ignoring invalid value…`
warning to stderr and the environment variable (or built-in default) applies
instead.

### Pool policies

- **`per_worker`** (default): one worker per `servicePath`, shared by every
  `create()` for that path. Subsequent `create()` calls return the running
  worker (`reused`) with a fresh port. Use `forceCreate: true` per call to opt
  out.
- **`per_request`**: workers are retired after serving; `create()` prefers fresh
  workers.
- **`oneshot`**: strictly one use per worker (`forceCreate` is implied).

## Environment variables

Environment variables form the base layer; the CLI flags above override them per
invocation.

| Variable                                   | Matching flag / effect                                                                                                                             |
| ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| `FLOW_WORKER_POOL_POLICY`                  | `--policy`                                                                                                                                         |
| `FLOW_WORKER_MAX_PARALLELISM`              | `--max-parallelism`                                                                                                                                |
| `FLOW_REQUEST_WAIT_TIMEOUT_MS`             | `--request-wait-timeout`                                                                                                                           |
| `FLOW_BEFOREUNLOAD_CPU_RATIO`              | `--dispatch-beforeunload-cpu-ratio`                                                                                                                |
| `FLOW_BEFOREUNLOAD_MEMORY_RATIO`           | `--dispatch-beforeunload-memory-ratio`                                                                                                             |
| `FLOW_BEFOREUNLOAD_WALL_CLOCK_RATIO`       | `--dispatch-beforeunload-wall-clock-ratio`                                                                                                         |
| `FLOW_USER_WORKER_INSPECTOR_ADDRESS`       | `--user-worker-inspect`                                                                                                                            |
| `FLOW_USER_WORKER_MAX_HEAP_SIZE_MIB`       | Default worker heap limit (MiB) when `create()` omits `memoryLimitMb`. Built-in default: 512                                                       |
| `FLOW_INCLUDE_MALLOCED_MEMORY_ON_MEMCHECK` | Include malloc'd (external) memory in the worker memory check (`1`/`true`/`yes`/`on`)                                                              |
| `FLOW_ESZIP_CHECKSUM`                      | Default for `flow eszip bundle --checksum`                                                                                                         |
| `FLOW_BUNDLE_CACHE_DIR`                    | Where `maybeEszip` bytes/streams are spilled to disk. Default: `<tmpdir>/flow-bundles` (see [user-workers.md](./user-workers.md#the-bundle-cache)) |
| `FLOW_BUNDLE_CACHE_TTL_SECS`               | Age after which bundle-cache entries are swept. Default: `604800` (7 days)                                                                         |
| `FLOW_BUNDLE_CACHE_MAX_SIZE`               | Soft bundle-cache size cap in bytes: admissions LRU-evict unpinned entries to fit under it. Default: unset (uncapped)                              |
| `DENO_NO_DEPRECATION_WARNINGS`             | Suppress deprecated-API warnings inside user workers                                                                                               |
| `DENO_VERBOSE_WARNINGS`                    | Verbose deprecated-API warnings inside user workers                                                                                                |

```console
$ FLOW_WORKER_POOL_POLICY=oneshot FLOW_USER_WORKER_MAX_HEAP_SIZE_MIB=256 \
    flow run -A main.ts
```

## `flow types`

`flow types` prints Deno's ambient TypeScript declarations **plus flow's own**
(`FlowRuntime`, the `create()` option interfaces, the `FlowRuntime.events` event
shapes, `FLOW_VERSION`, …), so the output is the complete, always-current typing
of the runtime:

```console
$ flow types > flow.d.ts
```

`flow check` and the flow LSP already see these declarations without the
generated file — generate it only for external tooling (a plain `tsc` build,
editors running a non-flow language server).

## `flow eszip` — deployment artifacts

An _eszip_ is a single binary artifact containing a module graph (all local and
remote modules of an entrypoint, plus npm packages, static assets, and
metadata), suitable for shipping a worker service as one file. A worker can be
booted directly from an eszip via `create()`'s `maybeEszip` option — the
artifact is then served **file-backed**: module sources are read from disk on
demand instead of holding the whole bundle in memory (see
[user-workers.md](./user-workers.md#booting-from-an-eszip-maybeeszip) for the
loading, caching, and integrity semantics).

Two compatibility notes:

- Only **current-format** eszips (flow version `2.0`) can boot workers. Archives
  produced by older flow/edge-runtime versions still unpack with
  `flow eszip unbundle`, so `unbundle` + `bundle` re-creates any old artifact in
  the current format.
- Bundling with `--checksum xxhash3` (or `sha256`) is recommended for artifacts
  that cross a network or shared storage: every module read at boot and import
  time is then verified against its stored hash, and corruption fails the worker
  with an `invalid source hash` boot error instead of running altered code.

The `eszip` group is listed in the `Flow:` section of `flow --help`.

### `flow eszip bundle`

Create an eszip from an entrypoint:

```console
$ flow eszip bundle --entrypoint ./service/index.ts --output service.eszip
```

| Flag                     | Default     | Meaning                                                                              |
| ------------------------ | ----------- | ------------------------------------------------------------------------------------ |
| `--entrypoint <Path>`    | (required)  | Entrypoint whose module graph is bundled                                             |
| `--output <DIR>`         | `bin.eszip` | Output file (`-` for stdout)                                                         |
| `--static <Path>`        | none        | Glob pattern of static files to include; repeatable                                  |
| `--exclude <PATTERN>`    | none        | Specifier or glob whose module subtree is left out of the bundle; repeatable (below) |
| `--checksum <KIND>`      | none        | Hash function for content checksums (env: `FLOW_ESZIP_CHECKSUM`)                     |
| `--disable-module-cache` | `false`     | Do not use the local module cache while building                                     |
| `--timeout <SECONDS>`    | none        | Abort the bundle if it takes longer than this                                        |

Import maps: the CLI applies the workspace configuration discovered for the
entrypoint (`deno.json` `imports`, `package.json`) — there is no flag for an
explicit map. To bundle with one, use the programmatic
[`FlowRuntime.bundle`](./user-workers.md#bundling-programmatically-flowruntimebundle--flowruntimeunbundle)
and its `importMapPath` option.

Include static assets alongside the code:

```console
$ flow eszip bundle --entrypoint ./svc/index.ts \
    --static "./svc/assets/**/*.html" --static "./svc/data/*.json"
```

#### `--exclude`: leaving modules out

`--exclude` prunes a module (or a whole subtree) from the artifact while keeping
the import statements that reference it, so the excluded modules are resolved at
boot time instead of being baked in:

```console
$ flow eszip bundle --entrypoint ./tenant/index.ts \
    --exclude "#services/shopify/mod.ts" --exclude "services/internal/**"
```

- A **specifier** (an authored import like `#services/shopify/mod.ts`, a path,
  or a `file://` URL) excludes that exact module; its dependency subtree is
  pruned only where reachable _solely_ through excluded modules.
- A **glob** (contains `*`, `?`, or `[`) is matched against each module's
  eszip-relative key and excludes every match, however it is imported.
- A dependency also reachable from a non-excluded module stays bundled. Patterns
  that match nothing are silently ignored.

A worker booted from such a bundle must be able to resolve the excluded imports:
today that requires creating it with `allowHostFsAccess: true` (the module
loader then falls back to the host filesystem); in a fully sandboxed worker the
excluded imports fail with `Module not found`.

### `flow eszip unbundle`

Extract an eszip back into files:

```console
$ flow eszip unbundle --eszip service.eszip --output ./extracted
```

| Flag             | Default    | Meaning                   |
| ---------------- | ---------- | ------------------------- |
| `--eszip <Path>` | (required) | The eszip to extract      |
| `--output <DIR>` | `./`       | Directory to extract into |
